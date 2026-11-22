//! `unit.fetch` (PR-7): the one call that puts a remote file's bytes on disk
//! (the first index's single read and pins, later, are the others).
//!
//! A file is streamed from the adapter into Tephra's own content cache,
//! `<data_dir>/remote/<source>/` (GitHub files under `clone/`, Drive files
//! directly under the source's directory), at the same relative path (every
//! name goes through [`safe_path`], so nothing can leave the cache root or
//! be written through a symlink). The bytes go to a temporary file in the
//! destination directory first, are checked against the listing (size, and
//! for GitHub the git blob sha; a Drive listing has no checksum), fsynced,
//! renamed into place, and the directory is fsynced: a reader sees the old file or the whole new one,
//! never half of it, and a failure at any step leaves no temporary file and
//! the item in state `remote` with the reason in `remote_items.reason`.
//!
//! One download per file at a time: a second caller (from the RPC or the
//! CLI) that arrives while one runs waits for it and gets its result,
//! success or failure. The next call after it finishes starts afresh.
//!
//! A fetch never overwrites a file Tephra did not just write: a copy that
//! is already there and is the remote's bytes is adopted as `cached` (for
//! Drive, the bytes are compared with a download kept in a temporary file);
//! one that differs is left as it is and the item needs attention. A file
//! that was adopted is never deleted by a fetch. A file with
//! unsent or unpushed edits (any state but `remote`, `downloading` and an
//! unedited `cached`/`pinned`) is returned as it is. A fetch never fails
//! because of the cache cap (the cap, PR-8, evicts; it does not refuse).
//!
//! Every call touches `last_used`, a call that did nothing included.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use tephra_cache::{
    Cache, CacheError, RemoteFetchRow, RemoteSourceRow, ITEM_STATE_CACHED, ITEM_STATE_DOWNLOADING,
    ITEM_STATE_NEEDS_ATTENTION, ITEM_STATE_REMOTE, REASON_LFS_POINTER,
};
use tephra_remote::github::{GitBlobHasher, RepoRef};
use tephra_remote::BodyStream;
use tephra_remote::{RemoteError, UnreachableCause};

use crate::registry::{retry_cache_on_busy, SourceRegistry};
use crate::remote::safe_path::{self, SafePathError};
use crate::remote_github::{hhmm_utc, now_secs, printable, RemoteHub, DRIVE_KIND, GITHUB_KIND};

/// The bytes read from a file when it is hashed again.
const HASH_BUFFER: usize = 64 * 1024;

// ---- the disk ------------------------------------------------------------------

/// The file a download is written to. Behind a trait so a test can make a
/// write or an fsync fail the way a full disk does (E16).
pub trait SinkFile: Send {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Flushes the file's data and metadata to the disk (fsync).
    fn sync_all(&mut self) -> io::Result<()>;
}

/// The disk operations of a download that can fail for want of space.
pub trait Disk: Send + Sync {
    /// A new, empty file at `path` (an error if anything is there, a
    /// symlink included).
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn SinkFile>>;
    /// fsync of a directory, so a rename in it survives a crash.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// The real disk.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDisk;

struct RealFile(fs::File);

impl SinkFile for RealFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
}

impl Disk for RealDisk {
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn SinkFile>> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(RealFile(file)))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        fs::File::open(dir)?.sync_all()
    }
}

// ---- what a call answers ---------------------------------------------------------

/// What `unit.fetch` answers: where the file is, and what Tephra says of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FetchWire {
    pub unit: String,
    /// The absolute path of the file in Tephra's content cache.
    pub path: String,
    /// `cached` after a download or for a file already there; the file's
    /// own state for one with edits (`pending_upload`, `not_pushed`, ...).
    pub state: String,
    /// The size of the file on disk, in bytes.
    pub size: u64,
}

/// A fetch that gave no file. `reason` is the stable word clients can
/// branch on; `message` is for a person and holds no credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRpcError {
    pub reason: &'static str,
    pub message: String,
    /// `not_fetchable` only: where the file can be opened instead.
    pub url: Option<String>,
    /// `not_fetchable` only: what the item is (`gdoc`, `symlink`, ...).
    pub kind: Option<String>,
}

impl FetchRpcError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        FetchRpcError {
            reason,
            message: message.into(),
            url: None,
            kind: None,
        }
    }

    /// The `data` of the JSON-RPC error.
    pub fn data(&self) -> serde_json::Value {
        let mut data = serde_json::json!({ "reason": self.reason });
        if let Some(url) = &self.url {
            data["url"] = serde_json::json!(url);
        }
        if let Some(kind) = &self.kind {
            data["kind"] = serde_json::json!(kind);
        }
        data
    }
}

impl From<&FetchFailure> for FetchRpcError {
    fn from(f: &FetchFailure) -> Self {
        FetchRpcError::new(f.code, f.message.clone())
    }
}

/// Why one download failed. Shared by every caller that waited for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchFailure {
    pub code: &'static str,
    pub message: String,
}

fn failure(code: &'static str, message: impl Into<String>) -> FetchFailure {
    FetchFailure {
        code,
        message: message.into(),
    }
}

/// Which remote a failure is about: the words of its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Github,
    Drive,
}

impl Service {
    fn of(kind: &str) -> Option<Service> {
        match kind {
            GITHUB_KIND => Some(Service::Github),
            DRIVE_KIND => Some(Service::Drive),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Service::Github => "GitHub",
            Service::Drive => "Google Drive",
        }
    }

    fn sign_in(self) -> &'static str {
        match self {
            Service::Github => "tephractl auth github",
            Service::Drive => "tephractl auth google",
        }
    }
}

/// The failure for an error the adapter gave before or while a body
/// arrived. None of these texts holds a credential (an adapter error never
/// carries one).
fn remote_failure(e: &RemoteError, clock_secs: u64, service: Service) -> FetchFailure {
    let name = service.name();
    match e {
        RemoteError::AuthRejected => failure(
            "auth_required",
            format!(
                "{name} rejected the token (401): run `{}` and try again",
                service.sign_in()
            ),
        ),
        RemoteError::PermissionDenied => failure(
            "permission_denied",
            format!("{name} refused the file (403): the token has no access to it"),
        ),
        RemoteError::NotFound => failure(
            "remote_missing",
            format!("{name} no longer has this file (404); the next sync updates the listing"),
        ),
        RemoteError::Server { status } => failure(
            "server_error",
            format!("{name} answered with a server error ({status}); try again later"),
        ),
        RemoteError::RateLimited { retry_at } => {
            let at = retry_at
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(clock_secs, |d| d.as_secs());
            failure(
                "rate_limited",
                format!(
                    "{name}'s rate limit is reached; it resets at {}",
                    hhmm_utc(at)
                ),
            )
        }
        RemoteError::Unreachable(UnreachableCause::BodyCut) => connection_dropped(),
        RemoteError::Unreachable(cause) => failure(
            "offline",
            format!(
                "{name} could not be reached ({}): the source is offline",
                cause.as_str()
            ),
        ),
        other => failure(
            "unexpected_answer",
            format!("{name} gave an answer Tephra could not use ({other})"),
        ),
    }
}

fn connection_dropped() -> FetchFailure {
    failure(
        "connection_dropped",
        "the connection dropped before the whole file arrived",
    )
}

fn io_failure(e: &io::Error, what: &str) -> FetchFailure {
    if matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) {
        failure(
            "disk_full",
            format!("the disk is full: no space was left while {what}; nothing was kept"),
        )
    } else {
        failure(
            "disk_error",
            format!("the disk gave an error while {what} ({})", e.kind()),
        )
    }
}

fn path_failure(e: SafePathError) -> FetchFailure {
    match e {
        SafePathError::Symlink(p) => failure(
            "unsafe_path",
            format!(
                "a symbolic link is in the way at {}; the cache never follows one",
                printable(&p.display().to_string())
            ),
        ),
        SafePathError::NotADirectory(p) | SafePathError::IsDirectory(p) => failure(
            "in_the_way",
            format!(
                "something that is not a file of the remote is in the way at {}",
                printable(&p.display().to_string())
            ),
        ),
        SafePathError::Unsafe(name) => failure(
            "unsafe_path",
            format!("{} is not a usable file name", printable(&name)),
        ),
        SafePathError::Io(e) => io_failure(&e, "making the cache directory"),
    }
}

fn db_failure(e: impl std::fmt::Display) -> FetchFailure {
    failure("cache_error", format!("Tephra's index gave an error: {e}"))
}

// ---- coalescing ------------------------------------------------------------------

/// What a finished download is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchDone {
    pub path: PathBuf,
    pub size: u64,
    pub state: String,
}

type Outcome = Result<Arc<FetchDone>, Arc<FetchFailure>>;

struct Flight {
    slot: Mutex<Option<Outcome>>,
    done: Condvar,
}

/// The downloads running now, one per file, and what a download needs of
/// the daemon besides the adapter.
pub struct FetchEngine {
    flights: Mutex<HashMap<(String, String), Arc<Flight>>>,
    serial: AtomicU64,
    pub(crate) disk: Arc<dyn Disk>,
}

impl FetchEngine {
    pub(crate) fn new(disk: Arc<dyn Disk>) -> Self {
        FetchEngine {
            flights: Mutex::new(HashMap::new()),
            serial: AtomicU64::new(0),
            disk,
        }
    }

    fn next_serial(&self) -> u64 {
        self.serial.fetch_add(1, Ordering::Relaxed)
    }
}

enum Role {
    Lead(LeaderGuard),
    Follow(Arc<Flight>),
}

/// Held by the caller that runs a download. However it ends (even a panic),
/// the flight leaves the map first and its waiters are woken after, so a
/// call that comes later starts afresh instead of reading a stale result.
struct LeaderGuard {
    engine: Arc<FetchEngine>,
    key: (String, String),
    flight: Arc<Flight>,
    published: bool,
}

impl LeaderGuard {
    fn publish(mut self, outcome: Outcome) {
        self.finish(outcome);
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.published {
            return;
        }
        self.published = true;
        self.engine
            .flights
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
        *self.flight.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
        self.flight.done.notify_all();
    }
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        self.finish(Err(Arc::new(failure(
            "interrupted",
            "the download stopped before it finished",
        ))));
    }
}

fn join_or_lead(engine: &Arc<FetchEngine>, source: &str, path: &str) -> Role {
    let key = (source.to_string(), path.to_string());
    let mut flights = engine.flights.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(flight) = flights.get(&key) {
        return Role::Follow(flight.clone());
    }
    let flight = Arc::new(Flight {
        slot: Mutex::new(None),
        done: Condvar::new(),
    });
    flights.insert(key.clone(), flight.clone());
    Role::Lead(LeaderGuard {
        engine: engine.clone(),
        key,
        flight,
        published: false,
    })
}

fn wait_for(flight: &Flight) -> Outcome {
    let mut slot = flight.slot.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if let Some(outcome) = slot.as_ref() {
            return outcome.clone();
        }
        slot = flight.done.wait(slot).unwrap_or_else(|p| p.into_inner());
    }
}

// ---- items that are never fetched --------------------------------------------------

/// Where a Google file of `kind` and `file_id` can be opened.
fn google_url(kind: &str, file_id: &str) -> Option<String> {
    let (host, segment) = match kind {
        "gdoc" => ("docs.google.com", "document/d"),
        "gsheet" => ("docs.google.com", "spreadsheets/d"),
        "gslides" => ("docs.google.com", "presentation/d"),
        "gform" => ("docs.google.com", "forms/d"),
        "gdrawing" => ("docs.google.com", "drawings/d"),
        _ => return None,
    };
    let safe = !file_id.is_empty()
        && file_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| format!("https://{host}/{segment}/{file_id}/edit"))
}

/// The refusal for an item whose bytes are never fetched, or `None` for one
/// that can be.
fn not_fetchable(row: &RemoteFetchRow) -> Option<FetchRpcError> {
    let kind = row.kind.as_deref().unwrap_or("");
    let (what, name) = match kind {
        "gdoc" => ("a Google Doc", "Doc"),
        "gsheet" => ("a Google Sheet", "Sheet"),
        "gslides" => ("a Google Slides deck", "Slides"),
        "gform" => ("a Google Form", "Form"),
        "gdrawing" => ("a Google Drawing", "Drawing"),
        "gshortcut" => ("a Drive shortcut", "shortcut"),
        "symlink" => ("a symbolic link", "symlink"),
        "submodule" => ("a git submodule", "submodule"),
        _ if row.reason.as_deref() == Some(REASON_LFS_POINTER) => {
            ("a Git LFS pointer", "Git LFS pointer")
        }
        _ => return None,
    };
    let url = row.file_id.as_deref().and_then(|id| google_url(kind, id));
    let message = match &url {
        Some(url) => format!("{what} has no file to download; open it at {url}"),
        None if kind == "gshortcut" => {
            format!("{what} has no file to download; it is listed by name only")
        }
        None => format!("{what} is listed by name only and its content is never fetched ({name})"),
    };
    let mut error = FetchRpcError::new("not_fetchable", message);
    error.url = url;
    error.kind = Some(if kind.is_empty() {
        "lfs_pointer".to_string()
    } else {
        kind.to_string()
    });
    Some(error)
}

// ---- the registry's side -------------------------------------------------------------

impl SourceRegistry {
    /// Runs `f` on the registry's cache connection under the writer gate.
    fn fetch_db<T>(&self, f: impl Fn(&Cache) -> Result<T, CacheError>) -> Result<T, FetchFailure> {
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        retry_cache_on_busy(&self.gate, || f(&cache)).map_err(db_failure)
    }

    /// `unit.fetch`: see the module documentation. Blocks until the file is
    /// in the cache (or the fetch failed): the caller runs on the blocking
    /// pool.
    pub fn fetch_unit(&self, unit: &str) -> Result<FetchWire, FetchRpcError> {
        let (source, rel) = unit
            .strip_prefix('@')
            .and_then(|rest| rest.split_once(':'))
            .filter(|(s, p)| !s.is_empty() && !p.is_empty())
            .ok_or_else(|| {
                FetchRpcError::new(
                    "unknown_unit",
                    format!(
                        "no such unit: {} (a unit id is @source:path)",
                        printable(unit)
                    ),
                )
            })?;
        let unknown =
            || FetchRpcError::new("unknown_unit", format!("no such unit: {}", printable(unit)));
        let cache_err = |f: FetchFailure| FetchRpcError::new(f.code, f.message);

        let remote_source = self
            .fetch_db(|c| c.remote_source(source))
            .map_err(cache_err)?;
        let Some(remote_source) = remote_source else {
            // A local source: its units are files on the person's own disk.
            let local = self
                .fetch_db(|c| {
                    Ok(match c.get_source(source)? {
                        Some(s) => c.get_unit_by_rel_path(s.id, rel)?.is_some(),
                        None => false,
                    })
                })
                .map_err(cache_err)?;
            return Err(if local {
                FetchRpcError::new(
                    "not_remote",
                    format!(
                        "{} is a file of a local source: there is nothing to fetch",
                        printable(unit)
                    ),
                )
            } else {
                unknown()
            });
        };
        let Some(hub) = self.remote.get().cloned() else {
            return Err(FetchRpcError::new(
                "fetch_unavailable",
                "this daemon has no remote support, so nothing can be fetched",
            ));
        };

        // A lost race (the row changed between reading and acting) reads
        // the row again; three goes are more than a race needs.
        for _ in 0..3 {
            let row = self
                .fetch_db(|c| c.remote_fetch_row(source, rel))
                .map_err(cache_err)?
                .ok_or_else(unknown)?;
            if let Some(refusal) = not_fetchable(&row) {
                return Err(refusal);
            }
            match self.fetch_step(&hub, &remote_source, &row)? {
                Step::Done(done) => {
                    let now = now_secs(&*hub.clock);
                    // Not touching `last_used` is no reason to fail a fetch
                    // that has its file.
                    if let Err(e) = self.fetch_db(|c| c.touch_remote_last_used(source, rel, now)) {
                        tracing::warn!(source, error = %e.message, "could not note the use of a fetched file");
                    }
                    return Ok(FetchWire {
                        unit: unit.to_string(),
                        path: done.path.display().to_string(),
                        state: done.state.clone(),
                        size: done.size,
                    });
                }
                Step::Again => continue,
                Step::Failed(f) => return Err(FetchRpcError::from(&*f)),
            }
        }
        Err(FetchRpcError::new(
            "busy",
            format!(
                "{} kept changing while it was fetched; try again",
                printable(unit)
            ),
        ))
    }

    fn fetch_step(
        &self,
        hub: &Arc<RemoteHub>,
        source: &RemoteSourceRow,
        row: &RemoteFetchRow,
    ) -> Result<Step, FetchRpcError> {
        let case_sibling = self
            .fetch_db(|c| c.remote_has_case_sibling(&row.source, &row.path))
            .map_err(|f| FetchRpcError::from(&f))?;
        let layout = Layout::of(hub, source, row, case_sibling);
        match row.state.as_str() {
            ITEM_STATE_REMOTE | ITEM_STATE_DOWNLOADING => {}
            state => {
                // A copy is on disk (or should be): hand it back untouched.
                let have = layout
                    .existing(false)
                    .map_err(|f| FetchRpcError::from(&f))?;
                return Ok(match have {
                    Some(file) => Step::Done(Arc::new(FetchDone {
                        size: file.size,
                        path: file.path,
                        state: state.to_string(),
                    })),
                    None if state == ITEM_STATE_CACHED => {
                        // Gone from the disk (Dan removed it): the item is
                        // remote again and the next pass downloads it.
                        let _ = self.fetch_db(|c| c.forget_remote_copy(&row.source, &row.path));
                        Step::Again
                    }
                    None => Step::Failed(Arc::new(failure(
                        "no_copy",
                        format!(
                            "the copy of {} is not in the cache any more (state {state}); Tephra left it as it is",
                            printable(&row.path)
                        ),
                    ))),
                });
            }
        }
        if Service::of(&source.kind).is_none() {
            return Ok(Step::Failed(Arc::new(failure(
                "unsupported_source",
                format!("files of a {} source cannot be fetched", source.kind),
            ))));
        }
        match join_or_lead(&hub.fetches, &row.source, &row.path) {
            Role::Follow(flight) => Ok(match wait_for(&flight) {
                Ok(done) => Step::Done(done),
                Err(f) => Step::Failed(f),
            }),
            Role::Lead(guard) => {
                let outcome = self.lead(hub, source, row, &layout);
                let outcome = match outcome {
                    Lead::Lost => {
                        // The row is not ours to change any more. Wake the
                        // waiters with the same word and read it again.
                        guard.publish(Err(Arc::new(failure(
                            "busy",
                            "the file changed while it was being fetched; try again",
                        ))));
                        return Ok(Step::Again);
                    }
                    Lead::Finished(outcome) => outcome,
                };
                guard.publish(outcome.clone());
                Ok(match outcome {
                    Ok(done) => Step::Done(done),
                    Err(f) => Step::Failed(f),
                })
            }
        }
    }

    /// Runs one download as its leader: `remote -> downloading -> cached`,
    /// or back to `remote` with the reason.
    fn lead(
        &self,
        hub: &Arc<RemoteHub>,
        source: &RemoteSourceRow,
        row: &RemoteFetchRow,
        layout: &Layout,
    ) -> Lead {
        match self.fetch_db(|c| c.begin_remote_download(&row.source, &row.path)) {
            Ok(true) => {}
            Ok(false) => return Lead::Lost,
            Err(f) => return Lead::Finished(Err(Arc::new(f))),
        }
        let result = self.download(hub, source, row, layout);
        match result {
            Ok(Downloaded::Fetched {
                path,
                size,
                renamed,
            }) => {
                let finished = self.fetch_db(|c| {
                    c.finish_remote_download(&row.source, &row.path, row.sha.as_deref(), row.size)
                });
                match finished {
                    Ok(true) => Lead::Finished(Ok(Arc::new(FetchDone {
                        path,
                        size,
                        state: ITEM_STATE_CACHED.to_string(),
                    }))),
                    // A sync listed the file anew while its old bytes came
                    // down: those bytes are stale. They go if this fetch
                    // renamed them into place; a copy that was already there
                    // (adopted) is not ours to delete.
                    Ok(false) => {
                        if renamed {
                            let _ = fs::remove_file(&path);
                        }
                        self.fail(
                            row,
                            failure(
                                "changed_meanwhile",
                                format!(
                                    "the file changed on {} while it was being fetched; try again",
                                    Service::of(&source.kind).map_or("the remote", Service::name)
                                ),
                            ),
                        )
                    }
                    Err(f) => {
                        if renamed {
                            let _ = fs::remove_file(&path);
                        }
                        self.fail(row, f)
                    }
                }
            }
            Ok(Downloaded::Existing { path, size, state }) => {
                Lead::Finished(Ok(Arc::new(FetchDone { path, size, state })))
            }
            Err(f) => self.fail(row, f),
        }
    }

    fn fail(&self, row: &RemoteFetchRow, f: FetchFailure) -> Lead {
        let reason = f.message.clone();
        if let Err(e) = self.fetch_db(|c| c.fail_remote_download(&row.source, &row.path, &reason)) {
            tracing::warn!(source = %row.source, error = %e.message, "could not note why a fetch failed");
        }
        tracing::info!(source = %row.source, code = f.code, "a fetch failed");
        Lead::Finished(Err(Arc::new(f)))
    }

    /// The download itself: the file is in place when this returns `Ok`
    /// (and no temporary file is left whatever it returns).
    fn download(
        &self,
        hub: &Arc<RemoteHub>,
        source: &RemoteSourceRow,
        row: &RemoteFetchRow,
        layout: &Layout,
    ) -> Result<Downloaded, FetchFailure> {
        let origin = Origin::of(source, row)?;

        // A GitHub copy already there is never replaced: its bytes can be
        // told from the remote's by the blob sha, with no download. (A Drive
        // listing has no checksum: the check is made against the download.)
        match &origin {
            Origin::Github { sha, .. } => {
                if let Some(found) = layout.existing(false)? {
                    return self.adopt_or_keep(row, found, |path| file_has_blob_sha(path, sha));
                }
            }
            // A symlink or a directory in the way is refused before any
            // bytes are read.
            Origin::Drive { .. } => {
                layout.existing(false)?;
            }
        }

        let dir = layout.parent_dir(true)?;
        let temp = dir.join(safe_path::temp_name(hub.fetches.next_serial()));
        let outcome = self.stream_to(hub, &origin, row, &dir, &temp, layout);
        if outcome.is_err() {
            // Whatever failed, nothing of the download stays.
            let _ = fs::remove_file(&temp);
        }
        outcome
    }

    /// A file Tephra did not write is at the cache path. When `is_remotes`
    /// says its bytes are the remote's (what a killed download or a restored
    /// backup leaves) it is taken as it is; otherwise it is left alone and
    /// the item needs attention.
    fn adopt_or_keep(
        &self,
        row: &RemoteFetchRow,
        found: Found,
        is_remotes: impl FnOnce(&Path) -> bool,
    ) -> Result<Downloaded, FetchFailure> {
        if is_remotes(&found.path) {
            return Ok(Downloaded::Fetched {
                path: found.path,
                size: found.size,
                renamed: false,
            });
        }
        let reason =
            "a file that is not the remote's is at the cache path; Tephra left it as it is";
        self.fetch_db(|c| c.mark_foreign_copy(&row.source, &row.path, reason))?;
        Ok(Downloaded::Existing {
            path: found.path,
            size: found.size,
            state: ITEM_STATE_NEEDS_ATTENTION.to_string(),
        })
    }

    fn stream_to(
        &self,
        hub: &Arc<RemoteHub>,
        origin: &Origin,
        row: &RemoteFetchRow,
        dir: &Path,
        temp: &Path,
        layout: &Layout,
    ) -> Result<Downloaded, FetchFailure> {
        let disk = hub.fetches.disk.clone();
        let now_secs_val = now_secs(&*hub.clock).max(0) as u64;
        let service = origin.service();
        let listed = row.size.filter(|s| *s >= 0).map(|s| s as u64);

        let mut body = self
            .runtime
            .block_on(origin.open(hub))
            .map_err(|e| remote_failure(&e, now_secs_val, service))?;

        let mut sink = disk
            .create_new(temp)
            .map_err(|e| io_failure(&e, "starting the file"))?;
        // A GitHub body is checked by its blob sha as it is written (the id
        // covers the size, so a size is needed to start). Drive has none.
        let mut hasher = match (origin, listed) {
            (Origin::Github { .. }, Some(size)) => Some(GitBlobHasher::new(size)),
            _ => None,
        };
        let mut written: u64 = 0;
        loop {
            let piece = self
                .runtime
                .block_on(body.chunk())
                .map_err(|e| remote_failure(&e, now_secs_val, service))?;
            let Some(piece) = piece else { break };
            written += piece.len() as u64;
            if listed.is_some_and(|size| written > size) {
                return Err(mismatch(row, written, listed));
            }
            if let Some(h) = hasher.as_mut() {
                h.update(&piece);
            }
            sink.write_all(&piece)
                .map_err(|e| io_failure(&e, "writing the file"))?;
        }
        if listed.is_some_and(|size| written != size) {
            // Short: the listing says more.
            return Err(mismatch(row, written, listed));
        }
        sink.sync_all()
            .map_err(|e| io_failure(&e, "flushing the file to the disk"))?;
        drop(sink);

        if let Origin::Github { sha, .. } = origin {
            let got = match hasher {
                Some(h) => h.finish(),
                // No size was listed: the id can only be worked out from the
                // whole file, so it is read back.
                None => {
                    hash_file(temp)
                        .map_err(|e| io_failure(&e, "checking the file"))?
                        .1
                }
            };
            if got != *sha {
                return Err(failure(
                    "corrupt_body",
                    format!(
                        "the bytes received are not the file the listing names ({written} bytes read); nothing was kept"
                    ),
                ));
            }
        }

        // Last look before the rename: nothing of Dan's may be replaced.
        if let Some(found) = layout.existing(false)? {
            let same = |path: &Path| match origin {
                Origin::Github { sha, .. } => file_has_blob_sha(path, sha),
                Origin::Drive { .. } => same_bytes(path, temp),
            };
            return self.adopt_or_keep(row, found, same).inspect(|_| {
                let _ = fs::remove_file(temp);
            });
        }
        let final_path = layout.final_path();
        fs::rename(temp, &final_path).map_err(|e| io_failure(&e, "putting the file in place"))?;
        if let Err(e) = disk.sync_dir(dir) {
            let _ = fs::remove_file(&final_path);
            return Err(io_failure(&e, "flushing the directory to the disk"));
        }
        Ok(Downloaded::Fetched {
            path: final_path,
            size: written,
            renamed: true,
        })
    }
}

/// Where one file's bytes come from.
enum Origin {
    /// A GitHub blob, named (and checked) by its sha.
    Github { repo: RepoRef, sha: String },
    /// A Drive binary or text file, named by its file id.
    Drive { file_id: String },
}

impl Origin {
    fn of(source: &RemoteSourceRow, row: &RemoteFetchRow) -> Result<Origin, FetchFailure> {
        match source.kind.as_str() {
            GITHUB_KIND => {
                let Some(sha) = row.sha.clone() else {
                    return Err(failure(
                        "no_listing",
                        "the listing has no blob id for this file; wait for the next sync",
                    ));
                };
                let repo = RepoRef::parse(&source.target).map_err(|_| {
                    failure("unexpected_answer", "the source names no usable repository")
                })?;
                Ok(Origin::Github { repo, sha })
            }
            DRIVE_KIND => {
                let Some(file_id) = row.file_id.clone().filter(|id| !id.is_empty()) else {
                    return Err(failure(
                        "no_listing",
                        "the listing has no Drive file id for this file; wait for the next sync",
                    ));
                };
                Ok(Origin::Drive { file_id })
            }
            other => Err(failure(
                "unsupported_source",
                format!("files of a {other} source cannot be fetched"),
            )),
        }
    }

    fn service(&self) -> Service {
        match self {
            Origin::Github { .. } => Service::Github,
            Origin::Drive { .. } => Service::Drive,
        }
    }

    /// The request for the bytes: the body arrives piece by piece.
    async fn open(&self, hub: &RemoteHub) -> Result<BodyStream, RemoteError> {
        match self {
            Origin::Github { repo, sha } => hub.adapter.stream_blob(repo, sha).await,
            Origin::Drive { file_id } => hub.drive.stream_download(file_id).await,
        }
    }
}

/// Whether two files hold the same bytes, read in pieces.
fn same_bytes(a: &Path, b: &Path) -> bool {
    fn read_full(file: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let mut buf_a = vec![0u8; HASH_BUFFER];
    let mut buf_b = vec![0u8; HASH_BUFFER];
    loop {
        let (Ok(na), Ok(nb)) = (
            read_full(&mut fa, &mut buf_a),
            read_full(&mut fb, &mut buf_b),
        ) else {
            return false;
        };
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
}

fn mismatch(row: &RemoteFetchRow, got: u64, listed: Option<u64>) -> FetchFailure {
    failure(
        "corrupt_body",
        format!(
            "the file {} arrived with {got} bytes but the listing says {}; nothing was kept",
            printable(&row.path),
            listed.map_or("another size".to_string(), |s| s.to_string())
        ),
    )
}

enum Step {
    Done(Arc<FetchDone>),
    Failed(Arc<FetchFailure>),
    /// The row changed under the call: read it again.
    Again,
}

enum Lead {
    Finished(Outcome),
    /// The item was not in a state to start a download any more.
    Lost,
}

enum Downloaded {
    Fetched {
        path: PathBuf,
        size: u64,
        /// This fetch renamed the file into place (false: a copy that was
        /// already there was adopted, and is not Tephra's to delete).
        renamed: bool,
    },
    /// A file Tephra did not write is there: left alone.
    Existing {
        path: PathBuf,
        size: u64,
        state: String,
    },
}

// ---- where a file goes ----------------------------------------------------------------

struct Found {
    path: PathBuf,
    size: u64,
}

/// Where one item's file lives: the content root of its source and the
/// names below it.
struct Layout {
    cache_dir: PathBuf,
    /// Names from `cache_dir` down to the file (`clone` first for GitHub).
    segments: Vec<String>,
}

impl Layout {
    /// `case_sibling`: another file of the source differs from this one
    /// only by letter case, so its name is made distinct (on a
    /// case-insensitive disk the two would be one file).
    fn of(
        hub: &RemoteHub,
        source: &RemoteSourceRow,
        row: &RemoteFetchRow,
        case_sibling: bool,
    ) -> Layout {
        let mut segments = Vec::new();
        if source.kind == GITHUB_KIND {
            segments.push("clone".to_string());
        }
        segments.extend(if case_sibling {
            safe_path::case_distinct(&row.path)
        } else {
            safe_path::safe_segments(&row.path)
        });
        Layout {
            cache_dir: hub.cache_dir(&source.name),
            segments,
        }
    }

    fn final_path(&self) -> PathBuf {
        let mut p = self.cache_dir.clone();
        p.extend(&self.segments);
        p
    }

    /// The directory the file is in, made (one level at a time, each checked
    /// not to be a symlink) when `create` is set.
    fn parent_dir(&self, create: bool) -> Result<PathBuf, FetchFailure> {
        if create {
            fs::create_dir_all(&self.cache_dir)
                .map_err(|e| io_failure(&e, "making the cache directory"))?;
        }
        // The whole path is resolved (its last name, the file, may be
        // missing); the directories above it are the ones made.
        let file = safe_path::resolve_under(&self.cache_dir, &self.segments, create)
            .map_err(path_failure)?;
        Ok(file
            .parent()
            .map_or_else(|| self.cache_dir.clone(), Path::to_path_buf))
    }

    /// The file at the destination if there is a regular one. A symlink, a
    /// directory or any other thing there is a failure (never followed).
    fn existing(&self, create: bool) -> Result<Option<Found>, FetchFailure> {
        let path = safe_path::resolve_under(&self.cache_dir, &self.segments, create)
            .map_err(path_failure)?;
        match fs::symlink_metadata(&path) {
            Ok(md) if md.file_type().is_file() => Ok(Some(Found {
                size: md.len(),
                path,
            })),
            Ok(_) => Err(failure(
                "in_the_way",
                format!(
                    "something that is not a regular file is at {}",
                    printable(&path.display().to_string())
                ),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_failure(&e, "looking at the cache")),
        }
    }
}

/// The length and the git blob sha of a file, read in pieces.
fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let len = fs::metadata(path)?.len();
    let mut file = fs::File::open(path)?;
    let mut hasher = GitBlobHasher::new(len);
    let mut buf = vec![0u8; HASH_BUFFER];
    let mut seen = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        seen += n as u64;
        hasher.update(&buf[..n]);
    }
    if seen != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file changed while it was read",
        ));
    }
    Ok((len, hasher.finish()))
}

/// Whether the file's bytes are the git blob `sha`.
fn file_has_blob_sha(path: &Path, sha: &str) -> bool {
    hash_file(path).is_ok_and(|(_, got)| got == sha)
}

// ---- start-up ----------------------------------------------------------------------------

/// Removes the temporary files of fetches under `dir`, never following a
/// symlink. What a daemon that was killed mid-download left. Returns how
/// many were removed.
pub fn sweep_temp_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            removed += sweep_temp_files(&entry.path());
        } else if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(safe_path::is_temp_name)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

impl SourceRegistry {
    /// Start-up: no download survives a restart. Every item that says
    /// `downloading` is `remote` again and the temporary files of the dead
    /// downloads are removed.
    pub(crate) fn clean_up_after_downloads(&self, hub: &RemoteHub) {
        match self.fetch_db(|c| c.reset_remote_downloads()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                items = n,
                "items that were downloading when the daemon stopped are remote again"
            ),
            Err(e) => {
                tracing::warn!(error = %e.message, "could not reset the items that were downloading")
            }
        }
        let sources = self.fetch_db(|c| c.remote_sources()).unwrap_or_default();
        for source in sources {
            let removed = sweep_temp_files(&hub.cache_dir(&source.name));
            if removed > 0 {
                tracing::info!(source = %source.name, files = removed, "temporary files of an unfinished fetch removed");
            }
        }
    }
}
