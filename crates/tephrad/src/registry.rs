//! Source management lifecycle (G1): `source.register` /
//! `source.unregister` / `source.list` / `source.rescan`, exposed as a
//! callable Rust API (not JSON-RPC — that's PR-6) against the
//! `tephra-cache` schema.
//!
//! Split into two layers, deliberately:
//! - Pure, injectable validation ([`probe_path`], [`validate_register`],
//!   [`Source`]) that the `#[cfg(test)] mod tests` below exercises
//!   directly against in-memory `Source` structs — no real `Cache`, no
//!   real watcher, no 100k-file fixture, just the duplicate/nested/
//!   non-existent-path decision logic. `probe_path` itself does touch
//!   the real filesystem (there is no other way to answer "does this
//!   path exist"), but every test below passes it a `tempfile`
//!   tempdir or a path that's guaranteed not to exist — no shared
//!   fixture tree, no parsing, no SQLite.
//! - [`SourceRegistry`]: the real orchestrator wiring validation,
//!   `tephra_cache::Cache`, [`crate::scan`], and [`crate::watcher`]
//!   together. Exercised by the integration tests under `tests/`
//!   (and by `tests/registry_lifecycle.rs` specifically for
//!   register/rescan/unregister against a real cache and filesystem).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tephra_cache::{Cache, CacheError, SourceRow, SourceStatus};

use crate::catch_up::{self, BootHooks};
use crate::ignore::IgnoreRules;
use crate::rename_detect::{self, RenameFixer, RenamePool, SourceRenames};
use crate::scan::{self, ScanError, ScanStats};
use crate::watcher::{self, WatchError, WatcherHandle};
use crate::writer_gate::{self, WriterGate};

// ---- pure validation -------------------------------------------------

/// The outcome of checking whether a candidate source root is usable,
/// injected rather than computed inline so [`validate_register`] stays
/// pure and testable without a real filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathProbe {
    Ok,
    Missing,
    NotADirectory,
    /// Exists but its contents can't be listed (e.g. `chmod 000`).
    NotReadable,
    /// Exists but the OS refused with EPERM ("Operation not permitted")
    /// on macOS: the signature of a missing Full Disk Access grant
    /// (TCC), as opposed to ordinary file modes, which give EACCES.
    /// Reported with the Full Disk Access fix line (E13).
    NotReadablePrivacy,
}

impl PathProbe {
    /// Whether the path exists but the OS won't list it: a permission
    /// problem, not a missing or offline source (E13).
    pub fn is_permission_denied(self) -> bool {
        matches!(self, PathProbe::NotReadable | PathProbe::NotReadablePrivacy)
    }
}

/// Real-filesystem [`PathProbe`]: does `path` exist, is it a
/// directory, and can its contents actually be listed. Used by
/// [`SourceRegistry::register`]; unit tests below use `PathProbe`
/// values directly instead of calling this.
///
/// A listing that fails with EPERM on macOS is
/// [`PathProbe::NotReadablePrivacy`] (TCC / Full Disk Access); any
/// other failure to list is [`PathProbe::NotReadable`].
pub fn probe_path(path: &Path) -> PathProbe {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_dir() => PathProbe::NotADirectory,
        Ok(_) => match std::fs::read_dir(path) {
            Ok(_) => PathProbe::Ok,
            Err(e) if is_privacy_block(&e) => PathProbe::NotReadablePrivacy,
            Err(_) => PathProbe::NotReadable,
        },
        // A stat the OS refuses is a permission problem, not a missing
        // path: a source under a protected folder must never be taken
        // for offline just because it can't be looked at (E13).
        Err(e) if is_privacy_block(&e) => PathProbe::NotReadablePrivacy,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => PathProbe::NotReadable,
        Err(_) => PathProbe::Missing,
    }
}

/// Whether `err` is macOS's privacy block: EPERM ("Operation not
/// permitted"), which TCC returns for a protected folder, whereas an
/// ordinary mode/ACL denial is EACCES.
fn is_privacy_block(err: &std::io::Error) -> bool {
    cfg!(target_os = "macos") && err.raw_os_error() == Some(1)
}

/// An in-memory record of one registered source's name and root, the
/// fixture [`validate_register`]'s unit tests use directly (spec:
/// "fixtures are in-memory `Source` structs, no real filesystem").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The candidate root is exactly an existing source's root.
    SameRoot,
    /// The candidate root is nested inside an existing source's root.
    NestedInsideExisting,
    /// An existing source's root is nested inside the candidate root
    /// (the candidate would be an ancestor of it).
    ExistingIsNestedInside,
}

impl fmt::Display for ConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictReason::SameRoot => write!(f, "duplicates its root"),
            ConflictReason::NestedInsideExisting => write!(f, "is nested inside its root"),
            ConflictReason::ExistingIsNestedInside => write!(f, "is an ancestor of its root"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    InvalidPath {
        path: PathBuf,
        reason: PathProbe,
    },
    DuplicateName(String),
    Conflict {
        with: String,
        reason: ConflictReason,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::InvalidPath {
                path,
                reason: PathProbe::NotReadablePrivacy,
            } => {
                // Same text as the plain NotReadable case, plus the
                // Full Disk Access fix line (E13).
                write!(
                    f,
                    "path {path:?} is invalid: {:?}. {}",
                    PathProbe::NotReadable,
                    crate::access::fix_line(&path.display().to_string())
                )
            }
            RegisterError::InvalidPath { path, reason } => {
                write!(f, "path {path:?} is invalid: {reason:?}")
            }
            RegisterError::DuplicateName(name) => {
                write!(f, "a source named \"{name}\" is already registered")
            }
            RegisterError::Conflict { with, reason } => {
                write!(f, "conflicts with existing source \"{with}\": {reason}")
            }
        }
    }
}

impl std::error::Error for RegisterError {}

/// Validates a candidate `(name, root)` registration against the
/// already-registered `existing` sources and a pre-computed `probe`
/// of the candidate root. Pure: no filesystem access, no SQLite — see
/// the module doc comment.
///
/// `root` must already be the *canonical* form of the candidate path
/// (see [`SourceRegistry::register`]) when `existing`'s roots are
/// themselves canonical, as they are in production: comparing a
/// non-canonical candidate (a symlink, a trailing slash, macOS's
/// `/var` vs `/private/var`) against canonical stored roots would
/// silently miss an exact-duplicate or nested/ancestor conflict (R1
/// review finding) — this function itself doesn't canonicalize
/// anything, since it must stay callable with purely synthetic,
/// nonexistent `Source` fixtures in the unit tests below.
///
/// Checked in this order: path validity first (an invalid path is
/// rejected regardless of what else is registered), then name
/// duplication, then the three path-conflict shapes (exact duplicate,
/// nested inside an existing source, or an ancestor of one) —
/// matching the spec's "duplicates ... root, or is nested inside (or
/// is an ancestor of) an existing source's root" wording exactly.
pub fn validate_register<'a>(
    existing: impl IntoIterator<Item = &'a Source>,
    name: &str,
    root: &Path,
    probe: PathProbe,
) -> Result<(), RegisterError> {
    if probe != PathProbe::Ok {
        return Err(RegisterError::InvalidPath {
            path: root.to_path_buf(),
            reason: probe,
        });
    }

    for src in existing {
        if src.name == name {
            return Err(RegisterError::DuplicateName(name.to_string()));
        }
        if src.root == root {
            return Err(RegisterError::Conflict {
                with: src.name.clone(),
                reason: ConflictReason::SameRoot,
            });
        }
        if root.starts_with(&src.root) {
            return Err(RegisterError::Conflict {
                with: src.name.clone(),
                reason: ConflictReason::NestedInsideExisting,
            });
        }
        if src.root.starts_with(root) {
            return Err(RegisterError::Conflict {
                with: src.name.clone(),
                reason: ConflictReason::ExistingIsNestedInside,
            });
        }
    }

    Ok(())
}

/// Why `source.promote` refused (E13). Each has a stable machine-readable
/// [`PromoteError::reason`]; a refusal changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteError {
    /// The path is not a folder: a file, or nothing at all.
    NotAFolder { path: PathBuf },
    /// The OS will not let `tephrad` read the folder.
    Unreadable { path: PathBuf, reason: PathProbe },
    /// The folder is the root of a registered source (a whole vault, or an
    /// already-promoted folder).
    IsASourceRoot { path: PathBuf, source: String },
    /// The folder is inside no registered source.
    NotInsideASource { path: PathBuf },
    /// The source the folder is in is offline.
    SourceOffline { source: String },
    /// The new name breaks the name rules ([`check_source_name`]).
    InvalidName { name: String, why: &'static str },
    /// A source already has this name, compared without regard to case
    /// (FR14).
    NameTaken { name: String, with: String },
}

impl PromoteError {
    /// The stable reason string clients branch on (`data.reason`).
    pub fn reason(&self) -> &'static str {
        match self {
            PromoteError::NotAFolder { .. } => "not_a_folder",
            PromoteError::Unreadable { .. } => "not_readable",
            PromoteError::IsASourceRoot { .. } => "is_a_source_root",
            PromoteError::NotInsideASource { .. } => "not_inside_a_source",
            PromoteError::SourceOffline { .. } => "source_offline",
            PromoteError::InvalidName { .. } => "invalid_name",
            PromoteError::NameTaken { .. } => "name_taken",
        }
    }
}

impl fmt::Display for PromoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromoteError::NotAFolder { path } => {
                write!(f, "{} is not a folder", path.display())
            }
            PromoteError::Unreadable { path, reason } => {
                write!(f, "{} can't be read: {reason:?}", path.display())
            }
            PromoteError::IsASourceRoot { path, source } => write!(
                f,
                "{} is already the root of the source \"{source}\"",
                path.display()
            ),
            PromoteError::NotInsideASource { path } => {
                write!(f, "{} is not inside any registered source", path.display())
            }
            PromoteError::SourceOffline { source } => {
                write!(f, "the source \"{source}\" is offline")
            }
            PromoteError::InvalidName { name, why } => {
                write!(f, "invalid source name \"{name}\": {why}")
            }
            PromoteError::NameTaken { name, with } => {
                write!(f, "the name \"{name}\" is taken by the source \"{with}\"")
            }
        }
    }
}

impl std::error::Error for PromoteError {}

/// The most characters a new source name may have.
pub const MAX_SOURCE_NAME_CHARS: usize = 64;

/// The name rules for a NEW source name (FR14: an existing name is never
/// re-checked): not empty, at most [`MAX_SOURCE_NAME_CHARS`] characters,
/// no whitespace or control character, none of the characters that mean
/// something in a link (`/ \\ @ : # ^ | [ ]`), and not `.` or `..`.
pub fn check_source_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("it must not be empty");
    }
    if name.chars().count() > MAX_SOURCE_NAME_CHARS {
        return Err("it must be at most 64 characters");
    }
    if name == "." || name == ".." {
        return Err("it must not be '.' or '..'");
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("it must not contain spaces or control characters");
    }
    if name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | '@' | ':' | '#' | '^' | '|' | '[' | ']'))
    {
        return Err("it must not contain any of / \\ @ : # ^ | [ ]");
    }
    Ok(())
}

/// The registered source that owns `path`: the one with the deepest root
/// that contains it (a file belongs to the deepest registered root above
/// it). `path` equal to a root is that source's own root.
pub fn deepest_owner<'a>(existing: &'a [Source], path: &Path) -> Option<&'a Source> {
    existing
        .iter()
        .filter(|s| path.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
}

/// Validates a `source.promote {name, root}` against the registered
/// `existing` sources and a pre-computed `probe` of `root`. Pure, like
/// [`validate_register`]: no filesystem, no SQLite. `root` must be
/// canonical (see [`SourceRegistry::promote`]). On success returns the
/// name of the source the folder is in, the one that gives it up.
///
/// Checked in this order: the folder itself (a file or a path that is
/// not there: `not_a_folder`; one the OS will not list), whether it is a
/// registered root (`is_a_source_root`), whether it is inside a source
/// at all (`not_inside_a_source`), then the name (`invalid_name`, then
/// `name_taken`, case-insensitively). A folder that already holds a
/// promoted source is fine: the inner source stays its own.
pub fn validate_promote(
    existing: &[Source],
    name: &str,
    root: &Path,
    probe: PathProbe,
) -> Result<String, PromoteError> {
    match probe {
        PathProbe::Ok => {}
        PathProbe::Missing | PathProbe::NotADirectory => {
            return Err(PromoteError::NotAFolder {
                path: root.to_path_buf(),
            })
        }
        PathProbe::NotReadable | PathProbe::NotReadablePrivacy => {
            return Err(PromoteError::Unreadable {
                path: root.to_path_buf(),
                reason: probe,
            })
        }
    }
    if let Some(src) = existing.iter().find(|s| s.root == root) {
        return Err(PromoteError::IsASourceRoot {
            path: root.to_path_buf(),
            source: src.name.clone(),
        });
    }
    let Some(parent) = deepest_owner(existing, root) else {
        return Err(PromoteError::NotInsideASource {
            path: root.to_path_buf(),
        });
    };
    if let Err(why) = check_source_name(name) {
        return Err(PromoteError::InvalidName {
            name: name.to_string(),
            why,
        });
    }
    if let Some(taken) = existing
        .iter()
        .find(|s| s.name.to_lowercase() == name.to_lowercase())
    {
        return Err(PromoteError::NameTaken {
            name: name.to_string(),
            with: taken.name.clone(),
        });
    }
    Ok(parent.name.clone())
}

// ---- orchestration ----------------------------------------------------

/// One internal `source_changed` event (GP6) — the seam a later
/// broadcaster (PR-6's JSON-RPC/WebSocket layer) subscribes to rather
/// than reimplementing change detection itself. `paths` only ever
/// contains absolute paths that were actually upserted or deleted in
/// the cache (R5) — never a raw, unfiltered watcher event batch.
/// `paths` is empty when no file changed but the source as a whole did
/// (a finished startup rescan, or the source's root vanishing and the
/// source going `offline`): the graph still has to be rebuilt, because
/// it carries each source's offline flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    /// `fixes` is empty for every event but Tephra's own link fix (G17):
    /// then `paths` are the files it rewrote, already indexed again, and
    /// `fixes` the log entries of the renames fixed. The forwarder
    /// refreshes the graph and sends ONE `graph:changed`; the entries
    /// were written to the fix log before the event was sent. `paths` is
    /// empty when no file could be written.
    SourceChanged {
        source: String,
        paths: Vec<PathBuf>,
        fixes: Vec<crate::fixes::FixNotice>,
    },
    /// The list of sources changed (G16, FR12): a source was added,
    /// removed, renamed, promoted or un-promoted. Sent only by
    /// [`SourceRegistry::announce_sources_changed`], after the change is
    /// made, and never for a file change or a fix. The forwarder refreshes
    /// the graph and sends ONE `graph:changed` carrying `sources_changed`.
    ///
    /// `fixes` is empty for every change but a promote (FR22), whose link
    /// rewrites go out in the same push: the log entry of kind `promote`,
    /// with its `files_changed`.
    SourceListChanged {
        changes: Vec<SourceChange>,
        fixes: Vec<crate::fixes::FixNotice>,
    },
}

/// How the source list changed (FR12). The wire name is the lowercase
/// variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceChangeKind {
    Added,
    Removed,
    Renamed,
    Promoted,
    Unpromoted,
}

/// One entry of the `sources_changed` list on `graph:changed`: `kind`
/// and `name`, plus `old_name` for a rename (`name` is then the new name,
/// an additive wire detail).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceChange {
    pub kind: SourceChangeKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
}

impl SourceChange {
    pub fn new(kind: SourceChangeKind, name: impl Into<String>) -> Self {
        SourceChange {
            kind,
            name: name.into(),
            old_name: None,
        }
    }

    /// A rename: `name` is the new name, `old_name` the old one.
    pub fn renamed(old_name: impl Into<String>, new_name: impl Into<String>) -> Self {
        SourceChange {
            kind: SourceChangeKind::Renamed,
            name: new_name.into(),
            old_name: Some(old_name.into()),
        }
    }

    /// The JSON object sent in `sources_changed`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("SourceChange always serializes")
    }
}

#[derive(Debug)]
pub enum RegistryError {
    Validation(RegisterError),
    Cache(CacheError),
    Io(std::io::Error),
    Watch(WatchError),
    Scan(ScanError),
    NotFound(String),
    /// `source.promote` refused (E13): nothing was changed.
    Promote(PromoteError),
    /// The link rewrites of a promote failed after the promote was
    /// recorded; what is left is finished at the next start.
    Fix(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Validation(e) => write!(f, "{e}"),
            RegistryError::Cache(e) => write!(f, "{e}"),
            RegistryError::Io(e) => write!(f, "{e}"),
            RegistryError::Watch(e) => write!(f, "{e}"),
            RegistryError::Scan(e) => write!(f, "{e}"),
            RegistryError::NotFound(name) => write!(f, "no source named \"{name}\""),
            RegistryError::Promote(e) => write!(f, "{e}"),
            RegistryError::Fix(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<RegisterError> for RegistryError {
    fn from(e: RegisterError) -> Self {
        RegistryError::Validation(e)
    }
}
impl From<PromoteError> for RegistryError {
    fn from(e: PromoteError) -> Self {
        RegistryError::Promote(e)
    }
}
impl From<CacheError> for RegistryError {
    fn from(e: CacheError) -> Self {
        RegistryError::Cache(e)
    }
}
impl From<std::io::Error> for RegistryError {
    fn from(e: std::io::Error) -> Self {
        RegistryError::Io(e)
    }
}
impl From<WatchError> for RegistryError {
    fn from(e: WatchError) -> Self {
        RegistryError::Watch(e)
    }
}
impl From<ScanError> for RegistryError {
    fn from(e: ScanError) -> Self {
        RegistryError::Scan(e)
    }
}

struct RunningSource {
    handle: WatcherHandle,
    /// GP8's pause/resume control, wrapping the same `on_batch` this
    /// source's watcher was started with (see `start_watcher`) — so
    /// [`SourceRegistry::pause_all_watchers`]/[`resume_all_watchers`]
    /// can gate every running source's debounced reindex around a
    /// `link.apply_refactor`'s own writes.
    pause: watcher::WatcherPause,
    /// Shared with the live watcher's event filter (R12 review
    /// finding): `rescan` writes a freshly-reloaded `.tephraignore`
    /// into this same lock so the running watcher's filtering (and
    /// every subsequent debounced batch's re-parse filtering)
    /// reflects the rescan's rules immediately, rather than only the
    /// stale rules captured when the watcher was first started.
    ignore_rules: Arc<RwLock<IgnoreRules>>,
    /// `true` while the source is registered. Every `SourceChanged` the
    /// watcher's batch or root-gone callback sends is sent holding this
    /// lock and only while it is `true`; [`SourceRegistry::unregister`]
    /// sets it `false` before it announces the removal, so a batch that
    /// was mid-flight cannot send an event for the name after `removed`.
    live: Arc<Mutex<bool>>,
}

/// Sends `event` unless the source it belongs to has been unregistered.
fn send_if_live(
    live: &Mutex<bool>,
    tx: &tokio::sync::broadcast::Sender<DaemonEvent>,
    event: DaemonEvent,
) {
    let alive = live.lock().unwrap_or_else(|p| p.into_inner());
    if *alive {
        let _ = tx.send(event);
    }
}

/// Aborts its task when dropped, so the task cannot outlive the source
/// that owns it however the source is removed.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How many times a `SQLITE_BUSY` [`CacheError`] is retried, and the
/// backoff slept between attempts, for the handful of quick writes
/// this module makes against the shared `self.cache` connection (R16
/// review finding) — `register()`'s insert and `on_root_removed`'s
/// offline transition specifically, named in the review, but every
/// other write below the same `self.cache.lock()` mutex is wrapped
/// too (status transitions, `set_last_scanned`, `delete_source`),
/// since any of them can just as easily land while another source's
/// scan is mid-chunk and briefly holding the write lock. Mirrors
/// `scan::retry_on_busy`'s shape (same retry count and backoff), but
/// against [`CacheError`] rather than `ScanError` since every write
/// here goes through `tephra_cache::Cache`'s typed API, not this
/// crate's own transaction helpers.
const MAX_CACHE_BUSY_RETRIES: u32 = 8;
const CACHE_BUSY_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);

/// Whether `err` is SQLite's `SQLITE_BUSY`, the same classification
/// `scan::is_database_busy` applies to a `ScanError`.
fn is_cache_busy(err: &CacheError) -> bool {
    matches!(
        err,
        CacheError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// Retries `attempt` up to [`MAX_CACHE_BUSY_RETRIES`] times, sleeping
/// [`CACHE_BUSY_RETRY_BACKOFF`] between attempts, whenever it fails
/// with a `SQLITE_BUSY`-classified [`CacheError`] (R16) — any other
/// error is returned immediately on its first occurrence. Before this
/// fix, a scan whose chunked commits held the write lock past the
/// 5-second `busy_timeout` pragma (R11) made every one of these
/// quick registry writes fail outright with "database is locked"
/// rather than simply waiting its turn.
///
/// `gate` is acquired fresh for each individual attempt, not held
/// across the whole retry loop (R18): this is the same writer gate
/// `crate::scan`'s chunked scan commits and debounced batch-apply
/// acquire around their own `BEGIN IMMEDIATE`/commit cycles against
/// this very same database file, so a quick registry write queued up
/// on it is handed the gate deterministically the moment a scan's
/// current chunk releases it, instead of relying on winning a race
/// against that scan's own immediate reacquire. Releasing it between
/// retries (rather than holding it for the whole backoff loop) means
/// a write that's genuinely stuck waiting on a database held busy by
/// something outside this gate (e.g. a real external connection, as
/// in `tests/register_busy_retry.rs`) never also blocks some other,
/// unrelated source's writes against the same gate for the whole
/// retry window.
pub(crate) fn retry_cache_on_busy<T>(
    gate: &WriterGate,
    mut attempt: impl FnMut() -> Result<T, CacheError>,
) -> Result<T, CacheError> {
    let mut tries = 0;
    loop {
        let result = {
            let _permit = gate.acquire();
            attempt()
        };
        match result {
            Ok(v) => return Ok(v),
            Err(e) if is_cache_busy(&e) && tries < MAX_CACHE_BUSY_RETRIES => {
                tries += 1;
                std::thread::sleep(CACHE_BUSY_RETRY_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

/// The live orchestrator: owns the shared `Cache` (used only for
/// quick, infrequent operations — registering/listing/transitioning a
/// source row), the path that `Cache` was opened against (so a scan
/// or a source's debounced batch-apply path can open its own,
/// independent connection to the same WAL-mode database rather than
/// contending for the shared one — R8), a dedicated tokio runtime for
/// watcher tasks (independent of whatever runtime a caller/test
/// happens to be in), and the map of currently-running per-source
/// watchers.
///
/// Every source's watcher task, `notify` watcher instance, ignore
/// evaluation, and scan/batch-apply SQLite connection are independent
/// of every other source's (GP4): the only state ever shared across
/// sources is the one `Cache` connection behind `cache` (used
/// sparingly and briefly — see above) and the broadcast event
/// channel. A large initial scan or a slow batch for one source does
/// not hold that shared lock, and so cannot stall another source's
/// `list`/`register`/`rescan` call or its own debounced watcher.
pub struct SourceRegistry {
    cache: Arc<Mutex<Cache>>,
    cache_path: PathBuf,
    /// The same fair writer gate (R18, see `crate::writer_gate`)
    /// every write against `cache_path`'s database file goes through
    /// — this registry's own quick writes below, and (via
    /// `writer_gate::for_path`, keyed by the identical path) every
    /// scan/rescan/batch-apply connection `crate::scan` opens
    /// independently. Stored here (rather than looked up fresh each
    /// time) purely so every call site shares one `Arc` without
    /// re-hashing the path on every write; `writer_gate::for_path`
    /// would return the exact same instance either way.
    gate: Arc<WriterGate>,
    runtime: tokio::runtime::Runtime,
    running: Mutex<HashMap<String, RunningSource>>,
    events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
    /// Sources whose root exists but the OS won't list (E13: macOS
    /// Full Disk Access not granted). A runtime status only: it is
    /// never written to the `sources` table (whose `status` CHECK
    /// constraint stays `active|stale|offline`), it starts empty on
    /// every start, and it is set by the startup access sweep
    /// ([`SourceRegistry::refresh_access`]) and by [`SourceRegistry::rescan`].
    /// [`SourceRegistry::list_with_runtime_status`] overlays it on the
    /// stored status, so `source.list` reports `permission_denied`.
    permission_denied: Mutex<HashSet<String>>,
    /// Every writer that pauses the watchers around its own writes
    /// (`link.apply_refactor`, `block.ensure_id`) goes through this one
    /// count, see [`SourceRegistry::pause_for_write`].
    write_pause: Mutex<WritePause>,
    /// What a certain rename seen by a watcher is handed to (G1): the
    /// fix engine, set once by the daemon. While unset, watchers index
    /// a rename as a delete and a create, as before.
    rename_fixer: Arc<RwLock<Option<Arc<dyn RenameFixer>>>>,
    /// The pending-delete pool every source's watcher feeds: what
    /// vanished and appeared and is not paired yet, across ALL sources
    /// (G6), and where each source is. The hold a rename the OS did not
    /// identify waits is [`watcher::RENAME_HOLD`] unless a test shortens
    /// it ([`SourceRegistry::set_rename_hold`]).
    renames: Arc<RenamePool>,
    /// Held for the whole of a `source.promote`: two promotes, however
    /// they overlap, run one after the other.
    promote_lock: Mutex<()>,
    /// The one task that wakes the pool's detector when a hold ends
    /// ([`rename_detect::run_holds`]).
    _hold_task: AbortOnDrop,
}

/// What a `source.promote` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutcome {
    pub name: String,
    /// The new source's canonical root.
    pub root: PathBuf,
    /// The source that gave the folder up, and the folder's path in it.
    pub parent: String,
    pub folder: String,
    /// Cached rows that moved to the new source.
    pub units_moved: usize,
    /// Linking files rewritten.
    pub files_changed: i64,
    /// Linking files left for a retry or a person.
    pub deferred: i64,
    /// The fix-log status: `applied`, `partial` or `needs_manual_fix`.
    pub status: String,
}

/// What [`SourceRegistry::start_all`] did with one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootOutcome {
    /// Compared with the cache (`paths` files brought up to date) and
    /// watched from now on.
    CaughtUp { paths: usize },
    /// Watched from now on, with no comparison: the source has no
    /// finished scan to compare with, or it was already watched.
    Watching,
    /// Skipped: its root is missing. Marked `offline`, rows kept.
    Offline,
    /// Skipped: the OS will not list its root. Rows kept.
    PermissionDenied,
    /// Skipped: its watcher could not be started (or its root could
    /// not be read). Nothing was written for it.
    Failed(String),
    /// A shutdown arrived first: nothing was written for it.
    Stopped,
}

/// The writers currently holding the watchers paused, and the paths
/// they have finished writing so far.
#[derive(Default)]
struct WritePause {
    active: usize,
    written: HashSet<PathBuf>,
}

/// One `source.list` row: the stored row plus the runtime
/// `permission_denied` overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceListing {
    pub row: SourceRow,
    pub permission_denied: bool,
}

impl SourceListing {
    /// The status shown to clients: `permission_denied` wins over the
    /// stored `active|stale|offline` (the source's rows are kept
    /// either way).
    pub fn status_str(&self) -> &'static str {
        if self.permission_denied {
            PERMISSION_DENIED_STATUS
        } else {
            self.row.status.as_str()
        }
    }
}

/// The runtime-only `source.list` status for a source the OS won't let
/// `tephrad` read (E13).
pub const PERMISSION_DENIED_STATUS: &str = "permission_denied";

impl SourceRegistry {
    pub fn new(cache: Cache) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (events_tx, _rx) = tokio::sync::broadcast::channel(1024);
        let cache_path = cache.path().to_path_buf();
        let gate = writer_gate::for_path(&cache_path);
        let renames = Arc::new(RenamePool::new(watcher::RENAME_HOLD));
        let rename_fixer: Arc<RwLock<Option<Arc<dyn RenameFixer>>>> = Arc::new(RwLock::new(None));
        // G2, G6: a rename the OS did not identify waits a hold. The
        // task that ends it runs on the registry's runtime and hands the
        // fix to the blocking pool, like a batch. Its clock stands still
        // while a pause or the writer gate is active.
        let hold_task = {
            let pool = Arc::clone(&renames);
            let pool_for_blocked = Arc::clone(&renames);
            let fixer_slot = Arc::clone(&rename_fixer);
            let blocking = runtime.handle().clone();
            let gate = Arc::clone(&gate);
            runtime.spawn(rename_detect::run_holds(
                Arc::clone(&renames.detector),
                Arc::clone(&renames.wake),
                move || pool_for_blocked.any_paused() || gate.is_busy(),
                move |decisions| {
                    let fixer = fixer_slot.read().unwrap_or_else(|p| p.into_inner()).clone();
                    let Some(fixer) = fixer else { return };
                    let pool = Arc::clone(&pool);
                    drop(blocking.spawn_blocking(move || {
                        pool.dispatch(&*fixer, decisions);
                    }));
                },
            ))
        };
        Ok(SourceRegistry {
            cache: Arc::new(Mutex::new(cache)),
            cache_path,
            gate,
            runtime,
            running: Mutex::new(HashMap::new()),
            events_tx,
            permission_denied: Mutex::new(HashSet::new()),
            write_pause: Mutex::new(WritePause::default()),
            rename_fixer,
            renames,
            promote_lock: Mutex::new(()),
            _hold_task: AbortOnDrop(hold_task),
        })
    }

    /// Replaces the hold a hash-only rename waits ([`watcher::RENAME_HOLD`]),
    /// for the one pool every source shares. For a test that cannot
    /// wait the real two seconds.
    pub fn set_rename_hold(&self, hold: std::time::Duration) {
        self.renames.set_hold(hold);
    }

    /// Subscribes to this registry's internal `source_changed` event
    /// stream (GP6) — the seam a later broadcaster (PR-6) uses.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<DaemonEvent> {
        self.events_tx.subscribe()
    }

    /// `source.register` (G1/GP2/GP3): validates the candidate root,
    /// registers it in the cache, runs its initial full scan
    /// synchronously (so the returned [`ScanStats`] reflect a
    /// genuinely complete scan, not an in-flight one), then starts its
    /// dedicated watcher task.
    ///
    /// If the scan or the watcher fails to start after the source row
    /// has already been inserted, that row is rolled back (deleted)
    /// rather than left behind `Active`, partially (or not at all)
    /// indexed, and with no running watcher (R2 review finding) — a
    /// state a retry of the same name could never otherwise recover
    /// from, since it would be rejected as a duplicate.
    pub fn register(&self, name: &str, root: impl AsRef<Path>) -> Result<ScanStats, RegistryError> {
        let root = root.as_ref();
        let probe = probe_path(root);

        // Canonicalize *before* validating (R1): every existing
        // source's stored root is already canonical (this same
        // method canonicalizes before inserting, below), so comparing
        // a non-canonical candidate against those would miss an exact
        // duplicate or a nested/ancestor conflict spelled differently
        // — a symlink, a trailing slash, or (on macOS) `/var` versus
        // its canonical `/private/var`. Only attempted once `probe`
        // has confirmed the path exists and is readable;
        // `canonicalize()` would just fail the same way otherwise, so
        // the original (invalid) path is kept for `validate_register`
        // to report the real reason against.
        let canonical_root = if probe == PathProbe::Ok {
            root.canonicalize()?
        } else {
            root.to_path_buf()
        };

        // Validate-then-insert happens under one, uninterrupted hold
        // of the cache lock (R1 related fix): otherwise two
        // concurrent `register` calls for overlapping roots could
        // each read "no conflict exists yet" before either one's
        // insert becomes visible to the other.
        let source_id = {
            let cache = self.cache.lock().unwrap();
            let existing: Vec<Source> = cache
                .list_sources()?
                .into_iter()
                .map(|r| Source {
                    name: r.name,
                    root: PathBuf::from(r.path),
                })
                .collect();
            validate_register(&existing, name, &canonical_root, probe)?;
            retry_cache_on_busy(&self.gate, || {
                cache.register_source(
                    name,
                    &canonical_root.to_string_lossy(),
                    SourceStatus::Active,
                )
            })?
        };

        match self.scan_and_watch(name, source_id, &canonical_root) {
            Ok(stats) => {
                tracing::info!(
                    source = %name,
                    source_id,
                    root = %canonical_root.display(),
                    "source_registered"
                );
                // G16: one push, after the row, the scan and the watcher
                // are in place (the forwarder refreshes the graph before it
                // broadcasts). A failed register returns above and sends
                // nothing.
                self.announce_sources_changed(vec![SourceChange::new(
                    SourceChangeKind::Added,
                    name,
                )]);
                Ok(stats)
            }
            Err(e) => {
                let cache = self.cache.lock().unwrap();
                if let Err(cleanup_err) =
                    retry_cache_on_busy(&self.gate, || cache.delete_source(name))
                {
                    tracing::warn!(
                        source = %name,
                        error = %cleanup_err,
                        "failed to roll back a partially registered source after a scan/watch failure"
                    );
                }
                Err(e)
            }
        }
    }

    /// `source.promote` (FR22): the folder `root` of a registered source
    /// becomes the source `name`. The order is the plan's:
    ///
    /// 1. validate ([`validate_promote`], on the canonical root, like
    ///    `register`), and refuse an offline parent;
    /// 2. the watchers are paused, so nothing interleaves;
    /// 3. [`FixEngine::promote`]: ONE transaction (the registry row, the
    ///    parent's exclusion, the log entry, every crossing link), the
    ///    units re-owned, the parent's live watcher told to skip the
    ///    folder, then the link rewrites;
    /// 4. the files written are indexed, and the new source is scanned
    ///    and watched;
    /// 5. the watchers resume (a change made meanwhile is applied now,
    ///    except Tephra's own writes), and ONE push goes out.
    ///
    /// A refusal ([`PromoteError`]) changes nothing and sends nothing.
    pub fn promote(
        &self,
        name: &str,
        root: impl AsRef<Path>,
        engine: &crate::fixes::FixEngine,
    ) -> Result<PromoteOutcome, RegistryError> {
        let _one_at_a_time = self.promote_lock.lock().unwrap_or_else(|p| p.into_inner());
        let root = root.as_ref();
        let probe = probe_path(root);
        // Canonical before validating, as `register` does: stored roots
        // are canonical, and a symlink out of the parent must be seen
        // for where it really goes.
        let canonical_root = if probe == PathProbe::Ok {
            root.canonicalize()?
        } else {
            root.to_path_buf()
        };

        let (parent, folder) = {
            let cache = self.cache.lock().unwrap();
            let rows = cache.list_sources()?;
            let existing: Vec<Source> = rows
                .iter()
                .map(|r| Source {
                    name: r.name.clone(),
                    root: PathBuf::from(&r.path),
                })
                .collect();
            let parent_name = validate_promote(&existing, name, &canonical_root, probe)?;
            let parent = rows
                .into_iter()
                .find(|r| r.name == parent_name)
                .ok_or_else(|| RegistryError::NotFound(parent_name.clone()))?;
            if parent.status == SourceStatus::Offline {
                return Err(PromoteError::SourceOffline {
                    source: parent.name,
                }
                .into());
            }
            let folder = canonical_root
                .strip_prefix(&parent.path)
                .map_err(|_| PromoteError::NotInsideASource {
                    path: canonical_root.clone(),
                })?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (parent, folder)
        };

        let spec = crate::fixes::PromoteSpec {
            parent: parent.name.clone(),
            name: name.to_string(),
            root: canonical_root.clone(),
            folder: folder.clone(),
        };

        self.pause_for_write();
        let result = self.promote_paused(&spec, &parent, engine);
        // No path is discarded on resume (FR24): the rewrites are indexed
        // here and recorded with size and mtime by the write loop's sink
        // (`FixEngine::set_sink`), so the watcher drops the event of
        // exactly those bytes and no other. A discard by path alone would
        // also throw away a user's save made after Tephra's write.
        self.resume_after_write(&HashSet::new());
        let (done, scanned) = result?;

        let mut outcome = PromoteOutcome {
            name: name.to_string(),
            root: canonical_root,
            parent: parent.name,
            folder,
            units_moved: done.units_moved,
            files_changed: done.entry.files_changed,
            deferred: 0,
            status: done.entry.status.clone(),
        };
        outcome.deferred = done
            .announcement
            .notices
            .iter()
            .map(|n| n.deferred)
            .sum::<i64>();
        // ONE push: the source-list change and the fix entry together.
        let _ = self.events_tx.send(DaemonEvent::SourceListChanged {
            changes: vec![SourceChange::new(SourceChangeKind::Promoted, name)],
            fixes: done.announcement.notices,
        });
        tracing::info!(
            source = %name,
            parent = %outcome.parent,
            folder = %outcome.folder,
            units_moved = outcome.units_moved,
            files_changed = outcome.files_changed,
            "source_promoted"
        );
        scanned?;
        Ok(outcome)
    }

    /// The part of [`SourceRegistry::promote`] that runs with the
    /// watchers paused. `Err` only when the promote did not get past the
    /// link rewrites; a failure of the new source's own scan or watcher
    /// (the promote is recorded and the rewrites are done) comes back
    /// beside the result, so the caller still pushes the change.
    fn promote_paused(
        &self,
        spec: &crate::fixes::PromoteSpec,
        parent: &SourceRow,
        engine: &crate::fixes::FixEngine,
    ) -> Result<(crate::fixes::PromoteDone, Result<(), RegistryError>), RegistryError> {
        let parent_root = PathBuf::from(&parent.path);
        let skip_folder = || {
            // From here the parent's live watcher drops every event in
            // the folder, and every load of its rules skips it.
            let rules = self.load_rules(&parent.name, &parent_root);
            let running = self.running.lock().unwrap();
            if let (Some(running), Ok(rules)) = (running.get(&parent.name), rules) {
                *running
                    .ignore_rules
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = rules;
            }
        };
        let done = engine
            .promote(spec, &skip_folder)
            .map_err(|e| RegistryError::Fix(e.to_string()))?;
        for file in &done.announcement.written {
            if let Err(e) = self.reindex_quietly(&file.source, &file.path) {
                tracing::warn!(source = %file.source, path = %file.path.display(), error = %e, "could not index a file the promote rewrote");
            }
        }
        let scanned = self
            .scan_and_watch(&spec.name, done.source_id, &spec.root)
            .map(|_| ());
        Ok((done, scanned))
    }

    /// Finishes the re-owning of any promote that was recorded and not
    /// completed (a kill between its transaction and the move of the
    /// rows). Idempotent; run before anything compares the cache with a
    /// disk, since the parent no longer reads the folder and would
    /// otherwise see its rows as deleted.
    pub fn complete_promotes(&self) {
        let result = {
            let cache = self.cache.lock().unwrap();
            let _permit = self.gate.acquire();
            crate::fixes::complete_promotes(&cache)
        };
        match result {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                units = n,
                "finished the re-owning of an interrupted promote"
            ),
            Err(e) => tracing::warn!(error = %e, "could not finish an interrupted promote"),
        }
    }

    /// The rest of `register`'s work once the source row exists:
    /// initial scan, then start the watcher. Split out so `register`
    /// can roll the row back on any failure from this point on (R2).
    fn scan_and_watch(
        &self,
        name: &str,
        source_id: i64,
        root: &Path,
    ) -> Result<ScanStats, RegistryError> {
        let ignore_rules = Arc::new(RwLock::new(self.load_rules(name, root)?));

        // R13: subscribe the real watcher *before* the walk, not
        // after. `watcher::start` returns as soon as the OS-level
        // `notify` watch is live; any edit landing from this point on
        // — including one that lands while the walk below is still
        // in progress — queues up on the tap's unbounded channel.
        // Only once the scan has committed does `start_watcher`
        // (below) actually start draining that channel into debounced
        // batches, so nothing that happened mid-scan is silently
        // missed the way it was when the watcher didn't exist yet
        // until after the walk returned.
        let tap = watcher::start(root.to_path_buf(), Arc::clone(&ignore_rules))?;

        // A dedicated connection for the initial scan (R8/GP4): never
        // holds `self.cache`'s shared mutex across the whole walk, so
        // a large source's scan can't stall another source's `list`,
        // `register`, `rescan`, or debounced batch-apply while it
        // runs.
        let scan_cache = Cache::open(&self.cache_path)?;
        let ignore_snapshot = ignore_rules.read().unwrap().clone();
        let stats = scan::scan_source(&scan_cache, source_id, root, &ignore_snapshot)?;
        drop(scan_cache);

        tracing::info!(
            source = %name,
            source_id,
            files_seen = stats.files_seen,
            files_ignored = stats.files_ignored,
            units_written = stats.units_written,
            links_written = stats.links_written,
            tags_written = stats.tags_written,
            blocks_written = stats.blocks_written,
            "scan_complete"
        );

        {
            let cache = self.cache.lock().unwrap();
            retry_cache_on_busy(&self.gate, || cache.set_last_scanned(name, now_unix()))?;
        }

        self.start_watcher(
            name.to_string(),
            source_id,
            root.to_path_buf(),
            ignore_rules,
            tap,
        )?;
        Ok(stats)
    }

    /// The ignore rules of source `name` at `root`: the defaults, its
    /// `.tephraignore`, and the folders it gave up to a source of their
    /// own (`promoted_roots`, FR22), which it never scans, watches or
    /// lists. Read from the cache each time, so a promote that has
    /// committed is honored by every load after it.
    fn load_rules(&self, name: &str, root: &Path) -> std::io::Result<IgnoreRules> {
        let rules = IgnoreRules::load(root)?;
        let promoted = Cache::open_read_only(&self.cache_path)
            .and_then(|cache| cache.promoted_roots_of(name))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(rules.with_promoted(promoted.iter().map(|r| r.rel_path.as_str())))
    }

    /// Hands every certain rename a watcher sees to `fixer` from now
    /// on, in every running source and every one started later (G1).
    pub fn set_rename_fixer(&self, fixer: Arc<dyn RenameFixer>) {
        *self.rename_fixer.write().unwrap_or_else(|p| p.into_inner()) = Some(fixer);
    }

    fn start_watcher(
        &self,
        name: String,
        source_id: i64,
        root: PathBuf,
        ignore_rules: Arc<RwLock<IgnoreRules>>,
        tap: watcher::WatcherTap,
    ) -> Result<Arc<SourceRenames>, RegistryError> {
        // A dedicated connection, guarded by its own mutex, for this
        // source's debounced batch-apply path (R8): independent of
        // `self.cache` and of every other source's own dedicated
        // connection, so one source's batch work can never block
        // another's, nor a `list`/`register`/`rescan` call against
        // the shared registry cache.
        let batch_cache = Arc::new(Mutex::new(Cache::open(&self.cache_path)?));
        let events_tx = self.events_tx.clone();
        let live = Arc::new(Mutex::new(true));
        let live_for_batch = Arc::clone(&live);
        let source_name = name.clone();
        let source_root = root.clone();
        let blocking_handle = self.runtime.handle().clone();
        let ignore_for_batch = Arc::clone(&ignore_rules);
        let hints = tap.hints();
        let renames = Arc::new(SourceRenames::new(
            name.clone(),
            root.clone(),
            Arc::clone(&self.renames),
        ));
        let renames_for_batch = Arc::clone(&renames);
        let fixer_for_batch = Arc::clone(&self.rename_fixer);

        let on_batch = move |mut changed: std::collections::HashSet<PathBuf>| {
            // What the OS said about the paths of this batch, taken now
            // so an event that arrives while it is applied belongs to
            // the next one.
            let os_marks = hints.take();
            // G19: the file events of Tephra's own duplicate ID rename
            // are not changes to react to: the file was indexed again
            // as part of the rename. A batch made only of them stops
            // here, so a rename never starts another batch.
            let own = crate::dup_ids::drop_self_writes(&mut changed);
            if own > 0 && changed.is_empty() {
                tracing::debug!(source = %source_name, own, "debounce_fired for Tephra's own write; skipped");
                return;
            }
            tracing::info!(
                source = %source_name,
                batch_size = changed.len(),
                "debounce_fired"
            );

            let batch_cache = Arc::clone(&batch_cache);
            let events_tx = events_tx.clone();
            let live = Arc::clone(&live_for_batch);
            let source_name = source_name.clone();
            let source_root = source_root.clone();
            let ignore_for_batch = Arc::clone(&ignore_for_batch);
            let renames = Arc::clone(&renames_for_batch);
            let fixer_slot = Arc::clone(&fixer_for_batch);

            // Offloaded to tokio's dedicated blocking thread pool
            // (R8): this closure is invoked synchronously from the
            // per-source debounce task, and the work inside it
            // (genuinely blocking SQLite writes) must never occupy
            // one of the daemon's fixed async worker threads for the
            // whole batch — doing so would stall every other
            // source's watcher sharing that runtime.
            let join_handle = blocking_handle.spawn_blocking(move || {
                // A file whose duplicate ID rename could not be written
                // yet (another writer holds it, or it changed under the
                // read) comes back as deferred and is tried again after
                // a pause, with no lock of ours held in between: a held
                // file lock raises no file event, so nothing else would
                // bring the file back, and its cached rows would stay
                // stale (`block.ensure_id` refuses against them).
                let result = scan::settle_deferred(
                    changed,
                    scan::DEFER_RETRY_DELAY,
                    |paths, can_defer| {
                        // R12: read the *current* shared rules, not a
                        // snapshot reloaded from disk independently of
                        // the watcher's own event filter — a rescan
                        // writes its freshly reloaded rules into this
                        // same lock, so the batch-apply path and the
                        // event filter always agree on what's ignored.
                        let ignore_rules = ignore_for_batch.read().unwrap().clone();
                        let cache = batch_cache.lock().unwrap();
                        scan::apply_change_batch_deferring(
                            &cache,
                            source_id,
                            &source_root,
                            paths,
                            &ignore_rules,
                            &crate::refactor::RealFs,
                            can_defer,
                        )
                    },
                    |outcome| {
                        // R5: report only the paths that were actually
                        // upserted or deleted, and skip the event
                        // entirely when nothing really changed (e.g. a
                        // batch made entirely of ignored/directory-only
                        // paths that the watcher-level filter let
                        // through anyway).
                        if !outcome.changed_paths.is_empty() {
                            tracing::info!(
                                source = %source_name,
                                paths = ?outcome.changed_paths,
                                "source_changed"
                            );
                            send_if_live(
                                &live,
                                &events_tx,
                                DaemonEvent::SourceChanged {
                                    source: source_name.clone(),
                                    paths: outcome.changed_paths.clone(),
                                    fixes: Vec::new(),
                                },
                            );
                        }
                    },
                );
                match result {
                    Ok(total) => {
                        // G1: what this batch found gone and new is
                        // matched up, and a certain rename fixed.
                        let fixer = fixer_slot
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        if let Some(fixer) = fixer {
                            renames.on_batch(&*fixer, &os_marks, total.vanished, total.appeared);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(source = %source_name, error = %e, "failed to apply change batch");
                    }
                }
            });
            // Fire-and-forget: `on_batch` itself is a synchronous
            // `Fn`, invoked from the debounce loop's async task, so
            // there's nothing further to await here — the spawned
            // blocking task reports its own success/failure via the
            // `tracing`/broadcast calls above. Dropped explicitly
            // (rather than via `let _ =`) so this fire-and-forget is
            // a documented choice, not a `clippy::let_underscore_future`
            // warning silenced away.
            drop(join_handle);
        };

        let cache_for_offline = Arc::clone(&self.cache);
        let name_for_offline = name.clone();
        let gate_for_offline = Arc::clone(&self.gate);
        let events_for_offline = self.events_tx.clone();
        let live_for_offline = Arc::clone(&live);
        let on_root_removed = move || {
            let cache = cache_for_offline.lock().unwrap();
            if let Err(e) = retry_cache_on_busy(&gate_for_offline, || {
                cache.transition_source_status(&name_for_offline, SourceStatus::Offline)
            }) {
                tracing::warn!(source = %name_for_offline, error = %e, "failed to mark source offline");
            } else {
                tracing::warn!(source = %name_for_offline, "source root is missing; marked offline");
                // The in-memory graph carries each source's offline flag
                // (it is built from the cache), so a status flip has to
                // reach it the way a file change does: announce it, and
                // the daemon refreshes the graph and tells clients.
                // `paths` is empty because no file changed.
                send_if_live(
                    &live_for_offline,
                    &events_for_offline,
                    DaemonEvent::SourceChanged {
                        source: name_for_offline.clone(),
                        paths: Vec::new(),
                        fixes: Vec::new(),
                    },
                );
            }
        };

        // GP8: wrap `on_batch` behind a pause/resume gate so a
        // `link.apply_refactor` can hold off this source's debounced
        // reindex for the duration of its own writes (see
        // `crate::refactor::RefactorEngine::apply` and
        // `SourceRegistry::pause_all_watchers`/`resume_all_watchers`
        // below) without losing any event that fires during that
        // window — only deferring it past the matching `resume_all`.
        let pause = watcher::WatcherPause::new(on_batch);
        let pause_for_handler = pause.clone();

        self.renames.add_source(&name, root.clone(), pause.clone());

        let handle = watcher::spawn_debounce(
            tap,
            root,
            self.runtime.handle().clone(),
            move |batch| pause_for_handler.handle(batch),
            on_root_removed,
        );
        self.running.lock().unwrap().insert(
            name,
            RunningSource {
                handle,
                pause,
                ignore_rules,
                live,
            },
        );
        Ok(renames)
    }

    /// GP8: pauses every currently-running source's watcher batch
    /// handler (see [`watcher::WatcherPause`]) — a filesystem event
    /// that fires while paused is queued, not dropped, and only
    /// processed into a reindex once [`SourceRegistry::resume_all_watchers`]
    /// is called. Idempotent; a source with no running watcher (not
    /// yet started, or already stopped) is simply skipped.
    pub fn pause_all_watchers(&self) {
        for running in self.running.lock().unwrap().values() {
            running.pause.pause();
        }
    }

    /// GP8: resumes every currently-running source's watcher, after
    /// [`pause_all_watchers`](Self::pause_all_watchers), flushing
    /// anything that fired during the pause window as one merged
    /// batch per source. Idempotent. Equivalent to
    /// `resume_all_watchers_except(&HashSet::new())`.
    pub fn resume_all_watchers(&self) {
        self.resume_all_watchers_except(&HashSet::new());
    }

    /// Same as [`resume_all_watchers`](Self::resume_all_watchers), but
    /// every absolute path in `written_paths` is discarded from each
    /// source's pending batch before it's flushed (R4 review finding
    /// — see `crate::refactor::WatcherPauseControl`'s doc comment).
    pub fn resume_all_watchers_except(&self, written_paths: &HashSet<PathBuf>) {
        for running in self.running.lock().unwrap().values() {
            running.pause.resume_except(written_paths);
        }
    }

    /// Whether a watcher is running for `name`: started by
    /// [`SourceRegistry::register`] or by [`SourceRegistry::start_all`].
    pub fn is_watching(&self, name: &str) -> bool {
        self.running.lock().unwrap().contains_key(name)
    }

    /// Whether any running source's watcher is currently paused.
    pub fn watchers_paused(&self) -> bool {
        self.running
            .lock()
            .unwrap()
            .values()
            .any(|running| running.pause.is_paused())
    }

    /// Pauses every watcher for one writer's own writes. Writers
    /// overlap (an `ensure_id` while an `apply_refactor` is writing):
    /// the first one in pauses, and the watchers stay paused until the
    /// matching [`resume_after_write`](Self::resume_after_write) of the
    /// last one out, so one writer finishing never resumes the watchers
    /// under another that is still writing.
    pub fn pause_for_write(&self) {
        let mut gate = self.write_pause.lock().unwrap_or_else(|p| p.into_inner());
        if gate.active == 0 {
            self.pause_all_watchers();
        }
        gate.active += 1;
    }

    /// One writer is done: `written_paths` are the absolute paths it
    /// wrote. The last writer out resumes the watchers, discarding
    /// every path any of the overlapping writers wrote. Each writer
    /// waits out the debounce window before calling this, so the
    /// events of its own writes are already queued by then. Must pair
    /// with a [`pause_for_write`](Self::pause_for_write).
    pub fn resume_after_write(&self, written_paths: &HashSet<PathBuf>) {
        let mut gate = self.write_pause.lock().unwrap_or_else(|p| p.into_inner());
        gate.written.extend(written_paths.iter().cloned());
        gate.active = gate.active.saturating_sub(1);
        if gate.active == 0 {
            let all = std::mem::take(&mut gate.written);
            self.resume_all_watchers_except(&all);
        }
    }

    /// `source.unregister` (G1): stops the watcher task (if one is
    /// running — it may not be, e.g. for a source whose watcher
    /// failed to start) and removes the source's rows from the cache.
    /// Idempotent: calling this twice on an already-removed source
    /// returns `Ok(())` both times rather than erroring. Only a call that
    /// really removed a registered source announces it (G16): one `removed`
    /// event, and none for an unknown name, since nothing changed.
    pub fn unregister(&self, name: &str) -> Result<(), RegistryError> {
        if let Some(running) = self.running.lock().unwrap().remove(name) {
            running.handle.stop();
            // A batch already running for this source can no longer send
            // an event once this is set, so none follows `removed`.
            *running.live.lock().unwrap_or_else(|p| p.into_inner()) = false;
        }
        self.renames.remove_source(name);
        self.set_permission_denied(name, false);
        let existed = {
            let cache = self.cache.lock().unwrap();
            let existed = cache.get_source(name)?.is_some();
            retry_cache_on_busy(&self.gate, || cache.delete_source(name))?;
            existed
        };
        if existed {
            self.announce_sources_changed(vec![SourceChange::new(SourceChangeKind::Removed, name)]);
        }
        Ok(())
    }

    /// The one way a change to the source list reaches clients (G16): sends
    /// ONE [`DaemonEvent::SourceListChanged`] for `changes`, which the
    /// forwarder turns into one `graph:changed` with `sources_changed`,
    /// through the same path as the `fixes` push. `register` and
    /// `unregister` call it; the `renamed`, `promoted` and `unpromoted`
    /// kinds are for the rename, promote and un-promote work to call, once
    /// the change is made. An empty list sends nothing. With nobody
    /// subscribed the event is dropped, not buffered.
    ///
    /// Every caller of `register`/`unregister` pushes, the
    /// `register_sources` bin included; that bin has no forwarder, so
    /// there the event goes to no one.
    pub fn announce_sources_changed(&self, changes: Vec<SourceChange>) {
        if changes.is_empty() {
            return;
        }
        let _ = self.events_tx.send(DaemonEvent::SourceListChanged {
            changes,
            fixes: Vec::new(),
        });
    }

    /// `source.list` (G1): the stored rows, with their stored status.
    /// Clients use [`SourceRegistry::list_with_runtime_status`], which
    /// adds the runtime `permission_denied` overlay.
    pub fn list(&self) -> Result<Vec<SourceRow>, RegistryError> {
        let cache = self.cache.lock().unwrap();
        Ok(cache.list_sources()?)
    }

    /// `source.list` as clients see it (E13): every stored row plus
    /// whether it is currently `permission_denied`.
    pub fn list_with_runtime_status(&self) -> Result<Vec<SourceListing>, RegistryError> {
        let rows = self.list()?;
        let denied = self.permission_denied.lock().unwrap();
        Ok(rows
            .into_iter()
            .map(|row| {
                let permission_denied = denied.contains(&row.name);
                SourceListing {
                    row,
                    permission_denied,
                }
            })
            .collect())
    }

    /// `unit.list` (G1): one folder of `source`, from a fresh read-only
    /// cache connection — it never takes the shared `self.cache` lock or
    /// the writer gate, so a listing can't stall a scan or a register,
    /// nor they it. See [`crate::unit_list`].
    pub fn list_units(
        &self,
        source: &str,
        folder: &str,
        recursive: bool,
    ) -> Result<crate::unit_list::UnitListWire, crate::unit_list::UnitListError> {
        crate::unit_list::list_units(&self.cache_path, source, folder, recursive)
    }

    /// `unit.search` with `scope: "names"` (G8): the best `limit` units
    /// of every source for `query`, from a fresh read-only cache
    /// connection, like [`SourceRegistry::list_units`]: it never takes the
    /// shared `self.cache` lock or the writer gate. See [`crate::search`].
    pub fn search_names(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::search::SearchHitWire>, crate::search::SearchError> {
        crate::search::search_names(&self.cache_path, query, limit)
    }

    /// `unit.search` with `scope: "headings"` or `"blocks"` (G9): the
    /// best `limit` headings, paragraphs or symbols of the one unit
    /// `unit_id` (`@source:path`), from a fresh read-only cache
    /// connection, like [`SourceRegistry::search_names`]. See
    /// [`crate::search::in_file`].
    pub fn search_in_file(
        &self,
        unit_id: &str,
        scope: tephra_cache::InFileScope,
        query: &str,
        limit: usize,
    ) -> Result<crate::search::in_file::InFileSearchWire, crate::search::SearchError> {
        crate::search::in_file::search_in_file(&self.cache_path, unit_id, scope, query, limit)
    }

    /// `unit.search` with `scope: "all_blocks"` (G10, `^^`): the best
    /// `limit` indexed paragraphs, comment blocks, symbols, headings and
    /// document paragraphs of every source for `query`, from a fresh
    /// read-only cache connection, like [`SourceRegistry::search_names`]:
    /// it never takes the shared `self.cache` lock or the writer gate.
    /// See [`crate::search::all_blocks`].
    pub fn search_all_blocks(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::search::all_blocks::AllBlocksHitWire>, crate::search::SearchError> {
        crate::search::all_blocks::search_all_blocks(&self.cache_path, query, limit)
    }

    /// Whether `name` is currently marked `permission_denied`.
    pub fn is_permission_denied(&self, name: &str) -> bool {
        self.permission_denied.lock().unwrap().contains(name)
    }

    fn set_permission_denied(&self, name: &str, denied: bool) {
        let mut set = self.permission_denied.lock().unwrap();
        if denied {
            set.insert(name.to_string());
        } else {
            set.remove(name);
        }
    }

    /// The startup access sweep (E13): checks each registered source's
    /// root once and marks the ones the OS refuses to list as
    /// `permission_denied` (clearing the mark on the ones that list
    /// fine). It reads nothing else and changes no cached row. A
    /// missing root is left alone: that is `offline`, handled by the
    /// watcher and by [`SourceRegistry::rescan`]. Returns the names
    /// newly marked.
    pub fn refresh_access(&self) -> Result<Vec<String>, RegistryError> {
        let mut marked = Vec::new();
        for row in self.list()? {
            let probe = probe_path(Path::new(&row.path));
            if probe.is_permission_denied() {
                if !self.is_permission_denied(&row.name) {
                    tracing::warn!(
                        source = %row.name,
                        root = %row.path,
                        "source root can't be read (permission denied); marked permission_denied"
                    );
                }
                self.set_permission_denied(&row.name, true);
                marked.push(row.name);
            } else if probe == PathProbe::Ok {
                self.set_permission_denied(&row.name, false);
            }
        }
        Ok(marked)
    }

    /// The boot's last two steps (G5), after
    /// [`SourceRegistry::rescan_unfinished`]: for every registered
    /// source that is online and readable, a startup catch-up (what
    /// changed on disk while `tephrad` was down, found by comparing the
    /// disk with the cached rows, a certain rename fixed under the live
    /// rules), then a watcher, in the same `running` map
    /// [`SourceRegistry::register`] fills.
    ///
    /// Per source, in this order: the OS watch is subscribed first (an
    /// edit made while the catch-up runs queues and is indexed once the
    /// watcher drains it, never missed); the source is compared with its
    /// cache and the new files that could be a vanished file's twin are
    /// hashed; then, one source at a time, the changed files are indexed
    /// in one transaction (through the writer gate, so an RPC that
    /// arrives meanwhile waits its turn), one `source_changed` is sent
    /// if anything changed, the watcher is started, and what vanished
    /// and appeared is handed to the source's rename detector, which
    /// applies a unique hash pair after the hold
    /// ([`SourceRegistry::set_rename_hold`]) and logs the vanished that
    /// got no pair as `uncertain`. `rescan_source` is never used: it
    /// deletes the rows first, which would leave nothing to compare.
    ///
    /// Skipped, with nothing written and no watcher: a source whose root
    /// is missing (marked `offline`, rows kept) and one the OS will not
    /// list. A source whose scan never finished has no baseline to
    /// compare with, so it is only watched. A source stored as `offline`
    /// whose root is back is compared like any other and goes
    /// `offline` -> `stale` -> `active` around the catch-up. A source whose watcher
    /// cannot start is logged and skipped; the others carry on.
    /// `should_stop` is checked before each source; one already being
    /// indexed finishes, since its batch is a single transaction.
    pub fn start_all(&self, should_stop: impl Fn() -> bool) -> Vec<(String, BootOutcome)> {
        self.start_all_with(should_stop, &BootHooks::default())
    }

    /// [`SourceRegistry::start_all`] with how a new file is hashed and
    /// how a source's watcher is started supplied by the caller.
    pub fn start_all_with(
        &self,
        should_stop: impl Fn() -> bool,
        hooks: &BootHooks<'_>,
    ) -> Vec<(String, BootOutcome)> {
        /// One online source on its way to a watcher.
        struct Boot {
            row: SourceRow,
            root: PathBuf,
            ignore_rules: Arc<RwLock<IgnoreRules>>,
            tap: watcher::WatcherTap,
            /// `None` when there is no baseline to compare with.
            diff: Option<catch_up::Diff>,
            vanished: Vec<crate::rename_detect::Vanished>,
            appeared: Vec<crate::rename_detect::Appeared>,
        }

        self.complete_promotes();
        let rows = match self.list() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "could not list the sources to start their watchers");
                return Vec::new();
            }
        };
        let mut outcomes: Vec<(String, BootOutcome)> = Vec::new();
        let mut boots: Vec<Boot> = Vec::new();

        // 1. Which sources are online, and their OS watch.
        for row in rows {
            if should_stop() {
                outcomes.push((row.name, BootOutcome::Stopped));
                continue;
            }
            if self.running.lock().unwrap().contains_key(&row.name) {
                outcomes.push((row.name, BootOutcome::Watching));
                continue;
            }
            let root = PathBuf::from(&row.path);
            match probe_path(&root) {
                PathProbe::Ok => self.set_permission_denied(&row.name, false),
                PathProbe::NotReadable | PathProbe::NotReadablePrivacy => {
                    tracing::warn!(source = %row.name, root = %root.display(), "source root can't be read (permission denied); no catch-up and no watcher");
                    self.set_permission_denied(&row.name, true);
                    outcomes.push((row.name, BootOutcome::PermissionDenied));
                    continue;
                }
                PathProbe::Missing | PathProbe::NotADirectory => {
                    self.mark_offline(&row);
                    outcomes.push((row.name, BootOutcome::Offline));
                    continue;
                }
            }
            let ignore_rules = match self.load_rules(&row.name, &root) {
                Ok(rules) => Arc::new(RwLock::new(rules)),
                Err(e) => {
                    tracing::warn!(source = %row.name, error = %e, "could not read the source's ignore rules; no catch-up and no watcher");
                    outcomes.push((row.name, BootOutcome::Failed(e.to_string())));
                    continue;
                }
            };
            let tap = match (hooks.start_tap)(&root, Arc::clone(&ignore_rules)) {
                Ok(tap) => tap,
                Err(e) => {
                    tracing::warn!(source = %row.name, error = %e, "the source's watcher could not be started; skipping the source");
                    outcomes.push((row.name, BootOutcome::Failed(e.to_string())));
                    continue;
                }
            };
            boots.push(Boot {
                row,
                root,
                ignore_rules,
                tap,
                diff: None,
                vanished: Vec::new(),
                appeared: Vec::new(),
            });
        }

        // 2. Compare each source with its cache (reads only).
        let mut stopped = false;
        let mut failed: Vec<usize> = Vec::new();
        for (i, boot) in boots.iter_mut().enumerate() {
            if should_stop() {
                stopped = true;
                break;
            }
            let compared = (|| -> Result<Option<catch_up::Diff>, String> {
                let cache = Cache::open_read_only(&self.cache_path).map_err(|e| e.to_string())?;
                if !cache
                    .scan_finished(boot.row.id)
                    .map_err(|e| e.to_string())?
                {
                    tracing::info!(source = %boot.row.name, "no finished scan to compare with; watching only");
                    return Ok(None);
                }
                let cached =
                    catch_up::cached_files(&cache, &boot.row.name).map_err(|e| e.to_string())?;
                let rules = boot.ignore_rules.read().unwrap().clone();
                let disk = catch_up::list_disk(&boot.root, &rules).map_err(|e| e.to_string())?;
                Ok(Some(catch_up::diff(cached, disk, |rel| {
                    catch_up::is_gone(&boot.root, rel)
                })))
            })();
            match compared {
                Ok(diff) => boot.diff = diff,
                Err(e) => {
                    tracing::warn!(source = %boot.row.name, error = %e, "catch-up could not compare the source with its cache; skipping the source");
                    outcomes.push((boot.row.name.clone(), BootOutcome::Failed(e)));
                    failed.push(i);
                }
            }
        }

        // 3. Hash the new files that could be a vanished file's twin
        // (size equal to a vanished file's, in any source). Whether a
        // hash is unique across the sources (FR21) is for the pool the
        // sources share ([`rename_detect::pair`]).
        if !stopped {
            let sizes = catch_up::size_matches(boots.iter().filter_map(|b| b.diff.as_ref()));
            for boot in boots.iter_mut() {
                let Some(diff) = &boot.diff else { continue };
                let hashes = catch_up::hash_new_files(diff, &boot.root, &sizes, hooks.hash_file);
                boot.vanished = catch_up::vanished_facts(diff);
                boot.appeared = catch_up::appeared_facts(diff, &hashes);
            }
        }

        // 4. One source at a time: index what changed, start the watcher,
        // hand the rename detector what vanished and appeared.
        for (i, boot) in boots.into_iter().enumerate() {
            if failed.contains(&i) {
                continue;
            }
            if stopped || should_stop() {
                stopped = true;
                outcomes.push((boot.row.name, BootOutcome::Stopped));
                continue;
            }
            let Boot {
                row,
                root,
                ignore_rules,
                tap,
                diff,
                vanished,
                appeared,
            } = boot;
            let mut paths = 0;
            if let Some(diff) = &diff {
                if row.status == SourceStatus::Offline {
                    // The root is back and its cached rows are the
                    // baseline just compared: stale while they are
                    // brought up to date (offline cannot go straight
                    // to active).
                    self.set_status(&row, SourceStatus::Stale);
                }
                let changed = diff.changed_paths(&root);
                if !changed.is_empty() {
                    match self.index_catch_up(&row, &root, &ignore_rules, changed) {
                        Ok(n) => paths = n,
                        Err(e) => {
                            tracing::warn!(source = %row.name, error = %e, "catch-up could not bring the cache up to date; skipping the source");
                            outcomes.push((row.name, BootOutcome::Failed(e)));
                            continue;
                        }
                    }
                }
            }
            if diff.is_some() && row.status != SourceStatus::Active {
                self.set_status(&row, SourceStatus::Active);
            }
            let renames = match self.start_watcher(
                row.name.clone(),
                row.id,
                root,
                ignore_rules,
                tap,
            ) {
                Ok(renames) => renames,
                Err(e) => {
                    tracing::warn!(source = %row.name, error = %e, "the source's watcher could not be started");
                    outcomes.push((row.name, BootOutcome::Failed(e.to_string())));
                    continue;
                }
            };
            if diff.is_some() {
                let fixer = self
                    .rename_fixer
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                if let Some(fixer) = fixer {
                    renames.on_batch(&*fixer, &HashMap::new(), vanished, appeared);
                }
                tracing::info!(source = %row.name, paths, "catch_up_complete");
                outcomes.push((row.name, BootOutcome::CaughtUp { paths }));
            } else {
                outcomes.push((row.name, BootOutcome::Watching));
            }
        }
        outcomes
    }

    /// Brings the cache up to date with `changed` in one transaction, as
    /// a watcher batch does, and announces it as one `source_changed`.
    /// Returns how many paths changed.
    fn index_catch_up(
        &self,
        row: &SourceRow,
        root: &Path,
        ignore_rules: &Arc<RwLock<IgnoreRules>>,
        changed: HashSet<PathBuf>,
    ) -> Result<usize, String> {
        let cache = Cache::open(&self.cache_path).map_err(|e| e.to_string())?;
        let rules = ignore_rules.read().unwrap().clone();
        let total = scan::settle_deferred(
            changed,
            scan::DEFER_RETRY_DELAY,
            |paths, can_defer| {
                scan::apply_change_batch_deferring(
                    &cache,
                    row.id,
                    root,
                    paths,
                    &rules,
                    &crate::refactor::RealFs,
                    can_defer,
                )
            },
            |_| {},
        )
        .map_err(|e| e.to_string())?;
        let n = total.changed_paths.len();
        if n > 0 {
            let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                source: row.name.clone(),
                paths: total.changed_paths,
                fixes: Vec::new(),
            });
        }
        Ok(n)
    }

    /// A source whose root is missing: stored as `offline` (its rows
    /// stay), and the graph told, as the watcher does when a root goes.
    fn mark_offline(&self, row: &SourceRow) {
        if row.status == SourceStatus::Offline {
            return;
        }
        let cache = self.cache.lock().unwrap();
        match retry_cache_on_busy(&self.gate, || {
            cache.transition_source_status(&row.name, SourceStatus::Offline)
        }) {
            Ok(()) => {
                tracing::warn!(source = %row.name, "source root is missing; marked offline");
                let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                    source: row.name.clone(),
                    paths: Vec::new(),
                    fixes: Vec::new(),
                });
            }
            Err(e) => {
                tracing::warn!(source = %row.name, error = %e, "failed to mark source offline");
            }
        }
    }

    /// Moves a source's stored status along one valid edge and tells the
    /// graph, logging (not failing) when the edge is refused.
    fn set_status(&self, row: &SourceRow, to: SourceStatus) {
        let cache = self.cache.lock().unwrap();
        match retry_cache_on_busy(&self.gate, || cache.transition_source_status(&row.name, to)) {
            Ok(()) => {
                let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                    source: row.name.clone(),
                    paths: Vec::new(),
                    fixes: Vec::new(),
                });
            }
            Err(e) => {
                tracing::warn!(source = %row.name, error = %e, status = ?to, "failed to change the source's status");
            }
        }
    }

    /// E11: rescans from the top every source whose last scan started
    /// and never finished (the daemon was killed mid-scan, or a schema
    /// rebuild emptied it). Called once at startup. There is no resume
    /// logic: [`SourceRegistry::rescan`] clears the source's rows and
    /// re-walks it, so the result has no duplicate `(source_id,
    /// rel_path)` rows and the same unit count as a clean scan.
    ///
    /// Each source is rescanned in turn; `should_stop` is checked
    /// before each one so a shutdown doesn't wait for sources that
    /// haven't started (their scan stays unfinished and is redone next
    /// start). A scan already running is not interrupted. A source
    /// whose root is missing is handled by `rescan` (it goes
    /// `offline`) and stays unfinished until a real scan completes.
    /// One failed source never stops the others; every outcome is
    /// returned. Each successful rescan also emits a `source_changed`
    /// event so the in-memory graph is refreshed and clients are told.
    pub fn rescan_unfinished(
        &self,
        should_stop: impl Fn() -> bool,
    ) -> Vec<(String, Result<ScanStats, RegistryError>)> {
        self.complete_promotes();
        let unfinished = {
            let cache = self.cache.lock().unwrap();
            match cache.sources_with_unfinished_scan() {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to list sources with an unfinished scan");
                    return Vec::new();
                }
            }
        };

        let mut outcomes = Vec::new();
        for row in unfinished {
            if should_stop() {
                tracing::info!(
                    "shutdown requested; leaving the remaining unfinished scans for the next start"
                );
                break;
            }
            tracing::info!(
                source = %row.name,
                source_id = row.id,
                root = %row.path,
                "scan did not finish last time; rescanning from the top"
            );
            let result = self.rescan(&row.name);
            match &result {
                Ok(stats) => {
                    tracing::info!(
                        source = %row.name,
                        units_written = stats.units_written,
                        "interrupted scan redone"
                    );
                    let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                        source: row.name.clone(),
                        paths: Vec::new(),
                        fixes: Vec::new(),
                    });
                }
                Err(e) => {
                    tracing::warn!(source = %row.name, error = %e, "failed to redo an interrupted scan");
                }
            }
            outcomes.push((row.name, result));
        }
        outcomes
    }

    /// `source.rescan` (G1): forces a full re-walk of one source,
    /// re-evaluating its `.tephraignore` fresh from disk (so an edit
    /// to it takes effect on the next rescan rather than reusing a
    /// cached file list) and clearing only that source's own rows
    /// first — never the whole cache (that's
    /// [`tephra_cache::Cache::rebuild`]'s job, gated by schema
    /// version).
    ///
    /// The clear-then-rewalk runs as **one** transaction against a
    /// dedicated connection (R2/R8: see [`scan::rescan_source`]),
    /// never holding `self.cache`'s shared mutex across the whole
    /// walk, and never leaving the source's rows cleared with nothing
    /// re-inserted if a walk entry errors out partway through.
    ///
    /// If the source's root is currently missing, this is a safe
    /// no-op that (re)confirms the `offline` status rather than
    /// erroring the whole call out or crashing — consistent with "a
    /// watched root deleted out from under a running watcher"
    /// (handled live by the watcher itself) also holding for an
    /// explicit rescan of an already-offline source.
    ///
    /// If the root exists but the OS won't list it (E13: macOS Full
    /// Disk Access not granted), the rescan changes no cached row and
    /// no stored status; the source is marked `permission_denied` (a
    /// runtime status, see [`SourceRegistry::list_with_runtime_status`])
    /// and the call returns empty stats. Once access is granted, the
    /// next rescan walks normally, lands on `active` and clears the
    /// mark, with no restart needed.
    pub fn rescan(&self, name: &str) -> Result<ScanStats, RegistryError> {
        let (source_id, root, status) = {
            let cache = self.cache.lock().unwrap();
            let row = cache
                .get_source(name)?
                .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
            (row.id, PathBuf::from(row.path), row.status)
        };

        let probe = probe_path(&root);

        // E13: a root that exists but can't be listed is not empty and
        // not offline. Keep every cached row and the stored status,
        // mark it `permission_denied`, and scan nothing.
        if probe.is_permission_denied() {
            tracing::warn!(
                source = %name,
                root = %root.display(),
                "source root can't be read (permission denied); keeping its cached rows"
            );
            self.set_permission_denied(name, true);
            return Ok(ScanStats::default());
        }

        if probe != PathProbe::Ok {
            let cache = self.cache.lock().unwrap();
            if status != SourceStatus::Offline {
                retry_cache_on_busy(&self.gate, || {
                    cache.transition_source_status(name, SourceStatus::Offline)
                })?;
            }
            return Ok(ScanStats::default());
        }

        {
            let cache = self.cache.lock().unwrap();
            if status != SourceStatus::Stale {
                retry_cache_on_busy(&self.gate, || {
                    cache.transition_source_status(name, SourceStatus::Stale)
                })?;
            }
        }

        let ignore_rules = self.load_rules(name, &root)?;

        // R12: push the freshly-reloaded rules into the live
        // watcher's shared lock (if one is currently running for this
        // source) *before* the re-walk below, so a path the walk
        // un-ignores is also immediately un-ignored for every
        // watcher event and debounced batch from this point on, not
        // just for this one walk — and conversely for a path newly
        // ignored.
        if let Some(running) = self.running.lock().unwrap().get(name) {
            *running.ignore_rules.write().unwrap() = ignore_rules.clone();
        }

        let scan_cache = Cache::open(&self.cache_path)?;
        let stats = match scan::rescan_source(&scan_cache, source_id, &root, &ignore_rules) {
            Ok(stats) => stats,
            Err(ScanError::PermissionDenied(path)) => {
                // The root listed a moment ago (the probe above) but
                // refused the walk: nothing was cleared or written
                // (the first chunk rolled back). Put the stored status
                // back to what it was, and report the denial.
                drop(scan_cache);
                tracing::warn!(
                    source = %name,
                    root = %path.display(),
                    "source root can't be read (permission denied); keeping its cached rows"
                );
                self.set_permission_denied(name, true);
                let cache = self.cache.lock().unwrap();
                if status != SourceStatus::Stale {
                    retry_cache_on_busy(&self.gate, || {
                        cache.transition_source_status(name, status)
                    })?;
                }
                return Ok(ScanStats::default());
            }
            Err(e) => return Err(e.into()),
        };
        drop(scan_cache);
        self.set_permission_denied(name, false);

        tracing::info!(
            source = %name,
            source_id,
            files_seen = stats.files_seen,
            files_ignored = stats.files_ignored,
            units_written = stats.units_written,
            links_written = stats.links_written,
            tags_written = stats.tags_written,
            blocks_written = stats.blocks_written,
            "scan_complete"
        );

        {
            let cache = self.cache.lock().unwrap();
            retry_cache_on_busy(&self.gate, || {
                cache.transition_source_status(name, SourceStatus::Active)
            })?;
            retry_cache_on_busy(&self.gate, || cache.set_last_scanned(name, now_unix()))?;
        }

        Ok(stats)
    }

    /// What `block.ensure_id` needs to know about the file `rel_path` of
    /// `source`, from a fresh cache connection (it never takes the shared
    /// `self.cache` lock): the source's root and state, and the cached
    /// `units` row, if there is one. `Ok(None)` when no such source is
    /// registered.
    pub fn cached_unit(
        &self,
        source: &str,
        rel_path: &str,
    ) -> Result<Option<CachedUnit>, RegistryError> {
        let cache = Cache::open_read_only(&self.cache_path)?;
        let Some(row) = cache.get_source(source)? else {
            return Ok(None);
        };
        let unit = cache.get_unit_by_rel_path(row.id, rel_path)?;
        Ok(Some(CachedUnit {
            source_id: row.id,
            root: PathBuf::from(row.path),
            status: row.status,
            permission_denied: self.is_permission_denied(source),
            unit,
        }))
    }

    /// Re-indexes the one file `abs_path` of `source` now, in one
    /// transaction (the same path the watcher's debounced batch takes), so
    /// the cache describes the file as it is on disk before the caller
    /// returns. Announces the change on the event stream, exactly as a
    /// watcher batch would, so the graph refresh and the `graph:changed`
    /// push still happen even though the caller discards the watcher's own
    /// event for this write.
    pub fn reindex_file(&self, source: &str, abs_path: &Path) -> Result<(), RegistryError> {
        let changed_paths = self.reindex_quietly(source, abs_path)?;
        if !changed_paths.is_empty() {
            let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                source: source.to_string(),
                paths: changed_paths,
                fixes: Vec::new(),
            });
        }
        Ok(())
    }

    /// [`SourceRegistry::reindex_file`] without the event: the paths
    /// that were upserted or deleted, for a caller that announces them
    /// itself.
    fn reindex_quietly(
        &self,
        source: &str,
        abs_path: &Path,
    ) -> Result<Vec<PathBuf>, RegistryError> {
        self.reindex_paths(source, &[abs_path.to_path_buf()])
    }

    /// Brings the cache up to date with `paths` of `source` (files or
    /// folders, still there or gone) in one batch, as a watcher batch
    /// does, and announces nothing: the paths upserted or deleted, for a
    /// caller that announces them itself. What `link.rename_event` uses
    /// so the graph it fixes from already shows the rename.
    pub fn reindex_paths(
        &self,
        source: &str,
        paths: &[PathBuf],
    ) -> Result<Vec<PathBuf>, RegistryError> {
        let cache = Cache::open(&self.cache_path)?;
        let row = cache
            .get_source(source)?
            .ok_or_else(|| RegistryError::NotFound(source.to_string()))?;
        let root = PathBuf::from(&row.path);
        let ignore_rules = self.load_rules(source, &root)?;
        let changed: HashSet<PathBuf> = paths.iter().cloned().collect();
        Ok(scan::apply_change_batch(&cache, row.id, &root, &changed, &ignore_rules)?.changed_paths)
    }

    /// Drops the cached rows of the files at `rels` (source-relative,
    /// exactly as spelled) and of everything below them. For a name the
    /// disk no longer has but still answers to: on a case-insensitive
    /// disk `Plan.md` stats fine after a rename to `plan.md`, so
    /// [`SourceRegistry::reindex_paths`] would index the old spelling
    /// again instead of removing it.
    pub fn forget_spellings(&self, source: &str, rels: &[String]) -> Result<(), RegistryError> {
        let cache = Cache::open(&self.cache_path)?;
        let row = cache
            .get_source(source)?
            .ok_or_else(|| RegistryError::NotFound(source.to_string()))?;
        for rel in rels {
            if let Some(unit) = cache.get_unit_by_rel_path(row.id, rel)? {
                cache.delete_unit(unit.id)?;
            }
            cache.delete_units_under_prefix(row.id, rel)?;
        }
        Ok(())
    }
}

/// The fix engine's announcements (G17): every file it rewrote is indexed
/// again here (its watcher event is dropped as Tephra's own, so nothing
/// else would), then ONE event per source goes out, carrying the files
/// and the entries. A batch whose files and entries are all in one
/// source, which is every rename within one source, makes one event.
impl crate::fixes::FixSink for SourceRegistry {
    fn announce(&self, announcement: crate::fixes::FixAnnouncement) {
        use std::collections::BTreeMap;
        let mut by_source: BTreeMap<String, (Vec<PathBuf>, Vec<crate::fixes::FixNotice>)> =
            BTreeMap::new();
        for file in &announcement.written {
            match self.reindex_quietly(&file.source, &file.path) {
                Ok(paths) => {
                    let slot = by_source.entry(file.source.clone()).or_default();
                    for p in paths {
                        if !slot.0.contains(&p) {
                            slot.0.push(p);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(source = %file.source, path = %file.path.display(), error = %e, "could not index a file the fix rewrote");
                    // Still named: the file did change on disk.
                    let slot = by_source.entry(file.source.clone()).or_default();
                    if !slot.0.contains(&file.path) {
                        slot.0.push(file.path.clone());
                    }
                }
            }
        }
        for notice in announcement.notices {
            by_source
                .entry(notice.source.clone())
                .or_default()
                .1
                .push(notice);
        }
        for (source, (paths, fixes)) in by_source {
            let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                source,
                paths,
                fixes,
            });
        }
    }
}

/// [`SourceRegistry::cached_unit`]'s answer.
#[derive(Debug, Clone)]
pub struct CachedUnit {
    pub source_id: i64,
    /// The source's canonical root, as stored.
    pub root: PathBuf,
    /// The stored status (`active`, `stale` or `offline`).
    pub status: SourceStatus,
    /// The runtime overlay: the OS won't let `tephrad` list the root.
    pub permission_denied: bool,
    /// The cached row for the file; `None` for a path that is no file of
    /// this source (or was never indexed).
    pub unit: Option<tephra_cache::UnitRow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, root: &str) -> Source {
        Source {
            name: name.to_string(),
            root: PathBuf::from(root),
        }
    }

    // ---- register: path validity --------------------------------------

    fn promote_fixtures() -> Vec<Source> {
        vec![
            source("vault", "/home/dan/Vault"),
            source("tephra", "/home/dan/Developer/Tephra"),
            source("inner", "/home/dan/Developer/Tephra/crates/tephrad/inner"),
        ]
    }

    /// E13: every refusal, with its stable reason, in one table.
    #[test]
    fn validate_promote_refuses_each_case_with_its_reason() {
        let existing = promote_fixtures();
        let cases: &[(&str, &str, PathProbe, &str)] = &[
            // The folder is a registered root: a vault, or a promoted source.
            ("x", "/home/dan/Vault", PathProbe::Ok, "is_a_source_root"),
            (
                "x",
                "/home/dan/Developer/Tephra",
                PathProbe::Ok,
                "is_a_source_root",
            ),
            (
                "x",
                "/home/dan/Developer/Tephra/crates/tephrad/inner",
                PathProbe::Ok,
                "is_a_source_root",
            ),
            // In no source, and above one.
            (
                "x",
                "/home/dan/Documents/Notes",
                PathProbe::Ok,
                "not_inside_a_source",
            ),
            (
                "x",
                "/home/dan/Developer",
                PathProbe::Ok,
                "not_inside_a_source",
            ),
            ("x", "/", PathProbe::Ok, "not_inside_a_source"),
            // A file, or nothing there.
            (
                "x",
                "/home/dan/Vault/a.md",
                PathProbe::NotADirectory,
                "not_a_folder",
            ),
            (
                "x",
                "/home/dan/Vault/gone",
                PathProbe::Missing,
                "not_a_folder",
            ),
            (
                "x",
                "/home/dan/Vault/locked",
                PathProbe::NotReadable,
                "not_readable",
            ),
            // Names.
            ("", "/home/dan/Vault/notes", PathProbe::Ok, "invalid_name"),
            (
                "my notes",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "my/notes",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "@notes",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "a:b",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "a]b",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            ("..", "/home/dan/Vault/notes", PathProbe::Ok, "invalid_name"),
            (
                "tab\there",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "invalid_name",
            ),
            (
                "vault",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "name_taken",
            ),
            (
                "VAULT",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "name_taken",
            ),
            (
                "Tephra",
                "/home/dan/Vault/notes",
                PathProbe::Ok,
                "name_taken",
            ),
        ];
        for (name, root, probe, reason) in cases {
            let err = validate_promote(&existing, name, Path::new(root), *probe)
                .expect_err(&format!("{name:?} at {root:?} must be refused"));
            assert_eq!(err.reason(), *reason, "{name:?} at {root:?}: {err}");
        }
    }

    #[test]
    fn validate_promote_allows_any_depth_and_names_the_source_that_gives_the_folder_up() {
        let existing = promote_fixtures();
        let ok = |name: &str, root: &str| {
            validate_promote(&existing, name, Path::new(root), PathProbe::Ok)
        };
        // A folder in a vault, at any depth, and a source's own top folder.
        assert_eq!(ok("notes", "/home/dan/Vault/Notes").unwrap(), "vault");
        assert_eq!(ok("deep", "/home/dan/Vault/a/b/c/d").unwrap(), "vault");
        assert_eq!(
            ok("tephrad", "/home/dan/Developer/Tephra/crates/tephrad").unwrap(),
            "tephra"
        );
        // A multi-byte folder name, and a name that is not ASCII.
        assert_eq!(ok("café", "/home/dan/Vault/café notes").unwrap(), "vault");
        // E14: a folder that holds a promoted source is allowed; the inner
        // source stays its own.
        assert_eq!(
            ok("outer", "/home/dan/Developer/Tephra/crates").unwrap(),
            "tephra"
        );
        // A folder inside the promoted source belongs to that source.
        assert_eq!(
            ok("sub", "/home/dan/Developer/Tephra/crates/tephrad/inner/src").unwrap(),
            "inner"
        );
        // A sibling whose name only starts like a root is not inside it.
        assert_eq!(
            validate_promote(
                &existing,
                "x",
                Path::new("/home/dan/Vault2/notes"),
                PathProbe::Ok
            )
            .unwrap_err()
            .reason(),
            "not_inside_a_source"
        );
    }

    #[test]
    fn a_refused_promote_carries_its_reason_and_a_plain_message() {
        let existing = promote_fixtures();
        let err = validate_promote(&existing, "x", Path::new("/home/dan/Vault"), PathProbe::Ok)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "/home/dan/Vault is already the root of the source \"vault\""
        );
        let err = validate_promote(
            &existing,
            "VAULT",
            Path::new("/home/dan/Vault/n"),
            PathProbe::Ok,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the name \"VAULT\" is taken by the source \"vault\""
        );
    }

    /// F10: `register` is untouched by promoted sources: a root nested in,
    /// or above, an existing one is still refused, whatever else exists.
    #[test]
    fn register_still_refuses_nested_and_ancestor_roots_when_promoted_roots_exist() {
        let existing = promote_fixtures();
        let nested = validate_register(
            &existing,
            "n",
            Path::new("/home/dan/Developer/Tephra/docs"),
            PathProbe::Ok,
        )
        .unwrap_err();
        assert!(matches!(
            nested,
            RegisterError::Conflict {
                reason: ConflictReason::NestedInsideExisting,
                ..
            }
        ));
        let above = validate_register(
            &existing,
            "a",
            Path::new("/home/dan/Developer"),
            PathProbe::Ok,
        )
        .unwrap_err();
        assert!(matches!(
            above,
            RegisterError::Conflict {
                reason: ConflictReason::ExistingIsNestedInside,
                ..
            }
        ));
    }

    #[test]
    fn register_rejects_a_missing_path() {
        let existing: Vec<Source> = vec![];
        let result = validate_register(
            &existing,
            "notes",
            Path::new("/no/such/path"),
            PathProbe::Missing,
        );
        assert_eq!(
            result,
            Err(RegisterError::InvalidPath {
                path: PathBuf::from("/no/such/path"),
                reason: PathProbe::Missing,
            })
        );
    }

    #[test]
    fn register_rejects_a_path_that_is_not_a_directory() {
        let existing: Vec<Source> = vec![];
        let result = validate_register(
            &existing,
            "notes",
            Path::new("/some/file.txt"),
            PathProbe::NotADirectory,
        );
        assert!(matches!(
            result,
            Err(RegisterError::InvalidPath {
                reason: PathProbe::NotADirectory,
                ..
            })
        ));
    }

    #[test]
    fn register_rejects_an_unreadable_path() {
        let existing: Vec<Source> = vec![];
        let result = validate_register(
            &existing,
            "notes",
            Path::new("/no/permission"),
            PathProbe::NotReadable,
        );
        assert!(matches!(
            result,
            Err(RegisterError::InvalidPath {
                reason: PathProbe::NotReadable,
                ..
            })
        ));
    }

    #[test]
    fn register_accepts_a_valid_path_with_no_conflicts() {
        let existing = vec![source("other", "/unrelated/root")];
        let result =
            validate_register(&existing, "notes", Path::new("/vault/notes"), PathProbe::Ok);
        assert_eq!(result, Ok(()));
    }

    // ---- register: name/path conflicts --------------------------------

    #[test]
    fn register_rejects_a_duplicate_name() {
        let existing = vec![source("notes", "/vault/notes")];
        let result =
            validate_register(&existing, "notes", Path::new("/vault/other"), PathProbe::Ok);
        assert_eq!(
            result,
            Err(RegisterError::DuplicateName("notes".to_string()))
        );
    }

    #[test]
    fn register_rejects_an_exact_duplicate_root() {
        let existing = vec![source("notes", "/vault/notes")];
        let result = validate_register(
            &existing,
            "notes2",
            Path::new("/vault/notes"),
            PathProbe::Ok,
        );
        assert_eq!(
            result,
            Err(RegisterError::Conflict {
                with: "notes".to_string(),
                reason: ConflictReason::SameRoot,
            })
        );
    }

    #[test]
    fn register_rejects_a_root_nested_inside_an_existing_source() {
        let existing = vec![source("notes", "/vault/notes")];
        let result = validate_register(
            &existing,
            "subnotes",
            Path::new("/vault/notes/sub"),
            PathProbe::Ok,
        );
        assert_eq!(
            result,
            Err(RegisterError::Conflict {
                with: "notes".to_string(),
                reason: ConflictReason::NestedInsideExisting,
            })
        );
    }

    #[test]
    fn register_rejects_a_root_that_is_an_ancestor_of_an_existing_source() {
        let existing = vec![source("subnotes", "/vault/notes/sub")];
        let result =
            validate_register(&existing, "notes", Path::new("/vault/notes"), PathProbe::Ok);
        assert_eq!(
            result,
            Err(RegisterError::Conflict {
                with: "subnotes".to_string(),
                reason: ConflictReason::ExistingIsNestedInside,
            })
        );
    }

    #[test]
    fn register_allows_sibling_directories_with_similar_names() {
        // "/vault/notes" vs "/vault/notes2": a naive string-prefix
        // check would falsely treat these as nested; `Path::starts_with`
        // is component-aware and must not.
        let existing = vec![source("notes", "/vault/notes")];
        let result = validate_register(
            &existing,
            "notes2",
            Path::new("/vault/notes2"),
            PathProbe::Ok,
        );
        assert_eq!(result, Ok(()));
    }

    // ---- E13: the Full Disk Access line on a privacy-blocked path -----

    #[test]
    fn register_error_for_a_privacy_blocked_path_carries_the_full_disk_access_line() {
        let existing: Vec<Source> = vec![];
        let err = validate_register(
            &existing,
            "notes",
            Path::new("/Users/dan/Documents/Notes"),
            PathProbe::NotReadablePrivacy,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("NotReadable"), "{message}");
        assert!(
            message.contains(&crate::access::fix_line("/Users/dan/Documents/Notes")),
            "{message}"
        );
    }

    #[test]
    fn register_error_for_other_unreadable_paths_keeps_the_plain_message() {
        let existing: Vec<Source> = vec![];
        let err = validate_register(
            &existing,
            "notes",
            Path::new("/no/permission"),
            PathProbe::NotReadable,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "path \"/no/permission\" is invalid: NotReadable"
        );
    }

    #[test]
    fn only_the_two_unreadable_probes_count_as_permission_denied() {
        assert!(PathProbe::NotReadable.is_permission_denied());
        assert!(PathProbe::NotReadablePrivacy.is_permission_denied());
        assert!(!PathProbe::Ok.is_permission_denied());
        assert!(!PathProbe::Missing.is_permission_denied());
        assert!(!PathProbe::NotADirectory.is_permission_denied());
    }

    // ---- state transitions (reusing tephra-cache's validated edges) ---

    #[test]
    fn rescan_transition_sequence_is_active_to_stale_to_active() {
        use tephra_cache::validate_transition;
        assert_eq!(
            validate_transition(SourceStatus::Active, SourceStatus::Stale),
            Ok(())
        );
        assert_eq!(
            validate_transition(SourceStatus::Stale, SourceStatus::Active),
            Ok(())
        );
    }

    #[test]
    fn offline_remount_sequence_is_offline_to_stale_to_active_never_direct() {
        use tephra_cache::validate_transition;
        assert_eq!(
            validate_transition(SourceStatus::Offline, SourceStatus::Stale),
            Ok(())
        );
        assert!(validate_transition(SourceStatus::Offline, SourceStatus::Active).is_err());
    }

    #[test]
    fn a_running_source_going_missing_transitions_to_offline_from_either_state() {
        use tephra_cache::validate_transition;
        assert_eq!(
            validate_transition(SourceStatus::Active, SourceStatus::Offline),
            Ok(())
        );
        assert_eq!(
            validate_transition(SourceStatus::Stale, SourceStatus::Offline),
            Ok(())
        );
    }

    #[test]
    fn unregister_then_register_the_same_name_again_is_allowed() {
        // Once a name is removed from the in-memory existing-sources
        // view, registering it again must validate clean.
        let mut existing = vec![source("notes", "/vault/notes")];
        existing.retain(|s| s.name != "notes");
        let result =
            validate_register(&existing, "notes", Path::new("/vault/notes"), PathProbe::Ok);
        assert_eq!(result, Ok(()));
    }

    // ---- R1: duplicate/nested-root rejection survives non-canonical
    // spellings of an already-registered root -------------------------

    #[test]
    fn register_rejects_a_symlink_to_an_already_registered_root() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        std::fs::create_dir(&real_root).unwrap();
        let canonical_real_root = real_root.canonicalize().unwrap();

        let existing = vec![source("notes", canonical_real_root.to_str().unwrap())];

        let symlink_path = dir.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_root, &symlink_path).unwrap();
        #[cfg(not(unix))]
        std::os::windows::fs::symlink_dir(&real_root, &symlink_path).unwrap();

        // The caller canonicalizes the candidate the same way
        // `SourceRegistry::register` does before calling
        // `validate_register` (R1): a symlink to an already-registered
        // root canonicalizes to the identical path and so must be
        // rejected as a duplicate, not accepted as a distinct root.
        let canonical_candidate = symlink_path.canonicalize().unwrap();
        let result = validate_register(&existing, "alias", &canonical_candidate, PathProbe::Ok);
        assert_eq!(
            result,
            Err(RegisterError::Conflict {
                with: "notes".to_string(),
                reason: ConflictReason::SameRoot,
            })
        );
    }

    #[test]
    fn canonicalizing_before_validation_collapses_differently_spelled_duplicate_roots() {
        // Simulates macOS's `/var` -> `/private/var` symlink: a
        // non-canonical candidate spelling of an already-registered
        // canonical root must be caught once canonicalized, the way
        // `SourceRegistry::register` canonicalizes before validating.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir(&root).unwrap();
        let canonical_root = root.canonicalize().unwrap();

        let existing = vec![source("notes", canonical_root.to_str().unwrap())];

        // A non-canonical spelling that still resolves to the exact
        // same directory (e.g. a trailing "." component).
        let non_canonical_candidate = root.join(".");
        let canonicalized_candidate = non_canonical_candidate.canonicalize().unwrap();
        assert_eq!(canonicalized_candidate, canonical_root);

        let result = validate_register(&existing, "dup", &canonicalized_candidate, PathProbe::Ok);
        assert_eq!(
            result,
            Err(RegisterError::Conflict {
                with: "notes".to_string(),
                reason: ConflictReason::SameRoot,
            })
        );
    }

    // ---- retry_cache_on_busy (R16) -------------------------------------

    /// A `SQLITE_BUSY` [`CacheError`], the exact shape
    /// `retry_cache_on_busy` and `is_cache_busy` classify as
    /// retryable — built without any real database or lock
    /// contention, so these tests are fast and deterministic (unlike
    /// `tests/register_busy_retry.rs`'s real-lock integration test,
    /// which proves the same helper is actually wired into
    /// `register()`'s call site against a genuine busy connection).
    fn busy_error() -> CacheError {
        CacheError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ))
    }

    #[test]
    fn retry_cache_on_busy_succeeds_once_the_operation_stops_failing() {
        let gate = WriterGate::new();
        let mut calls = 0u32;
        let result: Result<&str, CacheError> = retry_cache_on_busy(&gate, || {
            calls += 1;
            if calls < 4 {
                Err(busy_error())
            } else {
                Ok("done")
            }
        });
        assert!(matches!(result, Ok("done")));
        assert_eq!(
            calls, 4,
            "must retry exactly as many times as needed, no more"
        );
    }

    #[test]
    fn retry_cache_on_busy_gives_up_after_the_retry_budget_is_exhausted() {
        let gate = WriterGate::new();
        let mut calls = 0u32;
        let result: Result<(), CacheError> = retry_cache_on_busy(&gate, || {
            calls += 1;
            Err(busy_error())
        });
        assert!(
            matches!(result, Err(e) if is_cache_busy(&e)),
            "must surface the busy error once retries run out, not hang forever"
        );
        assert_eq!(
            calls,
            MAX_CACHE_BUSY_RETRIES + 1,
            "one initial attempt plus exactly MAX_CACHE_BUSY_RETRIES retries"
        );
    }

    #[test]
    fn retry_cache_on_busy_does_not_retry_a_non_busy_error() {
        let gate = WriterGate::new();
        let mut calls = 0u32;
        let result: Result<(), CacheError> = retry_cache_on_busy(&gate, || {
            calls += 1;
            Err(CacheError::DuplicateSource("notes".to_string()))
        });
        assert!(matches!(result, Err(CacheError::DuplicateSource(n)) if n == "notes"));
        assert_eq!(
            calls, 1,
            "a non-busy error must be returned immediately on the first attempt"
        );
    }

    // ---- start_all: behind the writer gate ---------------------------

    /// G5: the catch-up's write is one of the gate's writes, so an RPC
    /// write that is already holding the gate is not overtaken: the
    /// catch-up waits for it, then goes ahead.
    #[test]
    fn start_all_writes_behind_the_writer_gate() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.md"), "# A\n").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().join("cache.db");
        {
            let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
            registry.register("notes", root.path()).unwrap();
        }
        std::fs::write(root.path().join("a.md"), "# A\n\nedited while down\n").unwrap();
        let registry = Arc::new(SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap());

        let gate = writer_gate::for_path(&cache_path);
        let held = gate.acquire();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (registry, done) = (registry.clone(), done.clone());
            std::thread::spawn(move || {
                let outcomes = registry.start_all(|| false);
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                outcomes
            })
        };

        // A bounded negative wait: the catch-up must still be waiting.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "the catch-up wrote while another writer held the gate"
        );
        drop(held);
        let outcomes = thread.join().unwrap();
        assert!(
            matches!(outcomes[0].1, BootOutcome::CaughtUp { paths: 1 }),
            "{outcomes:?}"
        );
        registry.unregister("notes").unwrap();
    }

    // ---- the source-list push (G16, FR12) -----------------------------

    fn change_kinds() -> Vec<(SourceChange, serde_json::Value)> {
        use serde_json::json;
        vec![
            (
                SourceChange::new(SourceChangeKind::Added, "notes"),
                json!({"kind": "added", "name": "notes"}),
            ),
            (
                SourceChange::new(SourceChangeKind::Removed, "notes"),
                json!({"kind": "removed", "name": "notes"}),
            ),
            (
                SourceChange::renamed("notes", "journal"),
                json!({"kind": "renamed", "name": "journal", "old_name": "notes"}),
            ),
            (
                SourceChange::new(SourceChangeKind::Promoted, "docs"),
                json!({"kind": "promoted", "name": "docs"}),
            ),
            (
                SourceChange::new(SourceChangeKind::Unpromoted, "docs"),
                json!({"kind": "unpromoted", "name": "docs"}),
            ),
        ]
    }

    #[test]
    fn every_kind_builds_exact_json_and_only_a_rename_has_old_name() {
        for (change, want) in change_kinds() {
            assert_eq!(change.to_json(), want, "{change:?}");
        }
    }

    #[test]
    fn every_kind_builds_one_graph_changed_payload_keeping_source_and_empty_paths() {
        use serde_json::json;
        for (change, want) in change_kinds() {
            let name = change.name.clone();
            let payload = crate::forward::payload(&DaemonEvent::SourceListChanged {
                changes: vec![change],
                fixes: Vec::new(),
            });
            assert_eq!(
                payload,
                json!({"source": name, "paths": [], "sources_changed": [want]}),
            );
        }
        // Several changes in one push keep their order.
        let payload = crate::forward::payload(&DaemonEvent::SourceListChanged {
            changes: change_kinds().into_iter().map(|(c, _)| c).collect(),
            fixes: Vec::new(),
        });
        assert_eq!(payload["sources_changed"].as_array().unwrap().len(), 5);
        assert_eq!(payload["sources_changed"][2]["old_name"], "notes");
    }

    fn registry_in(dir: &Path) -> SourceRegistry {
        SourceRegistry::new(Cache::open(dir.join("cache.db")).unwrap()).unwrap()
    }

    fn source_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, text) in files {
            std::fs::write(dir.path().join(rel), text).unwrap();
        }
        dir
    }

    /// Every source-list event waiting on `rx`.
    fn list_events(
        rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    ) -> Vec<Vec<SourceChange>> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DaemonEvent::SourceListChanged { changes, .. } = event {
                out.push(changes);
            }
        }
        out
    }

    #[test]
    fn register_and_unregister_each_send_exactly_one_event_naming_the_source() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n"), ("b.md", "[[a]]\n"), ("c.md", "# C\n")]);
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();

        registry.register("notes", src.path()).unwrap();
        assert_eq!(
            list_events(&mut rx),
            vec![vec![SourceChange::new(SourceChangeKind::Added, "notes")]]
        );

        registry.unregister("notes").unwrap();
        assert_eq!(
            list_events(&mut rx),
            vec![vec![SourceChange::new(SourceChangeKind::Removed, "notes")]]
        );
    }

    #[test]
    fn the_event_comes_after_the_scan_and_the_row_so_a_requery_sees_the_change() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();

        registry.register("notes", src.path()).unwrap();
        // The event is already waiting when `register` returns, and what
        // it announces is already true: listed, and scanned.
        assert_eq!(list_events(&mut rx).len(), 1);
        let listed = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(registry.is_watching("notes"));
        let verify = Cache::open(cache_dir.path().join("cache.db")).unwrap();
        assert_eq!(
            verify.units_with_status_for_source("notes").unwrap().len(),
            2
        );

        registry.unregister("notes").unwrap();
        assert_eq!(list_events(&mut rx).len(), 1);
        assert!(registry.list().unwrap().is_empty());
        assert!(!registry.is_watching("notes"));
    }

    #[test]
    fn a_register_that_fails_validation_sends_no_event() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        let nested = src.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        let other = source_dir(&[("o.md", "# O\n")]);
        let a_file = src.path().join("a.md");
        let registry = registry_in(cache_dir.path());
        registry.register("notes", src.path()).unwrap();
        let mut rx = registry.subscribe();

        // Missing path, not a folder, duplicate name, duplicate root,
        // nested root, ancestor root.
        assert!(registry.register("x", "/no/such/path/for/tephra").is_err());
        assert!(registry.register("x", &a_file).is_err());
        assert!(registry.register("notes", other.path()).is_err());
        assert!(registry.register("again", src.path()).is_err());
        assert!(registry.register("inner", &nested).is_err());
        let parent = src.path().parent().unwrap();
        assert!(registry.register("outer", parent).is_err());

        assert!(list_events(&mut rx).is_empty());
        assert_eq!(registry.list().unwrap().len(), 1);
        registry.unregister("notes").unwrap();
    }

    #[test]
    fn a_register_whose_scan_fails_rolls_back_and_sends_no_added_event() {
        let cache_dir = tempfile::tempdir().unwrap();
        // A `.tephraignore` that is a folder cannot be read: the row has
        // been inserted by then, so this takes the rollback path.
        let src = source_dir(&[("a.md", "# A\n")]);
        std::fs::create_dir(src.path().join(".tephraignore")).unwrap();
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();

        assert!(registry.register("notes", src.path()).is_err());

        assert!(list_events(&mut rx).is_empty());
        assert!(
            registry.list().unwrap().is_empty(),
            "the row was rolled back"
        );
    }

    #[test]
    fn unregistering_a_name_that_is_not_registered_is_ok_and_sends_nothing() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();

        registry.unregister("ghost").unwrap();
        assert!(list_events(&mut rx).is_empty());

        // Twice on a source that was removed: one event in all.
        registry.register("notes", src.path()).unwrap();
        registry.unregister("notes").unwrap();
        registry.unregister("notes").unwrap();
        let events = list_events(&mut rx);
        assert_eq!(events.len(), 2, "one added and one removed: {events:?}");
        assert_eq!(events[1][0].kind, SourceChangeKind::Removed);
    }

    #[test]
    fn the_emitter_accepts_renamed_promoted_and_unpromoted_and_ignores_an_empty_list() {
        let cache_dir = tempfile::tempdir().unwrap();
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();

        registry.announce_sources_changed(Vec::new());
        assert!(list_events(&mut rx).is_empty());

        for (change, _) in change_kinds() {
            registry.announce_sources_changed(vec![change.clone()]);
            assert_eq!(list_events(&mut rx), vec![vec![change]]);
        }
    }

    #[test]
    fn announcing_with_nobody_subscribed_is_dropped_without_error() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        let registry = registry_in(cache_dir.path());
        // No `subscribe()` anywhere: register and unregister still succeed.
        registry.register("notes", src.path()).unwrap();
        registry.unregister("notes").unwrap();
        // A receiver that joins later hears nothing of what was dropped.
        let mut late = registry.subscribe();
        assert!(list_events(&mut late).is_empty());
    }

    #[test]
    fn a_restart_and_the_catch_up_announce_no_source_as_added() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        {
            let registry = registry_in(cache_dir.path());
            registry.register("notes", src.path()).unwrap();
            registry.unregister("notes").unwrap();
            registry.register("notes", src.path()).unwrap();
        }
        // The daemon restarts: a new registry over the same cache.
        let registry = registry_in(cache_dir.path());
        let mut rx = registry.subscribe();
        registry.start_all(|| false);
        assert!(list_events(&mut rx).is_empty());
        registry.unregister("notes").unwrap();
    }

    #[test]
    fn a_rescan_and_a_file_change_are_not_list_changes() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        let registry = registry_in(cache_dir.path());
        registry.register("notes", src.path()).unwrap();
        let mut rx = registry.subscribe();

        std::fs::write(src.path().join("new.md"), "# New\n").unwrap();
        registry.rescan("notes").unwrap();

        assert!(list_events(&mut rx).is_empty());
        registry.unregister("notes").unwrap();
    }

    #[test]
    fn a_batch_still_running_when_its_source_is_unregistered_sends_nothing_after_removed() {
        let cache_dir = tempfile::tempdir().unwrap();
        let src = source_dir(&[("a.md", "# A\n")]);
        let registry = registry_in(cache_dir.path());
        registry.register("notes", src.path()).unwrap();
        // What a batch captured before the unregister: the live flag.
        let live = Arc::clone(&registry.running.lock().unwrap().get("notes").unwrap().live);
        let mut rx = registry.subscribe();
        let stale = || DaemonEvent::SourceChanged {
            source: "notes".into(),
            paths: vec![src.path().join("a.md")],
            fixes: Vec::new(),
        };

        // Before: the batch's event goes out.
        send_if_live(&live, &registry.events_tx, stale());
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonEvent::SourceChanged { .. })
        ));

        registry.unregister("notes").unwrap();
        // After: `removed` is the last event for the name.
        send_if_live(&live, &registry.events_tx, stale());
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        assert_eq!(
            seen,
            vec![DaemonEvent::SourceListChanged {
                changes: vec![SourceChange::new(SourceChangeKind::Removed, "notes")],
                fixes: Vec::new(),
            }]
        );
    }
}
