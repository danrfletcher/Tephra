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
//!   together. Exercised by the integration tests under `tests/`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tephra_cache::{Cache, CacheError, SourceRow, SourceStatus};

use crate::ignore::IgnoreRules;
use crate::scan::{self, ScanError, ScanStats};
use crate::watcher::{self, WatchError, WatcherHandle};

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
/// than reimplementing change detection itself.
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
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The live orchestrator: owns the shared `Cache`, a dedicated tokio
/// runtime for watcher tasks (independent of whatever runtime a
/// caller/test happens to be in), and the map of currently-running
/// per-source watchers.
///
/// Every source's watcher task, `notify` watcher instance, and ignore
/// evaluation are independent of every other source's (GP4): the only
/// thing ever shared across sources is the single `Cache` connection
/// (guarded by a mutex) and the broadcast event channel, and every
/// cache operation is scoped by `source_id` — one source's register,
/// rescan, or crash never touches another's rows or watcher.
pub struct SourceRegistry {
    cache: Arc<Mutex<Cache>>,
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
        Ok(SourceRegistry {
            cache: Arc::new(Mutex::new(cache)),
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

    fn existing_sources(&self) -> Result<Vec<Source>, RegistryError> {
        let cache = self.cache.lock().unwrap();
        Ok(cache
            .list_sources()?
            .into_iter()
            .map(|r| Source {
                name: r.name,
                root: PathBuf::from(r.path),
            })
            .collect())
    }

    /// `source.register` (G1/GP2/GP3): validates the candidate root,
    /// registers it in the cache, runs its initial full scan
    /// synchronously (so the returned [`ScanStats`] reflect a
    /// genuinely complete scan, not an in-flight one), then starts its
    /// dedicated watcher task.
    pub fn register(&self, name: &str, root: impl AsRef<Path>) -> Result<ScanStats, RegistryError> {
        let root = root.as_ref();
        let probe = probe_path(root);
        let existing = self.existing_sources()?;
        validate_register(&existing, name, root, probe)?;

        // Canonicalize once validation has confirmed the path exists
        // and is a readable directory, so the stored root and every
        // subsequent nested/ancestor check for later registrations
        // compare resolved paths (no trailing-slash or `..`
        // false-negatives).
        let canonical_root = root.canonicalize()?;
        let ignore_rules = IgnoreRules::load(&canonical_root)?;

        let source_id = {
            let cache = self.cache.lock().unwrap();
            cache.register_source(
                name,
                &canonical_root.to_string_lossy(),
                SourceStatus::Active,
            )?
        };

        let stats = {
            let cache = self.cache.lock().unwrap();
            scan::scan_source(&cache, source_id, &canonical_root, &ignore_rules)?
        };

        {
            let cache = self.cache.lock().unwrap();
            cache.set_last_scanned(name, now_unix())?;
        }

        self.start_watcher(name.to_string(), source_id, canonical_root)?;

        Ok(stats)
    }

    fn start_watcher(
        &self,
        name: String,
        source_id: i64,
        root: PathBuf,
    ) -> Result<(), RegistryError> {
        let cache_for_batch = Arc::clone(&self.cache);
        let events_tx = self.events_tx.clone();
        let name_for_batch = name.clone();
        let root_for_batch = root.clone();

        let on_batch = move |changed: std::collections::HashSet<PathBuf>| {
            let ignore_rules = IgnoreRules::load(&root_for_batch).unwrap_or_default();
            let cache = cache_for_batch.lock().unwrap();
            match scan::apply_change_batch(
                &cache,
                source_id,
                &root_for_batch,
                &changed,
                &ignore_rules,
            ) {
                Ok(_) => {
                    let _ = events_tx.send(DaemonEvent::SourceChanged {
                        source: name_for_batch.clone(),
                        paths: changed.into_iter().collect(),
                    });
                }
                Err(e) => {
                    tracing::warn!(source = %name_for_batch, error = %e, "failed to apply change batch");
                }
            }
        };

        let cache_for_offline = Arc::clone(&self.cache);
        let name_for_offline = name.clone();
        let on_root_removed = move || {
            let cache = cache_for_offline.lock().unwrap();
            if let Err(e) = cache.transition_source_status(&name_for_offline, SourceStatus::Offline)
            {
                tracing::warn!(source = %name_for_offline, error = %e, "failed to mark source offline");
            } else {
                tracing::warn!(source = %name_for_offline, "source root is missing; marked offline");
            }
        };

        let handle = watcher::spawn(
            root,
            self.runtime.handle().clone(),
            on_batch,
            on_root_removed,
        )?;
        self.running
            .lock()
            .unwrap()
            .insert(name, RunningSource { handle });
        Ok(())
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
        cache.delete_source(name)?;
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
                cache.transition_source_status(name, SourceStatus::Offline)?;
            }
            return Ok(ScanStats::default());
        }

        {
            let cache = self.cache.lock().unwrap();
            if status != SourceStatus::Stale {
                cache.transition_source_status(name, SourceStatus::Stale)?;
            }
        }

        let ignore_rules = IgnoreRules::load(&root)?;
        let stats = {
            let cache = self.cache.lock().unwrap();
            cache.delete_units_for_source(source_id)?;
            scan::scan_source(&cache, source_id, &root, &ignore_rules)?
        };

        {
            let cache = self.cache.lock().unwrap();
            cache.transition_source_status(name, SourceStatus::Active)?;
            cache.set_last_scanned(name, now_unix())?;
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

    // ---- list/unregister semantics on the pure `Source` fixture -------

    #[test]
    fn list_returns_every_registered_source_unmodified() {
        let sources = [
            source("notes", "/vault/notes"),
            source("repo", "/code/repo"),
        ];
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|s| s.name == "notes"));
        assert!(sources.iter().any(|s| s.name == "repo"));
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
}
