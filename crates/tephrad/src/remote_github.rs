//! GitHub remote sources (PR-5): `source.add_github`, the first index, and
//! what `source.list` says about a remote source.
//!
//! A remote source is added in two steps. The first runs inside the call:
//! the checks that touch no network (name, one `owner/repo`, a token), then
//! the repository, the branch and its head commit, and the whole tree at
//! THAT commit ([`tephra_remote::github::full_tree`]). It is stored, every
//! file in state `remote`, and the call returns: the listing is there
//! within seconds. A refusal at any point changes nothing and announces
//! nothing.
//!
//! The second step is the first index, in the background on the registry's
//! runtime. Each text file is read ONCE through the API by its blob sha
//! (a sha names its bytes, so a branch that moves meanwhile changes
//! nothing), parsed by the same code a local file goes through
//! ([`crate::scan::write_remote_unit`]) and the bytes are dropped: nothing
//! is written to disk (F1, F12). A symlink or a submodule is listed by name
//! and never followed (F11), a file over [`MAX_INDEXED_BYTES`] or with a
//! known binary extension is never read, and a Git LFS pointer is kept by
//! name and its object never fetched (F11). Each file is written in ONE
//! transaction, so a file has its content hash only when all its rows are
//! there. An index cut short by a kill is read again from scratch at the
//! next start (nothing it read is trusted). One cut short by a network
//! failure while the daemon runs is finished from what has no content hash
//! yet by the source's poller (the source shows `offline` meanwhile).
//!
//! The indexer holds no `Arc<SourceRegistry>` (the registry owns the runtime
//! it runs on, and the last drop of the registry must not happen on one of
//! that runtime's workers): only the cache path, the writer gate, the event
//! sender and the hub.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tephra_cache::{
    encode_cursor, Cache, NewRemoteItem, NewRemoteSource, RemoteSourceRow, ITEM_KIND_SUBMODULE,
    ITEM_KIND_SYMLINK, REASON_LFS_POINTER, STATUS_ACTIVE, STATUS_AUTH_REQUIRED, STATUS_INDEXING,
    STATUS_OFFLINE,
};
use tephra_remote::credentials::{credential_state, CredentialState, CredentialStore, Provider};
use tephra_remote::drive::{DriveAdapter, DriveClient};
use tephra_remote::github::{
    full_tree, GithubAdapter, GithubClient, RepoRef, TreeEntry, TreeEntryKind,
};
use tephra_remote::{AdapterOptions, Clock, Conditional, RemoteError};

use crate::file_kind::{kind_from_name, MAX_INDEXED_BYTES};
use crate::registry::{
    check_source_name, retry_cache_on_busy, DaemonEvent, RegistryError, SourceChange,
    SourceChangeKind, SourceRegistry,
};
use crate::writer_gate::WriterGate;

/// The `kind` of a GitHub source in `remote_sources` and on the wire.
pub const GITHUB_KIND: &str = "github";
/// The `kind` of a Google Drive source in `remote_sources`.
pub const DRIVE_KIND: &str = "drive";

/// Git's mode for a symlink: listed by name, never followed.
const MODE_SYMLINK: &str = "120000";

/// The first line of a Git LFS pointer file.
const LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// An LFS pointer is a few lines; a file past this is not one.
const LFS_POINTER_MAX_BYTES: usize = 1024;

/// The longest the indexer sleeps at one time while a rate limit holds, so
/// a shutdown is noticed.
const RATE_LIMIT_SLICE: Duration = Duration::from_secs(1);

/// Tries a file's read gets for a failure that may pass (a dropped
/// connection, a 5xx) before the pass stops and the source is `offline`.
const READ_TRIES: u32 = 3;

/// Why `source.add_github` refused or failed. A refusal changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAddError {
    /// Refused, with the stable `reason` the wire carries and a plain
    /// message for a person. Never holds a token.
    Refused {
        reason: &'static str,
        message: String,
    },
    /// Failed for a reason that is not the caller's (the network, the
    /// cache).
    Failed(String),
}

impl std::fmt::Display for RemoteAddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteAddError::Refused { message, .. } | RemoteAddError::Failed(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for RemoteAddError {}

fn refused(reason: &'static str, message: impl Into<String>) -> RegistryError {
    RegistryError::Remote(RemoteAddError::Refused {
        reason,
        message: message.into(),
    })
}

/// What a GitHub source needs from the daemon: the adapter (which reads the
/// token from `store` on every call), the clock, and where the remote
/// content cache would live.
pub struct RemoteHub {
    pub(crate) adapter: Arc<dyn GithubAdapter>,
    /// The Drive adapter `unit.fetch` reads a Drive file through (it reads
    /// the Google token from `store` on every call). It lists nothing on
    /// its own: Drive sources are added and synced by PR-10.
    pub(crate) drive: Arc<dyn DriveAdapter>,
    store: Arc<dyn CredentialStore>,
    options: AdapterOptions,
    pub(crate) clock: Arc<dyn Clock>,
    data_dir: PathBuf,
    /// How often a waiting poller looks at the clock. Real time, whatever
    /// the clock is: a poll is due when the CLOCK says so (a test moves a
    /// fake clock), and this only bounds how late it notices.
    pub(crate) poll_tick: Duration,
    /// The sources that have a poller: one each, however often it is asked
    /// for.
    pub(crate) polling: Mutex<HashSet<String>>,
    /// The sources with an index job reading right now: one each. A job
    /// that finds its source here does nothing.
    indexing: Mutex<HashSet<String>>,
    /// How many polls the pollers have finished (a poll that only found the
    /// first index still reading is not one). A test waits on it to know a
    /// poller has acted when nothing else shows it (a network that is away).
    pub(crate) polls_done: std::sync::atomic::AtomicUsize,
    /// The downloads of `unit.fetch` running now, and the disk they write to.
    pub(crate) fetches: Arc<crate::remote::fetch::FetchEngine>,
}

impl RemoteHub {
    /// `github_api_url` is the config key (`None`: the real API).
    pub fn new(
        store: Arc<dyn CredentialStore>,
        github_api_url: Option<&str>,
        clock: Arc<dyn Clock>,
        data_dir: PathBuf,
    ) -> Result<Self, String> {
        Self::with_options(
            store,
            github_api_url,
            clock,
            data_dir,
            AdapterOptions::default(),
        )
    }

    /// [`RemoteHub::new`] with the adapter's timeouts given (a test makes
    /// them short).
    pub fn with_options(
        store: Arc<dyn CredentialStore>,
        github_api_url: Option<&str>,
        clock: Arc<dyn Clock>,
        data_dir: PathBuf,
        options: AdapterOptions,
    ) -> Result<Self, String> {
        let client =
            GithubClient::with_options(github_api_url, store.clone(), clock.clone(), options)
                .map_err(|e| e.to_string())?;
        let drive = DriveClient::with_options(None, store.clone(), clock.clone(), options)
            .map_err(|e| e.to_string())?;
        Ok(RemoteHub {
            adapter: Arc::new(client),
            drive: Arc::new(drive),
            store,
            options,
            clock,
            data_dir,
            poll_tick: crate::remote::poll::DEFAULT_POLL_TICK,
            polling: Mutex::new(HashSet::new()),
            indexing: Mutex::new(HashSet::new()),
            polls_done: std::sync::atomic::AtomicUsize::new(0),
            fetches: Arc::new(crate::remote::fetch::FetchEngine::new(Arc::new(
                crate::remote::fetch::RealDisk,
            ))),
        })
    }

    /// Reads Drive files from `drive_api_url` (the config key; `None`: the
    /// real API) instead of the default.
    pub fn with_drive_api_url(mut self, drive_api_url: Option<&str>) -> Result<Self, String> {
        let client = DriveClient::with_options(
            drive_api_url,
            self.store.clone(),
            self.clock.clone(),
            self.options,
        )
        .map_err(|e| e.to_string())?;
        self.drive = Arc::new(client);
        Ok(self)
    }

    /// The disk a fetch writes to (a test makes it fail the way a full
    /// disk does).
    pub fn with_disk(mut self, disk: Arc<dyn crate::remote::fetch::Disk>) -> Self {
        self.fetches = Arc::new(crate::remote::fetch::FetchEngine::new(disk));
        self
    }

    /// How often a waiting poller looks at the clock (default one second).
    pub fn with_poll_tick(mut self, tick: Duration) -> Self {
        self.poll_tick = tick;
        self
    }

    /// The directory the content cache of `source` would use. Nothing here
    /// creates it.
    pub fn cache_dir(&self, source: &str) -> PathBuf {
        self.data_dir.join("remote").join(source)
    }
}

/// What `source.add_github` answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RemoteAddWire {
    pub name: String,
    pub kind: &'static str,
    pub repo: String,
    pub branch: String,
    /// The commit the listing was taken at; empty for a repository with no
    /// commit.
    pub commit: String,
    /// Files in the listing (symlinks and submodules included).
    pub files: usize,
    pub status: String,
    pub index_done: usize,
    pub index_total: usize,
}

/// The remote part of one `source.list` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteListing {
    pub kind: String,
    pub repo: String,
    pub branch: Option<String>,
    pub cache: bool,
    pub cache_bytes: u64,
    pub git_bytes: u64,
    pub status: String,
    pub status_reason: Option<String>,
    pub index_done: usize,
    pub index_total: usize,
    pub last_sync_at: Option<i64>,
}

pub(crate) fn now_secs(clock: &dyn Clock) -> i64 {
    clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The bytes of the regular files under `dir`, never following a symlink;
/// 0 when it is not there.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// A repo spec that names exactly one repository: `owner/repo`.
fn parse_one_repo(text: &str) -> Result<RepoRef, RegistryError> {
    let pick_one = || {
        refused(
            "pick_one_repo",
            format!(
                "\"{}\" is not one repository: give owner/repo (pick one repo; a whole account or \
                 a list of repos cannot be added)",
                printable(text)
            ),
        )
    };
    if text.contains(['*', ':', ' ', '\\', '?', '#']) || text.matches('/').count() != 1 {
        return Err(pick_one());
    }
    RepoRef::parse(text).map_err(|_| pick_one())
}

/// `text` with control characters shown as `?`, for a message.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The refusal for an adapter error met while checking the repository or
/// the branch. The messages say what the person can do; none holds a token
/// (an adapter error never carries one).
fn refuse_remote(e: RemoteError, repo: &RepoRef, branch: Option<&str>) -> RegistryError {
    let full = repo.full_name();
    match e {
        RemoteError::AuthRejected => refused(
            "auth_required",
            "GitHub rejected the token (it may be revoked or expired): run `tephractl auth github` and try again",
        ),
        RemoteError::PermissionDenied => refused(
            "permission_denied",
            format!("the token is not allowed to read {full}"),
        ),
        RemoteError::NotFound => match branch {
            Some(b) => refused(
                "unknown_branch",
                format!("{full} has no branch \"{}\"", printable(b)),
            ),
            None => refused(
                "not_found",
                format!(
                    "{full} was not found: GitHub answers the same when the repository does not \
                     exist and when the token has no access to it"
                ),
            ),
        },
        RemoteError::InvalidInput(_) => refused(
            "unknown_branch",
            format!("{full} has no branch \"{}\"", printable(branch.unwrap_or(""))),
        ),
        RemoteError::Unexpected { status: 422 } if branch.is_some() => refused(
            "unknown_branch",
            format!(
                "{full} has no branch \"{}\"",
                printable(branch.unwrap_or(""))
            ),
        ),
        RemoteError::Moved { location } => refused(
            "repo_moved",
            format!(
                "{full} has moved to {}: add it under its new name",
                printable(&location)
            ),
        ),
        RemoteError::RateLimited { .. } => refused(
            "rate_limited",
            "GitHub is rate limiting this token: try again in a few minutes",
        ),
        other => RegistryError::Remote(RemoteAddError::Failed(format!(
            "could not reach GitHub: {other}"
        ))),
    }
}

/// One file of a repository's tree, as the index lists it: a symlink and a
/// submodule by name only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeItem {
    pub path: String,
    pub kind: &'static str,
    pub sha: String,
    pub size: Option<i64>,
}

impl TreeItem {
    pub(crate) fn as_new(&self) -> NewRemoteItem<'_> {
        NewRemoteItem {
            path: &self.path,
            kind: self.kind,
            sha: &self.sha,
            size: self.size,
        }
    }
}

/// The files of a tree (directories are not items; an empty path or one
/// with a NUL is skipped).
pub(crate) fn tree_items(tree: &[TreeEntry]) -> Vec<TreeItem> {
    let mut items = Vec::with_capacity(tree.len());
    for entry in tree {
        if entry.path.is_empty() || entry.path.contains('\0') {
            continue;
        }
        let name_part = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        let (kind, size) = match entry.kind {
            TreeEntryKind::Tree => continue,
            TreeEntryKind::Commit => (ITEM_KIND_SUBMODULE, None),
            TreeEntryKind::Blob if entry.mode == MODE_SYMLINK => {
                (ITEM_KIND_SYMLINK, entry.size.map(|s| s as i64))
            }
            TreeEntryKind::Blob => (
                kind_from_name(name_part).as_str(),
                entry.size.map(|s| s as i64),
            ),
        };
        items.push(TreeItem {
            path: entry.path.clone(),
            kind,
            sha: entry.sha.clone(),
            size,
        });
    }
    items
}

impl SourceRegistry {
    /// Gives the registry what it needs to add and index remote sources.
    /// Set once; a second call is ignored.
    pub fn set_remote_hub(&self, hub: Arc<RemoteHub>) {
        let _ = self.remote.set(hub);
    }

    /// How many polls the remote pollers have finished since the daemon
    /// started (0 with no remote hub). See [`RemoteHub::polls_done`].
    pub fn remote_polls_done(&self) -> usize {
        self.remote.get().map_or(0, |h| {
            h.polls_done.load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    /// `source.add_github`: registers ONE repository on ONE branch and takes
    /// its listing at one commit (see the module doc). Blocking: call it
    /// from a blocking thread.
    ///
    /// `branch` is the repository's default branch when `None`.
    pub fn register_github(
        &self,
        name: &str,
        repo_text: &str,
        branch: Option<&str>,
    ) -> Result<RemoteAddWire, RegistryError> {
        // 1. What needs no network.
        check_source_name(name).map_err(|why| {
            refused(
                "invalid_name",
                format!("\"{}\" is not a usable source name: {why}", printable(name)),
            )
        })?;
        let repo = parse_one_repo(repo_text)?;
        if let Some(b) = branch {
            if b.is_empty() || b.chars().any(|c| c.is_control()) {
                return Err(refused(
                    "unknown_branch",
                    "the branch name is empty or unusable",
                ));
            }
        }
        let lowered = name.to_lowercase();
        {
            let cache = self.cache.lock().unwrap();
            if cache
                .list_sources()?
                .iter()
                .any(|r| r.name.to_lowercase() == lowered)
            {
                return Err(refused(
                    "name_taken",
                    format!("a source named \"{name}\" (or differing only in case) already exists"),
                ));
            }
        }
        let Some(hub) = self.remote.get().cloned() else {
            return Err(refused(
                "auth_required",
                "this daemon has no credential store, so no GitHub source can be added",
            ));
        };
        match credential_state(&*hub.store, Provider::Github) {
            CredentialState::SignedIn => {}
            CredentialState::SignedOut => {
                return Err(refused(
                    "auth_required",
                    "no GitHub token is stored: run `tephractl auth github` first",
                ))
            }
            CredentialState::AuthRequired(reason) => {
                return Err(refused(
                    "auth_required",
                    format!("the GitHub token cannot be read: {reason}"),
                ))
            }
        }

        // 2. The repository, the branch and the tree at one commit.
        let adapter = hub.adapter.clone();
        let (branch_name, commit, etag, tree) = self.runtime.block_on(async {
            let info = adapter
                .get_repo(&repo)
                .await
                .map_err(|e| refuse_remote(e, &repo, None))?;
            let branch_name = branch.map_or(info.default_branch, str::to_string);
            let head = match adapter.poll_ref(&repo, &branch_name, None).await {
                Ok(Conditional::Modified(head)) => head,
                Ok(Conditional::NotModified) => {
                    return Err(RegistryError::Remote(RemoteAddError::Failed(
                        "GitHub answered 'not modified' to an unconditional request".into(),
                    )))
                }
                // A repository with no commit: 0 files, nothing to index.
                Err(RemoteError::Unexpected { status: 409 }) => {
                    return Ok((branch_name, String::new(), None, Vec::new()))
                }
                Err(e) => return Err(refuse_remote(e, &repo, Some(&branch_name))),
            };
            let tree = full_tree(&*adapter, &repo, &head.sha)
                .await
                .map_err(|e| refuse_remote(e, &repo, Some(&branch_name)))?;
            Ok::<_, RegistryError>((branch_name, head.sha, head.etag, tree.entries))
        })?;

        // 3. The listing, as rows: a symlink and a submodule by name only.
        let listed = tree_items(&tree);
        let items: Vec<NewRemoteItem<'_>> = listed.iter().map(TreeItem::as_new).collect();
        let files = items.len();
        {
            let cache = self.cache.lock().unwrap();
            let new = NewRemoteSource {
                name,
                kind: GITHUB_KIND,
                target: &repo.full_name(),
                branch: Some(&branch_name),
                cursor: &encode_cursor(&commit, etag.as_deref()),
                now: now_secs(&*hub.clock),
            };
            retry_cache_on_busy(&self.gate, || {
                cache.add_remote_source(&new, &items, MAX_INDEXED_BYTES)
            })?;
        }
        tracing::info!(source = %name, repo = %repo.full_name(), branch = %branch_name, files, "remote source added");

        // 4. Tell the clients, then read the text in the background.
        self.announce_sources_changed(vec![SourceChange::new(SourceChangeKind::Added, name)]);
        self.spawn_remote_index(name);
        self.spawn_remote_poll(name);
        let progress = {
            let cache = self.cache.lock().unwrap();
            cache.remote_index_progress(name, MAX_INDEXED_BYTES)?
        };
        Ok(RemoteAddWire {
            name: name.to_string(),
            kind: GITHUB_KIND,
            repo: repo.full_name(),
            branch: branch_name,
            commit,
            files,
            status: STATUS_INDEXING.to_string(),
            index_done: progress.done,
            index_total: progress.total,
        })
    }

    /// Starts the background first index of the remote source `name`, if
    /// the registry has a hub. One run per call: a caller starts it once
    /// per source per start.
    pub fn spawn_remote_index(&self, name: &str) {
        let Some(hub) = self.remote.get().cloned() else {
            tracing::warn!(source = %name, "no remote hub: the source is listed but not indexed");
            return;
        };
        let job = IndexJob::new(
            name,
            self.cache_path.clone(),
            self.gate.clone(),
            self.events_tx.clone(),
            hub,
        );
        self.runtime.spawn(job.run());
    }

    /// Start-up for the remote sources: makes again the rows a schema
    /// rebuild dropped (from `remote_items`, with no remote call), then
    /// indexes again from scratch every source whose first index was cut
    /// short or whose cursor row is lost. A source whose index is complete
    /// costs no adapter call.
    ///
    /// No download survives a restart: an item that says `downloading` is
    /// `remote` again and the temporary files of a fetch that was cut off
    /// are swept ([`SourceRegistry::clean_up_after_downloads`]).
    pub fn resume_remote_sources(&self) {
        if let Some(hub) = self.remote.get() {
            self.clean_up_after_downloads(hub);
        }
        let rows = {
            let cache = self.cache.lock().unwrap();
            match cache.remote_sources() {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list the remote sources at start");
                    return;
                }
            }
        };
        for row in rows {
            // A source whose cursor row is lost, or whose first index was
            // cut short (a kill), is read again from scratch: nothing it
            // has read is trusted to be current.
            let from_scratch = {
                let cache = self.cache.lock().unwrap();
                match cache.remote_cursor(&row.name) {
                    Ok(Some(c)) => (!c.index_complete).then_some("the first index was cut short"),
                    Ok(None) => Some("the cursor row was lost"),
                    Err(_) => None,
                }
            };
            if let Some(why) = from_scratch {
                let now = self.remote.get().map_or(0, |h| now_secs(&*h.clock));
                let reset = {
                    let cache = self.cache.lock().unwrap();
                    retry_cache_on_busy(&self.gate, || {
                        cache.reset_remote_index(&row.name, now, MAX_INDEXED_BYTES)
                    })
                };
                match reset {
                    Ok(_) => {
                        tracing::info!(source = %row.name, "{why}; the source is indexed again from scratch");
                        self.spawn_remote_index(&row.name);
                        self.spawn_remote_poll(&row.name);
                    }
                    Err(e) => {
                        tracing::warn!(source = %row.name, error = %e, "could not reset a remote source that must be indexed again");
                    }
                }
                continue;
            }
            let restored = {
                let cache = self.cache.lock().unwrap();
                retry_cache_on_busy(&self.gate, || {
                    cache.restore_remote_units(&row.name, MAX_INDEXED_BYTES)
                })
            };
            match restored {
                Ok(true) => {
                    tracing::info!(source = %row.name, "remote source rows made again from its items")
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(source = %row.name, error = %e, "could not restore a remote source");
                    continue;
                }
            }
            self.spawn_remote_poll(&row.name);
        }
    }

    /// The remote part of the `source.list` row of `row`, from the cache
    /// alone. `None` for a local source.
    pub(crate) fn remote_listing_of(
        &self,
        cache: &Cache,
        source: &RemoteSourceRow,
    ) -> Option<RemoteListing> {
        let progress = cache
            .remote_index_progress(&source.name, MAX_INDEXED_BYTES)
            .ok()?;
        let dir = self
            .remote
            .get()
            .map(|hub| hub.cache_dir(&source.name))
            .unwrap_or_default();
        Some(RemoteListing {
            kind: source.kind.clone(),
            repo: source.target.clone(),
            branch: source.branch.clone(),
            cache: source.cache,
            cache_bytes: cache.remote_cache_bytes(&source.name).unwrap_or(0),
            git_bytes: if dir.as_os_str().is_empty() {
                0
            } else {
                dir_bytes(&dir.join(".git"))
            },
            status: source.status.clone(),
            status_reason: source.status_reason.clone(),
            index_done: progress.done,
            index_total: progress.total,
            last_sync_at: source.last_sync_at,
        })
    }
}

/// One background first index (and the catch-up of a poll that left files
/// unread).
pub(crate) struct IndexJob {
    name: String,
    cache_path: PathBuf,
    /// The job's one connection, opened on first use (a connection per
    /// file would cost more than the file's rows).
    conn: Arc<std::sync::Mutex<Option<Cache>>>,
    /// A reason is set on the source (a rate-limit wait): the next
    /// successful read clears it, and only then is there a write to do.
    reason_set: std::sync::atomic::AtomicBool,
    gate: Arc<WriterGate>,
    events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
    hub: Arc<RemoteHub>,
}

/// How a pass over the pending files ended.
enum PassEnd {
    Done,
    /// Stopped (the failure is recorded on the source). Another run, from
    /// the source's poller or the next start, reads what is left.
    Stopped,
}

/// What one [`IndexJob::run`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// Another job was already reading this source: nothing was done.
    Skipped,
    /// Every pending file was read; the source is `active`.
    Finished,
    /// Stopped before the end (offline, a rejected token, a failure).
    Stopped,
}

/// Holds a source's place among the running index jobs.
struct IndexingSlot {
    hub: Arc<RemoteHub>,
    name: String,
}

impl Drop for IndexingSlot {
    fn drop(&mut self) {
        self.hub
            .indexing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.name);
    }
}

impl IndexJob {
    pub(crate) fn new(
        name: &str,
        cache_path: PathBuf,
        gate: Arc<WriterGate>,
        events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
        hub: Arc<RemoteHub>,
    ) -> Self {
        IndexJob {
            name: name.to_string(),
            cache_path,
            conn: Arc::new(std::sync::Mutex::new(None)),
            reason_set: std::sync::atomic::AtomicBool::new(false),
            gate,
            events_tx,
            hub,
        }
    }

    /// Runs the whole first index, unless another job is reading this
    /// source. Never panics the runtime: every failure is recorded on the
    /// source and logged.
    pub(crate) async fn run(self) -> RunEnd {
        let name = self.name.clone();
        let first = self
            .hub
            .indexing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.clone());
        if !first {
            return RunEnd::Skipped;
        }
        let _slot = IndexingSlot {
            hub: self.hub.clone(),
            name: name.clone(),
        };
        match self.index().await {
            Ok(PassEnd::Done) => {
                tracing::info!(source = %name, "first index finished");
                RunEnd::Finished
            }
            Ok(PassEnd::Stopped) => {
                tracing::warn!(source = %name, "first index stopped; the source's poller tries again");
                RunEnd::Stopped
            }
            Err(e) => {
                tracing::warn!(source = %name, error = %e, "first index failed");
                let _ = self
                    .db(move |c, name| c.set_remote_status(name, STATUS_INDEXING, Some(&e), None))
                    .await;
                RunEnd::Stopped
            }
        }
    }

    /// Runs `f` with a fresh cache connection on the blocking pool, under
    /// the writer gate (retried on a busy database).
    async fn db<T: Send + 'static>(
        &self,
        f: impl Fn(&Cache, &str) -> Result<T, tephra_cache::CacheError> + Send + 'static,
    ) -> Result<T, String> {
        let path = self.cache_path.clone();
        let gate = self.gate.clone();
        let name = self.name.clone();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut slot = conn.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(Cache::open(&path).map_err(|e| e.to_string())?);
            }
            let cache = slot.as_ref().expect("opened above");
            retry_cache_on_busy(&gate, || f(cache, &name)).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn announce(&self) {
        let _ = self.events_tx.send(DaemonEvent::SourceChanged {
            source: self.name.clone(),
            paths: Vec::new(),
            fixes: Vec::new(),
        });
    }

    async fn index(&self) -> Result<PassEnd, String> {
        let source = self
            .db(|c, n| c.remote_source(n))
            .await?
            .ok_or_else(|| "the source is gone".to_string())?;
        let repo = RepoRef::parse(&source.target).map_err(|e| e.to_string())?;
        // A run after a stop starts on a source that says `offline` or
        // `auth_required`: its first good read puts `indexing` back.
        if source.status == STATUS_OFFLINE || source.status == STATUS_AUTH_REQUIRED {
            self.reason_set
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let pending = self
            .db(|c, n| c.remote_pending_items(n, MAX_INDEXED_BYTES))
            .await?;
        let mut since_announce = 0usize;
        for item in pending {
            let Some(sha) = item.sha.clone() else {
                continue;
            };
            let bytes = match self.read_blob(&repo, &sha).await {
                Ok(bytes) => bytes,
                Err(ReadStop::Gone) => {
                    let path = item.path.clone();
                    self.db(move |c, n| c.set_remote_item_reason(n, &path, Some("not_found")))
                        .await?;
                    continue;
                }
                Err(ReadStop::AuthRejected) => {
                    self.stop_with(
                        &source,
                        STATUS_AUTH_REQUIRED,
                        "GitHub rejected the token: run `tephractl auth github`".to_string(),
                    )
                    .await?;
                    return Ok(PassEnd::Stopped);
                }
                // The remote cannot be reached: the source is `offline`, as
                // a failed poll makes it, and the poller reads again.
                Err(ReadStop::Offline(why)) => {
                    self.stop_with(&source, STATUS_OFFLINE, why).await?;
                    return Ok(PassEnd::Stopped);
                }
                Err(ReadStop::Failed(why)) => {
                    self.db(move |c, n| c.set_remote_status(n, STATUS_INDEXING, Some(&why), None))
                        .await?;
                    return Ok(PassEnd::Stopped);
                }
                Err(ReadStop::Shutdown) => return Ok(PassEnd::Stopped),
            };
            let path = item.path.clone();
            // The bytes are parsed on the blocking pool, written, and
            // dropped there.
            self.write_one(path, bytes).await?;
            since_announce += 1;
            if since_announce >= 200 {
                since_announce = 0;
                self.announce();
            }
        }
        let now = now_secs(&*self.hub.clock);
        self.db(move |c, n| {
            c.set_remote_index_complete(n, true, now)?;
            c.set_remote_status(n, STATUS_ACTIVE, None, Some(now))
        })
        .await?;
        self.announce();
        Ok(PassEnd::Done)
    }

    /// Records why the pass stops (the status and its reason) and tells the
    /// clients when that is news. Nothing is written for a status the source
    /// already has: a poller that tries again every interval while the
    /// remote stays away leaves the index and the row alone.
    async fn stop_with(
        &self,
        source: &RemoteSourceRow,
        status: &'static str,
        reason: String,
    ) -> Result<(), String> {
        let unchanged =
            source.status == status && source.status_reason.as_deref() == Some(reason.as_str());
        if unchanged {
            return Ok(());
        }
        self.db(move |c, n| c.set_remote_status(n, status, Some(&reason), None))
            .await?;
        self.announce();
        Ok(())
    }

    /// Writes the rows of one file from its bytes, or notes it is an LFS
    /// pointer and keeps it by name. The file's rows go in one transaction:
    /// a reader sees the file as it was or whole, and a kill leaves no unit
    /// with a content hash and part of its rows.
    async fn write_one(&self, path: String, bytes: Vec<u8>) -> Result<(), String> {
        // The bytes move into the blocking task, are parsed there and
        // dropped there.
        self.db(move |cache, name| {
            if is_lfs_pointer(&bytes) {
                return cache.set_remote_item_reason(name, &path, Some(REASON_LFS_POINTER));
            }
            let Some(row) = cache.get_source(name)? else {
                return Ok(());
            };
            let tx = cache.connection().unchecked_transaction()?;
            crate::scan::write_remote_unit(cache, row.id, &path, &bytes)
                .map_err(crate::scan::cache_error_of)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// One blob, with the waits a rate limit asks for and a few tries for a
    /// failure that may pass. A rate limit loses nothing and marks nothing
    /// broken.
    async fn read_blob(&self, repo: &RepoRef, sha: &str) -> Result<Vec<u8>, ReadStop> {
        let mut tries = 0;
        loop {
            match self.hub.adapter.get_blob(repo, sha).await {
                Ok(bytes) => {
                    if self
                        .reason_set
                        .swap(false, std::sync::atomic::Ordering::Relaxed)
                    {
                        let _ = self
                            .db(|c, n| c.set_remote_status(n, STATUS_INDEXING, None, None))
                            .await;
                    }
                    return Ok(bytes);
                }
                Err(RemoteError::RateLimited { retry_at }) => {
                    self.wait_out(retry_at).await?;
                }
                Err(RemoteError::NotFound) => return Err(ReadStop::Gone),
                Err(RemoteError::AuthRejected) => return Err(ReadStop::AuthRejected),
                Err(e @ (RemoteError::Unreachable(_) | RemoteError::Server { .. })) => {
                    tries += 1;
                    if tries >= READ_TRIES {
                        return Err(ReadStop::Offline(crate::remote::poll::offline_reason(&e)));
                    }
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(tries))).await;
                }
                Err(e) => return Err(ReadStop::Failed(format!("could not read from GitHub: {e}"))),
            }
        }
    }

    /// Sleeps until `retry_at`, noting it on the source (status stays
    /// `indexing`).
    async fn wait_out(&self, retry_at: SystemTime) -> Result<(), ReadStop> {
        let secs = retry_at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let reason = format!(
            "rate limited by GitHub; reading resumes at {}",
            hhmm_utc(secs)
        );
        self.reason_set
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .db(move |c, n| c.set_remote_status(n, STATUS_INDEXING, Some(&reason), None))
            .await;
        loop {
            let left = retry_at
                .duration_since(self.hub.clock.now())
                .unwrap_or_default();
            if left.is_zero() {
                return Ok(());
            }
            if self.source_gone().await {
                return Err(ReadStop::Shutdown);
            }
            tokio::time::sleep(left.min(RATE_LIMIT_SLICE)).await;
        }
    }

    async fn source_gone(&self) -> bool {
        matches!(self.db(|c, n| c.remote_source(n)).await, Ok(None))
    }
}

enum ReadStop {
    /// The blob is no longer there.
    Gone,
    AuthRejected,
    /// GitHub cannot be reached (a connect error, a timeout, a 5xx), with
    /// the reason the source shows.
    Offline(String),
    Failed(String),
    Shutdown,
}

pub(crate) fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes.len() <= LFS_POINTER_MAX_BYTES && bytes.starts_with(LFS_POINTER_PREFIX)
}

/// `HH:MM` (UTC) of a Unix time in seconds.
pub(crate) fn hhmm_utc(secs: u64) -> String {
    let day = secs % 86_400;
    format!("{:02}:{:02} UTC", day / 3600, (day % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_repo_means_exactly_owner_slash_repo() {
        assert!(parse_one_repo("danrfletcher/atlas").is_ok());
        assert!(parse_one_repo("a.b/c_d-e").is_ok());
        for bad in [
            "danrfletcher",
            "danrfletcher/*",
            "*/*",
            "https://github.com/danrfletcher",
            "https://github.com/danrfletcher/atlas",
            "owner/repo/extra",
            "",
            "/",
            "owner/",
            "/repo",
            "owner/re po",
            "owner/..",
        ] {
            let err = parse_one_repo(bad).expect_err(bad);
            assert!(
                matches!(
                    &err,
                    RegistryError::Remote(RemoteAddError::Refused {
                        reason: "pick_one_repo",
                        ..
                    })
                ),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn an_lfs_pointer_is_a_short_file_that_starts_with_the_spec_line() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 12\n";
        assert!(is_lfs_pointer(pointer));
        assert!(!is_lfs_pointer(b"# a readme\n"));
        let mut long = pointer.to_vec();
        long.extend(vec![b'x'; LFS_POINTER_MAX_BYTES]);
        assert!(!is_lfs_pointer(&long));
    }

    #[test]
    fn hhmm_is_utc_wall_time() {
        assert_eq!(hhmm_utc(0), "00:00 UTC");
        assert_eq!(hhmm_utc(13 * 3600 + 7 * 60 + 59), "13:07 UTC");
    }
}
