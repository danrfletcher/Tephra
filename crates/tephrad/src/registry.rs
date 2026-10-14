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

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tephra_cache::{Cache, CacheError, SourceRow, SourceStatus};

use crate::ignore::IgnoreRules;
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
    NotReadable,
}

/// Real-filesystem [`PathProbe`]: does `path` exist, is it a
/// directory, and can its contents actually be listed. Used by
/// [`SourceRegistry::register`]; unit tests below use `PathProbe`
/// values directly instead of calling this.
pub fn probe_path(path: &Path) -> PathProbe {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_dir() => PathProbe::NotADirectory,
        Ok(_) => match std::fs::read_dir(path) {
            Ok(_) => PathProbe::Ok,
            Err(_) => PathProbe::NotReadable,
        },
        Err(_) => PathProbe::Missing,
    }
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

// ---- orchestration ----------------------------------------------------

/// One internal `source_changed` event (GP6) — the seam a later
/// broadcaster (PR-6's JSON-RPC/WebSocket layer) subscribes to rather
/// than reimplementing change detection itself. `paths` only ever
/// contains absolute paths that were actually upserted or deleted in
/// the cache (R5) — never a raw, unfiltered watcher event batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    SourceChanged { source: String, paths: Vec<PathBuf> },
}

#[derive(Debug)]
pub enum RegistryError {
    Validation(RegisterError),
    Cache(CacheError),
    Io(std::io::Error),
    Watch(WatchError),
    Scan(ScanError),
    NotFound(String),
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
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<RegisterError> for RegistryError {
    fn from(e: RegisterError) -> Self {
        RegistryError::Validation(e)
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
fn retry_cache_on_busy<T>(
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
}

impl SourceRegistry {
    pub fn new(cache: Cache) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (events_tx, _rx) = tokio::sync::broadcast::channel(1024);
        let cache_path = cache.path().to_path_buf();
        let gate = writer_gate::for_path(&cache_path);
        Ok(SourceRegistry {
            cache: Arc::new(Mutex::new(cache)),
            cache_path,
            gate,
            runtime,
            running: Mutex::new(HashMap::new()),
            events_tx,
        })
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

    /// The rest of `register`'s work once the source row exists:
    /// initial scan, then start the watcher. Split out so `register`
    /// can roll the row back on any failure from this point on (R2).
    fn scan_and_watch(
        &self,
        name: &str,
        source_id: i64,
        root: &Path,
    ) -> Result<ScanStats, RegistryError> {
        let ignore_rules = Arc::new(RwLock::new(IgnoreRules::load(root)?));

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

    fn start_watcher(
        &self,
        name: String,
        source_id: i64,
        root: PathBuf,
        ignore_rules: Arc<RwLock<IgnoreRules>>,
        tap: watcher::WatcherTap,
    ) -> Result<(), RegistryError> {
        // A dedicated connection, guarded by its own mutex, for this
        // source's debounced batch-apply path (R8): independent of
        // `self.cache` and of every other source's own dedicated
        // connection, so one source's batch work can never block
        // another's, nor a `list`/`register`/`rescan` call against
        // the shared registry cache.
        let batch_cache = Arc::new(Mutex::new(Cache::open(&self.cache_path)?));
        let events_tx = self.events_tx.clone();
        let source_name = name.clone();
        let source_root = root.clone();
        let blocking_handle = self.runtime.handle().clone();
        let ignore_for_batch = Arc::clone(&ignore_rules);

        let on_batch = move |changed: std::collections::HashSet<PathBuf>| {
            tracing::info!(
                source = %source_name,
                batch_size = changed.len(),
                "debounce_fired"
            );

            let batch_cache = Arc::clone(&batch_cache);
            let events_tx = events_tx.clone();
            let source_name = source_name.clone();
            let source_root = source_root.clone();
            let ignore_for_batch = Arc::clone(&ignore_for_batch);

            // Offloaded to tokio's dedicated blocking thread pool
            // (R8): this closure is invoked synchronously from the
            // per-source debounce task, and the work inside it
            // (genuinely blocking SQLite writes) must never occupy
            // one of the daemon's fixed async worker threads for the
            // whole batch — doing so would stall every other
            // source's watcher sharing that runtime.
            let join_handle = blocking_handle.spawn_blocking(move || {
                // R12: read the *current* shared rules, not a
                // snapshot reloaded from disk independently of the
                // watcher's own event filter — a rescan writes its
                // freshly reloaded rules into this same lock, so the
                // batch-apply path and the event filter always agree
                // on what's ignored.
                let ignore_rules = ignore_for_batch.read().unwrap().clone();
                let cache = batch_cache.lock().unwrap();
                match scan::apply_change_batch(&cache, source_id, &source_root, &changed, &ignore_rules) {
                    Ok(outcome) => {
                        // R5: report only the paths that were
                        // actually upserted or deleted, and skip the
                        // event entirely when nothing really changed
                        // (e.g. a batch made entirely of
                        // ignored/directory-only paths that the
                        // watcher-level filter let through anyway).
                        if !outcome.changed_paths.is_empty() {
                            tracing::info!(
                                source = %source_name,
                                paths = ?outcome.changed_paths,
                                "source_changed"
                            );
                            let _ = events_tx.send(DaemonEvent::SourceChanged {
                                source: source_name,
                                paths: outcome.changed_paths,
                            });
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
        let on_root_removed = move || {
            let cache = cache_for_offline.lock().unwrap();
            if let Err(e) = retry_cache_on_busy(&gate_for_offline, || {
                cache.transition_source_status(&name_for_offline, SourceStatus::Offline)
            }) {
                tracing::warn!(source = %name_for_offline, error = %e, "failed to mark source offline");
            } else {
                tracing::warn!(source = %name_for_offline, "source root is missing; marked offline");
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
            },
        );
        Ok(())
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
    /// batch per source. Idempotent.
    pub fn resume_all_watchers(&self) {
        for running in self.running.lock().unwrap().values() {
            running.pause.resume();
        }
    }

    /// `source.unregister` (G1): stops the watcher task (if one is
    /// running — it may not be, e.g. for a source whose watcher
    /// failed to start) and removes the source's rows from the cache.
    /// Idempotent: calling this twice on an already-removed source
    /// returns `Ok(())` both times rather than erroring.
    pub fn unregister(&self, name: &str) -> Result<(), RegistryError> {
        if let Some(running) = self.running.lock().unwrap().remove(name) {
            running.handle.stop();
        }
        let cache = self.cache.lock().unwrap();
        retry_cache_on_busy(&self.gate, || cache.delete_source(name))?;
        Ok(())
    }

    /// `source.list` (G1).
    pub fn list(&self) -> Result<Vec<SourceRow>, RegistryError> {
        let cache = self.cache.lock().unwrap();
        Ok(cache.list_sources()?)
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
    pub fn rescan(&self, name: &str) -> Result<ScanStats, RegistryError> {
        let (source_id, root, status) = {
            let cache = self.cache.lock().unwrap();
            let row = cache
                .get_source(name)?
                .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
            (row.id, PathBuf::from(row.path), row.status)
        };

        if probe_path(&root) != PathProbe::Ok {
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

        let ignore_rules = IgnoreRules::load(&root)?;

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
        let stats = scan::rescan_source(&scan_cache, source_id, &root, &ignore_rules)?;
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
            retry_cache_on_busy(&self.gate, || {
                cache.transition_source_status(name, SourceStatus::Active)
            })?;
            retry_cache_on_busy(&self.gate, || cache.set_last_scanned(name, now_unix()))?;
        }

        Ok(stats)
    }
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
}
