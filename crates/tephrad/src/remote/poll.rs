//! The per-source poller (PR-6): keeps a GitHub source current, and keeps it
//! usable when the network goes away.
//!
//! Every [`POLL_INTERVAL`] a source makes ONE conditional request (the ref's
//! ETag in `If-None-Match`). A 304 writes `last_sync_at` and nothing else. A
//! changed ref is read as a whole tree, compared with the items the index
//! has, and the difference is applied in ONE transaction together with the
//! rows of every added or changed text file, which is read again through the
//! API and parsed, the bytes dropped (nothing is written under
//! `remote/<source>/`). Exactly one `SourceChanged` event follows a poll that
//! changed something, and none follows one that did not.
//!
//! Time is the hub's clock. A poll is due when the CLOCK says so; the loop
//! looks at it every `poll_tick` of real time. (A test moves a fake clock; the
//! daemon's runtime is multi-threaded and talks to real sockets, where tokio's
//! paused time would fire every request timeout at once.)
//!
//! One loop per source, so one poll in flight per source: a slow poll delays
//! its own next one and never stacks, and two sources poll independently. A
//! shutdown cancels the loop at an await point; nothing is written before the
//! one transaction at the end, so nothing is half-applied.
//!
//! What a failed poll leaves (never a token, never a removed item):
//!
//! | the poll met                       | status          |
//! |------------------------------------|-----------------|
//! | connect error, DNS, timeout, 5xx   | `offline`       |
//! | a rate limit                       | `syncing`       |
//! | a 401 (or no token)                | `auth_required` |
//! | a 301, a 404 or a 403 on the ref   | `needs_attention` |
//!
//! The index, links and `unit.list` rows are untouched by all of them; the
//! next good poll returns the source to `active` with no re-index.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tephra_cache::{
    is_kept_deleted, Cache, CacheError, RemoteItemRow, RemoteSourceRow, STATUS_ACTIVE,
    STATUS_AUTH_REQUIRED, STATUS_NEEDS_ATTENTION, STATUS_OFFLINE, STATUS_SYNCING,
};
use tephra_cache::{PollState, REASON_LFS_POINTER};
use tephra_remote::github::{full_tree, GitBlobHasher, RefHead, RepoRef};
use tephra_remote::{Conditional, RemoteError, UnreachableCause};

use crate::file_kind::MAX_INDEXED_BYTES;
use crate::registry::{retry_cache_on_busy, DaemonEvent, SourceRegistry};
use crate::remote_github::{
    hhmm_utc, is_lfs_pointer, now_secs, printable, tree_items, IndexJob, RemoteHub, RunEnd,
    TreeItem,
};
use crate::writer_gate::WriterGate;

/// How often a source is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// How often a waiting poller looks at the clock (real time).
pub const DEFAULT_POLL_TICK: Duration = Duration::from_secs(1);
/// How soon a poller looks again while the first index is still reading.
const INDEX_RECHECK: Duration = Duration::from_secs(5);
/// The most file bytes one poll holds before it writes. Files past it are
/// listed and read afterwards, like the first index reads them.
const STAGE_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// The longest an uncached read waits on the remote before it answers
/// `offline`.
pub const READ_BOUND: Duration = Duration::from_secs(10);
const REASON_TOKEN: &str = "GitHub rejected the token: run `tephractl auth github`";
/// The longest a location is quoted in a status reason.
const LOCATION_SHOWN: usize = 200;

// ---------------------------------------------------------------------------
// The registry's side: start a poller, and read one file that is not cached.
// ---------------------------------------------------------------------------

/// Why a read of a file that is not cached could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteReadError {
    /// The remote cannot be reached (connect, DNS, timeout, 5xx).
    Offline {
        reason: String,
    },
    /// The token is missing or was rejected.
    AuthRequired,
    RateLimited {
        retry_at: SystemTime,
    },
    /// The source or the file is not there (or the remote has no such blob).
    NotFound,
    Failed(String),
}

impl std::fmt::Display for RemoteReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteReadError::Offline { reason } => write!(f, "offline: {reason}"),
            RemoteReadError::AuthRequired => f.write_str(REASON_TOKEN),
            RemoteReadError::RateLimited { retry_at } => {
                let secs = retry_at
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                write!(f, "rate limited by GitHub until {}", hhmm_utc(secs))
            }
            RemoteReadError::NotFound => f.write_str("not found"),
            RemoteReadError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for RemoteReadError {}

impl SourceRegistry {
    /// Starts the poller of the remote source `name`: once per source,
    /// however often this is called. The first poll is one [`POLL_INTERVAL`]
    /// from now, by the hub's clock.
    pub fn spawn_remote_poll(&self, name: &str) {
        let Some(hub) = self.remote.get().cloned() else {
            return;
        };
        if !hub
            .polling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string())
        {
            return;
        }
        let job = PollJob {
            // Fixed here, not when the task first runs: the first poll is
            // one interval after the source was started.
            first_due: hub.clock.now() + POLL_INTERVAL,
            name: name.to_string(),
            cache_path: self.cache_path.clone(),
            conn: Arc::new(Mutex::new(None)),
            gate: self.gate.clone(),
            events_tx: self.events_tx.clone(),
            hub,
        };
        self.runtime.spawn(job.run());
    }

    /// The bytes of a file of a remote source that is not cached, read
    /// through the API: nothing is written anywhere. Blocking: call it from
    /// a thread of its own, not from the registry's runtime.
    ///
    /// An unreachable remote is [`RemoteReadError::Offline`] with a reason,
    /// and it is answered within [`READ_BOUND`] whatever the remote does.
    /// (PR-7's `unit.fetch` surfaces it.)
    pub fn read_remote_file(&self, source: &str, path: &str) -> Result<Vec<u8>, RemoteReadError> {
        let hub =
            self.remote.get().cloned().ok_or_else(|| {
                RemoteReadError::Failed("this daemon has no remote sources".into())
            })?;
        let (repo, sha) = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            let row = cache
                .remote_source(source)
                .map_err(|e| RemoteReadError::Failed(e.to_string()))?
                .ok_or(RemoteReadError::NotFound)?;
            let item = cache
                .remote_items(source)
                .map_err(|e| RemoteReadError::Failed(e.to_string()))?
                .into_iter()
                .find(|i| i.path == path && !is_kept_deleted(i))
                .ok_or(RemoteReadError::NotFound)?;
            let repo = RepoRef::parse(&row.target)
                .map_err(|_| RemoteReadError::Failed("the source names no repository".into()))?;
            (repo, item.sha.ok_or(RemoteReadError::NotFound)?)
        };
        let adapter = hub.adapter.clone();
        let answer = self.runtime.block_on(async {
            tokio::time::timeout(READ_BOUND, adapter.get_blob(&repo, &sha)).await
        });
        match answer {
            Err(_) => Err(RemoteReadError::Offline {
                reason: "cannot reach GitHub (timed out)".into(),
            }),
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(RemoteError::Unreachable(cause))) => Err(RemoteReadError::Offline {
                reason: unreachable_reason(cause),
            }),
            Ok(Err(RemoteError::Server { status })) => Err(RemoteReadError::Offline {
                reason: format!("GitHub answered {status}"),
            }),
            Ok(Err(RemoteError::AuthRejected)) => Err(RemoteReadError::AuthRequired),
            Ok(Err(RemoteError::RateLimited { retry_at })) => {
                Err(RemoteReadError::RateLimited { retry_at })
            }
            Ok(Err(RemoteError::NotFound)) => Err(RemoteReadError::NotFound),
            Ok(Err(e)) => Err(RemoteReadError::Failed(format!(
                "could not read from GitHub: {e}"
            ))),
        }
    }
}

fn unreachable_reason(cause: UnreachableCause) -> String {
    format!("cannot reach GitHub ({})", cause.as_str())
}

/// The reason an `offline` source shows for `error` (a connect error, a
/// timeout or a 5xx). The poller and the first index say it the same way.
pub(crate) fn offline_reason(error: &RemoteError) -> String {
    match error {
        RemoteError::Unreachable(cause) => unreachable_reason(*cause),
        RemoteError::Server { status } => format!("cannot reach GitHub (it answered {status})"),
        other => format!("cannot reach GitHub ({other})"),
    }
}

// ---------------------------------------------------------------------------
// The poller.
// ---------------------------------------------------------------------------

/// What the loop does after a poll.
enum Step {
    /// The source is gone: stop.
    Gone,
    /// Poll again one interval from the start of this poll, or at the given
    /// time when that is later (a rate limit).
    Next(Option<SystemTime>),
    /// Look again soon: the first index is still reading, and it is what
    /// brings the source up to date. No request was made.
    Again(Duration),
}

/// Where in a poll an error was met: only the first request says what the
/// REPOSITORY is (a 404 later is a race with a push, which passes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Head,
    Later,
}

struct PollJob {
    first_due: SystemTime,
    name: String,
    cache_path: PathBuf,
    /// The poller's one connection, opened on first use.
    conn: Arc<Mutex<Option<Cache>>>,
    gate: Arc<WriterGate>,
    events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
    hub: Arc<RemoteHub>,
}

/// What a tree changed, against the items the index has.
#[derive(Default)]
struct Plan {
    /// Indexes into the tree's items.
    added: Vec<usize>,
    changed: Vec<usize>,
    removed: Vec<Removal>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// A file the remote no longer has.
struct Removal {
    path: String,
    /// The copy has edits (or an edit is queued): it stays, marked.
    keep: bool,
    /// An unmodified cached copy, deleted after the commit.
    copy: Option<PathBuf>,
    /// The sha the copy must still have when it is deleted.
    sha: Option<String>,
}

impl PollJob {
    async fn run(self) {
        let mut due = self.first_due;
        loop {
            self.wait_until(due).await;
            let started = self.hub.clock.now();
            let step = self.poll().await;
            if !matches!(step, Step::Again(_)) {
                self.hub
                    .polls_done
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            match step {
                Step::Gone => break,
                Step::Next(at) => {
                    due = started + POLL_INTERVAL;
                    if let Some(at) = at.filter(|at| *at > due) {
                        due = at;
                    }
                }
                Step::Again(after) => due = started + after,
            }
        }
        self.hub
            .polling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.name);
    }

    /// Returns once the hub's clock is at `due`.
    async fn wait_until(&self, due: SystemTime) {
        loop {
            match due.duration_since(self.hub.clock.now()) {
                Ok(left) if !left.is_zero() => {
                    tokio::time::sleep(left.min(self.hub.poll_tick)).await
                }
                _ => return,
            }
        }
    }

    /// Runs `f` with the poller's connection on the blocking pool, under
    /// the writer gate (retried on a busy database).
    async fn db<T: Send + 'static>(
        &self,
        f: impl Fn(&Cache, &str) -> Result<T, CacheError> + Send + 'static,
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

    async fn poll(&self) -> Step {
        let loaded = self
            .db(|c, n| Ok((c.remote_source(n)?, c.remote_poll_state(n)?)))
            .await;
        let (source, state) = match loaded {
            Ok((Some(source), Some(state))) => (source, state),
            Ok((None, _)) => return Step::Gone,
            // No cursor row: start-up makes it again.
            Ok((Some(_), None)) => return Step::Next(None),
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "could not read the source before a poll");
                return Step::Next(None);
            }
        };
        // The first index is not finished: it is what brings the source up
        // to date. If a job is reading, look again soon. If none is (it
        // stopped on a network failure, or on the token), the poller runs
        // it: the source shows `offline` (or `auth_required`) while it
        // cannot, and goes on with the index when it can.
        if !state.index_complete {
            let job = IndexJob::new(
                &self.name,
                self.cache_path.clone(),
                self.gate.clone(),
                self.events_tx.clone(),
                self.hub.clone(),
            );
            return match job.run().await {
                RunEnd::Skipped => Step::Again(INDEX_RECHECK),
                RunEnd::Finished | RunEnd::Stopped => Step::Next(None),
            };
        }
        let Ok(repo) = RepoRef::parse(&source.target) else {
            return Step::Next(None);
        };
        let Some(branch) = source.branch.clone() else {
            return Step::Next(None);
        };
        let answer = self
            .hub
            .adapter
            .poll_ref(&repo, &branch, state.etag.as_deref())
            .await;
        match answer {
            Ok(Conditional::NotModified) => self.settle(&source, &state, None).await,
            Ok(Conditional::Modified(head)) if state.commit.as_deref() == Some(&head.sha) => {
                self.settle(&source, &state, Some(head)).await
            }
            Ok(Conditional::Modified(head)) => self.apply(&source, &repo, head).await,
            Err(e) => self.fail(&source, &repo, &branch, e, Phase::Head).await,
        }
    }

    /// The remote has nothing new: `last_sync_at` advances, and a source
    /// that was offline (or waiting, or refused) is `active` again. The
    /// index is not touched, and nothing is announced unless the status is.
    async fn settle(
        &self,
        source: &RemoteSourceRow,
        state: &PollState,
        head: Option<RefHead>,
    ) -> Step {
        let now = now_secs(&*self.hub.clock);
        let was_active = source.status == STATUS_ACTIVE && source.status_reason.is_none();
        let new_etag = head.as_ref().and_then(|h| h.etag.clone());
        let commit = state.commit.clone();
        let etag_moved = head.is_some() && new_etag != state.etag;
        let wrote = self
            .db(move |c, n| {
                if etag_moved {
                    if let Some(commit) = &commit {
                        c.set_remote_poll_state(n, commit, new_etag.as_deref(), now)?;
                    }
                }
                if was_active {
                    c.set_remote_last_sync(n, now)
                } else {
                    c.set_remote_status(n, STATUS_ACTIVE, None, Some(now))
                }
            })
            .await;
        match wrote {
            Ok(()) => {
                if !was_active {
                    tracing::info!(source = %self.name, "remote source is active again");
                    self.announce();
                }
            }
            Err(e) => tracing::warn!(source = %self.name, error = %e, "could not record a poll"),
        }
        Step::Next(None)
    }

    /// The ref moved: read the tree, apply the difference.
    async fn apply(&self, source: &RemoteSourceRow, repo: &RepoRef, head: RefHead) -> Step {
        let branch = source.branch.clone().unwrap_or_default();
        let tree = match full_tree(&*self.hub.adapter, repo, &head.sha).await {
            Ok(tree) => tree,
            Err(e) => return self.fail(source, repo, &branch, e, Phase::Later).await,
        };
        let items = Arc::new(tree_items(&tree.entries));
        drop(tree);

        let clone_root = self.hub.cache_dir(&self.name).join("clone");
        let plan = {
            let items = items.clone();
            let root = clone_root.clone();
            self.db(move |c, n| build_plan(c, n, &items, &root)).await
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "could not compare the tree with the index");
                return Step::Next(None);
            }
        };

        // Read the text of every added or changed file. Nothing is written
        // yet: a failure here (or a shutdown) leaves the index as it was.
        let mut staged: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut staged_bytes = 0u64;
        let mut left_unread = false;
        for &i in plan.added.iter().chain(plan.changed.iter()) {
            let item = &items[i];
            if !tephra_cache::is_text_item_kind(item.kind)
                || item.size.is_some_and(|s| s as u64 > MAX_INDEXED_BYTES)
            {
                continue;
            }
            let size = item.size.unwrap_or(0) as u64;
            if staged_bytes + size > STAGE_CAP_BYTES {
                left_unread = true;
                continue;
            }
            match self.hub.adapter.get_blob(repo, &item.sha).await {
                Ok(bytes) => {
                    staged_bytes += bytes.len() as u64;
                    staged.push((i, bytes));
                }
                Err(e) => return self.fail(source, repo, &branch, e, Phase::Later).await,
            }
        }

        // The one transaction.
        let now = now_secs(&*self.hub.clock);
        let status_changed = source.status != STATUS_ACTIVE || source.status_reason.is_some();
        let plan = Arc::new(plan);
        let staged = Arc::new(staged);
        let committed = {
            let (items, plan, staged, head) =
                (items.clone(), plan.clone(), staged.clone(), head.clone());
            self.db(move |c, n| commit_poll(c, n, &items, &plan, &staged, &head, now))
                .await
        };
        drop(staged);
        match committed {
            Ok(true) => {}
            Ok(false) => return Step::Gone,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "a poll could not be applied; nothing was changed");
                return Step::Next(None);
            }
        }
        // A cached copy nobody edited goes with its file. Re-checked right
        // before it is deleted.
        let removals = plan.clone();
        let root = clone_root.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for r in &removals.removed {
                if let (false, Some(copy), Some(sha)) = (r.keep, &r.copy, &r.sha) {
                    remove_unmodified_copy(&root, copy, sha);
                }
            }
        })
        .await;

        tracing::info!(
            source = %self.name,
            added = plan.added.len(),
            changed = plan.changed.len(),
            removed = plan.removed.len(),
            "remote source updated"
        );
        if !plan.is_empty() || status_changed {
            self.announce();
        }
        if left_unread {
            // Over the poll's byte budget: read like the first index reads.
            IndexJob::new(
                &self.name,
                self.cache_path.clone(),
                self.gate.clone(),
                self.events_tx.clone(),
                self.hub.clone(),
            )
            .run()
            .await;
        }
        Step::Next(None)
    }

    /// Records what a failed request means for the source. Never removes an
    /// item, and never writes anything an unchanged status would not.
    async fn fail(
        &self,
        source: &RemoteSourceRow,
        repo: &RepoRef,
        branch: &str,
        error: RemoteError,
        phase: Phase,
    ) -> Step {
        let full = repo.full_name();
        let mut retry_at = None;
        let (status, reason) = match error {
            e @ (RemoteError::Unreachable(_) | RemoteError::Server { .. }) => {
                (STATUS_OFFLINE, offline_reason(&e))
            }
            RemoteError::RateLimited { retry_at: at } => {
                retry_at = Some(at);
                let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                (
                    STATUS_SYNCING,
                    format!("rate limited by GitHub; polling resumes at {}", hhmm_utc(secs)),
                )
            }
            RemoteError::AuthRejected => (STATUS_AUTH_REQUIRED, REASON_TOKEN.to_string()),
            RemoteError::Moved { location } if phase == Phase::Head => {
                let shown: String = printable(&location).chars().take(LOCATION_SHOWN).collect();
                (
                    STATUS_NEEDS_ATTENTION,
                    format!(
                        "{full} has moved to {shown}: add it under its new name; nothing was removed"
                    ),
                )
            }
            RemoteError::NotFound if phase == Phase::Head => (
                STATUS_NEEDS_ATTENTION,
                format!(
                    "GitHub no longer finds {full} (branch {}): it may be deleted, renamed or no longer shared with the token; nothing was removed",
                    printable(branch)
                ),
            ),
            RemoteError::PermissionDenied if phase == Phase::Head => (
                STATUS_NEEDS_ATTENTION,
                format!("the token is no longer allowed to read {full}; nothing was removed"),
            ),
            other => (
                STATUS_SYNCING,
                format!("a poll did not finish ({other}); trying again"),
            ),
        };
        let unchanged =
            source.status == status && source.status_reason.as_deref() == Some(reason.as_str());
        if !unchanged {
            let why = reason.clone();
            let wrote = self
                .db(move |c, n| c.set_remote_status(n, status, Some(&why), None))
                .await;
            match wrote {
                Ok(()) => {
                    tracing::warn!(source = %self.name, status, "remote source is not being kept current");
                    self.announce();
                }
                Err(e) => {
                    tracing::warn!(source = %self.name, error = %e, "could not record a failed poll")
                }
            }
        }
        Step::Next(retry_at)
    }
}

// ---------------------------------------------------------------------------
// Blocking steps (run on the blocking pool).
// ---------------------------------------------------------------------------

/// What the tree changed against the items. A removed file's cached copy is
/// looked at here, so the commit knows which items to keep.
fn build_plan(
    cache: &Cache,
    name: &str,
    items: &[TreeItem],
    clone_root: &Path,
) -> Result<Plan, CacheError> {
    let rows = cache.remote_items(name)?;
    let existing: HashMap<&str, &RemoteItemRow> =
        rows.iter().map(|r| (r.path.as_str(), r)).collect();
    let present: HashSet<&str> = items.iter().map(|i| i.path.as_str()).collect();
    let mut plan = Plan::default();
    for (i, item) in items.iter().enumerate() {
        match existing.get(item.path.as_str()) {
            None => plan.added.push(i),
            // The remote has the file again; its kept copy is not forgotten.
            Some(row) if is_kept_deleted(row) => plan.added.push(i),
            Some(row) => {
                if row.sha.as_deref() != Some(item.sha.as_str())
                    || row.kind.as_deref() != Some(item.kind)
                {
                    plan.changed.push(i);
                }
            }
        }
    }
    for row in &rows {
        if present.contains(row.path.as_str()) || is_kept_deleted(row) {
            continue;
        }
        let queued = cache.has_queued_upload(name, &row.path)?;
        let mut keep = queued;
        let mut copy = None;
        if let CopyState::File(path) = clone_copy(clone_root, &row.path) {
            let same = row
                .sha
                .as_deref()
                .is_some_and(|sha| file_has_blob_sha(&path, sha));
            if same && !queued {
                copy = Some(path);
            } else {
                keep = true;
            }
        }
        plan.removed.push(Removal {
            path: row.path.clone(),
            keep,
            copy,
            sha: row.sha.clone(),
        });
    }
    Ok(plan)
}

/// Applies a poll: the rows, the staged files, the cursor and the status,
/// in one transaction. `false` when the source is gone.
fn commit_poll(
    cache: &Cache,
    name: &str,
    items: &[TreeItem],
    plan: &Plan,
    staged: &[(usize, Vec<u8>)],
    head: &RefHead,
    now: i64,
) -> Result<bool, CacheError> {
    let Some(source) = cache.get_source(name)? else {
        return Ok(false);
    };
    let tx = cache.connection().unchecked_transaction()?;
    for r in &plan.removed {
        if let Some(unit) = cache.get_unit_by_rel_path(source.id, &r.path)? {
            cache.delete_unit(unit.id)?;
        }
        if r.keep {
            cache.keep_deleted_remote_item(name, &r.path)?;
        } else {
            cache.delete_remote_item(name, &r.path)?;
        }
    }
    let read: HashMap<usize, &Vec<u8>> = staged.iter().map(|(i, b)| (*i, b)).collect();
    for &i in plan.added.iter().chain(plan.changed.iter()) {
        let item = &items[i];
        let new = item.as_new();
        cache.upsert_remote_item(name, &new)?;
        match read.get(&i) {
            Some(bytes) if is_lfs_pointer(bytes) => {
                cache.replace_remote_unit_listing(source.id, &new, MAX_INDEXED_BYTES)?;
                cache.set_remote_item_reason(name, &item.path, Some(REASON_LFS_POINTER))?;
            }
            Some(bytes) => {
                crate::scan::write_remote_unit(cache, source.id, &item.path, bytes)
                    .map(|_| ())
                    .map_err(crate::scan::cache_error_of)?;
            }
            None => cache.replace_remote_unit_listing(source.id, &new, MAX_INDEXED_BYTES)?,
        }
    }
    cache.set_remote_poll_state(name, &head.sha, head.etag.as_deref(), now)?;
    cache.set_remote_status(name, STATUS_ACTIVE, None, Some(now))?;
    tx.commit()?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Cached copies: found only inside `remote/<source>/clone/`, never through a
// symlink, never by a name that could leave it.
// ---------------------------------------------------------------------------

/// The longest name a file system takes.
const NAME_MAX: usize = 255;

/// What is on disk for a repository path.
#[derive(Debug, PartialEq, Eq)]
enum CopyState {
    /// Nothing there.
    Absent,
    /// A regular file, reached without a symlink.
    File(PathBuf),
    /// A name that could leave the clone, a symlink on the way, or
    /// something that is not a regular file: it is not touched.
    Unsafe,
}

/// Looks for the cached copy of `rel` under `clone_root`.
fn clone_copy(clone_root: &Path, rel: &str) -> CopyState {
    if rel.is_empty() || rel.starts_with('/') {
        return CopyState::Unsafe;
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > NAME_MAX
            || part.contains(['\0', '\\'])
        {
            return CopyState::Unsafe;
        }
        parts.push(part);
    }
    // The root itself, then every step below it.
    let mut path = clone_root.to_path_buf();
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_dir() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CopyState::Absent,
        _ => return CopyState::Unsafe,
    }
    let last = parts.len() - 1;
    for (n, part) in parts.iter().enumerate() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if n == last && m.file_type().is_file() => return CopyState::File(path),
            Ok(m) if n < last && m.file_type().is_dir() => {}
            Ok(_) => return CopyState::Unsafe,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CopyState::Absent,
            Err(_) => return CopyState::Unsafe,
        }
    }
    CopyState::Absent
}

/// Whether the file's bytes are the git blob `sha`: the copy is what the
/// remote had, unedited. Any doubt (unreadable, changed while read) is
/// "no".
fn file_has_blob_sha(path: &Path, sha: &str) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut hasher = GitBlobHasher::new(len);
    let mut buf = vec![0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                read += n as u64;
                hasher.update(&buf[..n]);
            }
            Err(_) => return false,
        }
    }
    read == len && hasher.finish() == sha
}

/// Deletes `copy` if it is still the unedited blob `sha`, then the folders
/// it leaves empty (never `clone_root`). An edit made since the poll
/// looked keeps the file.
fn remove_unmodified_copy(clone_root: &Path, copy: &Path, sha: &str) {
    if !file_has_blob_sha(copy, sha) {
        tracing::warn!(path = %copy.display(), "a cached copy changed during a poll; it was kept");
        return;
    }
    if let Err(e) = std::fs::remove_file(copy) {
        tracing::warn!(path = %copy.display(), error = %e, "could not delete a cached copy");
        return;
    }
    let mut dir = copy.parent();
    while let Some(d) = dir {
        if d == clone_root || !d.starts_with(clone_root) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clone_with(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote").join("src").join("clone");
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"hello\n").unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn a_copy_is_found_by_its_repository_path_and_only_as_a_regular_file() {
        let (_d, root) = clone_with(&["docs/Guide.md"]);
        assert_eq!(
            clone_copy(&root, "docs/Guide.md"),
            CopyState::File(root.join("docs/Guide.md"))
        );
        assert_eq!(clone_copy(&root, "docs/Other.md"), CopyState::Absent);
        assert_eq!(clone_copy(&root, "nothing/at/all.md"), CopyState::Absent);
        assert_eq!(clone_copy(&root, "docs"), CopyState::Unsafe, "a folder");
    }

    #[test]
    fn a_name_that_could_leave_the_clone_is_never_looked_up() {
        let (d, root) = clone_with(&["a.md"]);
        std::fs::write(d.path().join("x"), b"outside").unwrap();
        let long = "n".repeat(NAME_MAX + 1);
        for bad in [
            "../../x",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "a//b",
            "./a.md",
            "a\\b",
            "a\0b",
            "",
            long.as_str(),
        ] {
            assert_eq!(clone_copy(&root, bad), CopyState::Unsafe, "{bad:?}");
        }
        // 255 bytes is the longest a name may be.
        assert_eq!(clone_copy(&root, &"n".repeat(NAME_MAX)), CopyState::Absent);
        assert!(d.path().join("x").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_on_the_way_or_at_the_end_is_not_followed() {
        let (d, root) = clone_with(&["a.md"]);
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), root.join("file-link")).unwrap();
        assert_eq!(clone_copy(&root, "link/secret.md"), CopyState::Unsafe);
        assert_eq!(clone_copy(&root, "file-link"), CopyState::Unsafe);
    }

    #[test]
    fn an_unedited_copy_is_the_blob_sha_and_an_edited_one_is_not() {
        let (_d, root) = clone_with(&["a.md"]);
        let sha = tephra_remote::github::git_blob_sha(b"hello\n");
        let path = root.join("a.md");
        assert!(file_has_blob_sha(&path, &sha));
        std::fs::write(&path, b"hello!\n").unwrap();
        assert!(!file_has_blob_sha(&path, &sha));
        assert!(!file_has_blob_sha(&root.join("missing"), &sha));
    }

    #[test]
    fn removing_a_copy_removes_the_folders_it_empties_but_never_the_root() {
        let (_d, root) = clone_with(&["x/y/z.md", "x/keep.md"]);
        let sha = tephra_remote::github::git_blob_sha(b"hello\n");
        remove_unmodified_copy(&root, &root.join("x/y/z.md"), &sha);
        assert!(!root.join("x/y").exists(), "the emptied folder goes");
        assert!(
            root.join("x/keep.md").exists(),
            "a folder with a file stays"
        );
        remove_unmodified_copy(&root, &root.join("x/keep.md"), &sha);
        assert!(root.exists(), "the root stays");
        assert!(!root.join("x").exists());
    }

    #[test]
    fn a_copy_edited_after_the_poll_looked_is_kept() {
        let (_d, root) = clone_with(&["a.md"]);
        let sha = tephra_remote::github::git_blob_sha(b"hello\n");
        std::fs::write(root.join("a.md"), b"edited\n").unwrap();
        remove_unmodified_copy(&root, &root.join("a.md"), &sha);
        assert_eq!(std::fs::read(root.join("a.md")).unwrap(), b"edited\n");
    }
}
