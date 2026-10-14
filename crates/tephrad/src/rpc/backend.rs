//! The two backend seams the RPC dispatch table (`dispatch.rs`) calls
//! through, rather than depending directly on `tephra_graph::UnitGraph`
//! or `crate::registry::SourceRegistry` — so every test in this PR
//! (and the spec's own "mocked at the trait/function-pointer
//! boundary" requirement) can substitute an in-memory fake for either
//! one without a real cache, filesystem walk, or watcher (PR-2/PR-4
//! are not exercised by this crate's RPC tests).
//!
//! This module adds no new graph or registry *logic* (F4): both
//! trait implementations below for the real types are one-line calls
//! into already-implemented PR-4/PR-5 methods, plus small `Serialize`
//! wire-shape structs ([`ScanStatsWire`], [`SourceRowWire`]) because
//! `tephrad::scan::ScanStats` and `tephra_cache::SourceRow` don't
//! derive `Serialize` themselves (and this PR does not touch those
//! other crates' structs to add it).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use tephra_cache::{Cache, SourceRow, SourceStatus};
use tephra_graph::{Edge, ResolveError, ResolveResult, UnitGraph};

use crate::registry::{RegistryError, SourceRegistry};
use crate::scan::ScanStats;

/// `unit.resolve_link` / `graph.get_*` (G4), behind a trait so tests
/// substitute a canned in-memory fixture for PR-5's real
/// [`UnitGraph`].
pub trait GraphBackend: Send + Sync {
    fn resolve_link(
        &self,
        raw: &str,
        default_source: Option<&str>,
    ) -> Result<ResolveResult, ResolveError>;
    fn get_backlinks(&self, id: &str) -> Vec<Edge>;
    /// Same lookup as [`GraphBackend::get_backlinks`], but without
    /// its query-display dedup (T1 review finding): a referring file
    /// that links to `id` more than once, via a different link kind
    /// or a differently-spelled raw target, must have *every* such
    /// occurrence found and rewritten, not just whichever one
    /// `get_backlinks`'s dedup happens to keep. Defaults to
    /// `get_backlinks` itself — correct for a test double that never
    /// populates more than one edge per `(from, resolved-target)`
    /// pair anyway — and is overridden by the real
    /// [`UnitGraph`]/[`SharedGraph`] backends below, which do need the
    /// distinction.
    fn get_backlinks_all(&self, id: &str) -> Vec<Edge> {
        self.get_backlinks(id)
    }
    fn get_forward_links(&self, id: &str) -> Vec<Edge>;
    fn get_edges(&self) -> Vec<Edge>;
}

impl GraphBackend for UnitGraph {
    fn resolve_link(
        &self,
        raw: &str,
        default_source: Option<&str>,
    ) -> Result<ResolveResult, ResolveError> {
        UnitGraph::resolve_link(self, raw, default_source)
    }
    fn get_backlinks(&self, id: &str) -> Vec<Edge> {
        UnitGraph::get_backlinks(self, id)
    }
    fn get_backlinks_all(&self, id: &str) -> Vec<Edge> {
        UnitGraph::get_backlinks_all(self, id)
    }
    fn get_forward_links(&self, id: &str) -> Vec<Edge> {
        UnitGraph::get_forward_links(self, id)
    }
    fn get_edges(&self) -> Vec<Edge> {
        UnitGraph::get_edges(self)
    }
}

/// R1 fix: a [`GraphBackend`] whose underlying [`UnitGraph`] snapshot
/// can be swapped out in place, behind a plain `RwLock` — every query
/// takes the read lock for the duration of one call, exactly like a
/// bare `UnitGraph` would; [`SharedGraph::replace`] is the only way to
/// install a fresher snapshot, under the write lock, atomically from
/// every concurrent reader's point of view.
///
/// Without this, the production daemon's in-memory graph was built
/// once at startup and never updated: a `source.register`/`rescan`
/// over RPC (or a watcher-driven change) updated `tephra-cache` on
/// disk but left every running `unit.resolve_link`/`graph.get_*` call
/// answering from the stale startup snapshot until the daemon was
/// restarted (R1 review finding). [`crate::rpc::server::AppState`]
/// holds this behind `Arc<dyn GraphBackend>` for queries, and
/// additionally keeps the concrete `Arc<SharedGraph>` alongside a
/// [`GraphRefresh`] so a successful mutating `source.*` call (and the
/// watcher's own `DaemonEvent`s) can call [`GraphRefresh::refresh`]
/// to rebuild it from the cache and swap it in.
pub struct SharedGraph {
    inner: RwLock<UnitGraph>,
}

impl SharedGraph {
    pub fn new(initial: UnitGraph) -> Self {
        SharedGraph {
            inner: RwLock::new(initial),
        }
    }

    /// Atomically swaps in a freshly built `graph` as this backend's
    /// current snapshot. Every in-flight or subsequent query call
    /// sees either the old snapshot in full or the new one in full —
    /// never a partially-updated graph.
    pub fn replace(&self, graph: UnitGraph) {
        *self.inner.write().unwrap() = graph;
    }
}

impl GraphBackend for SharedGraph {
    fn resolve_link(
        &self,
        raw: &str,
        default_source: Option<&str>,
    ) -> Result<ResolveResult, ResolveError> {
        self.inner.read().unwrap().resolve_link(raw, default_source)
    }
    fn get_backlinks(&self, id: &str) -> Vec<Edge> {
        self.inner.read().unwrap().get_backlinks(id)
    }
    fn get_backlinks_all(&self, id: &str) -> Vec<Edge> {
        self.inner.read().unwrap().get_backlinks_all(id)
    }
    fn get_forward_links(&self, id: &str) -> Vec<Edge> {
        self.inner.read().unwrap().get_forward_links(id)
    }
    fn get_edges(&self) -> Vec<Edge> {
        self.inner.read().unwrap().get_edges()
    }
}

/// Bundles everything needed to rebuild a [`SharedGraph`] from the
/// on-disk cache: the cache's path (a fresh, short-lived [`Cache`]
/// handle is opened from it for each rebuild, exactly like
/// `main.rs`'s own startup-time build does) and the live snapshot to
/// swap the result into. Cheap to clone (two `Arc`-backed fields) so
/// every request handler can own one without borrowing from
/// [`crate::rpc::server::AppState`] across an `.await`/`spawn_blocking`
/// boundary.
///
/// R9 fix: `rebuild_lock` serializes the *entire* body of
/// [`GraphRefresh::refresh`] (cache open, rebuild, and
/// [`SharedGraph::replace`]) across every clone, rather than letting
/// several rebuilds run concurrently. Without it, two overlapping
/// `source.*` mutations (or a mutation racing a watcher-driven
/// refresh) could each read the cache and build a fresh graph in
/// parallel, and whichever `replace()` call happened to *finish*
/// last would win — even if it had started reading the cache
/// *before* the other rebuild's underlying mutation had committed.
/// That could install a snapshot missing a source whose `source.*`
/// call had already returned success (reproducing R1's
/// `unit.resolve_link` -> "no source named ... is registered" error
/// under concurrent use). Serializing the whole read-build-replace
/// sequence makes this impossible: whichever rebuild acquires the
/// lock *last* always opens the cache strictly after every earlier
/// rebuild's own triggering mutation has committed (that mutation
/// necessarily finished before the earlier rebuild's own call was
/// even made, which in turn finished — lock released — before the
/// later rebuild could acquire it), so the last one in is always the
/// most complete, and it is also always the last one to call
/// `replace()`.
#[derive(Clone)]
pub struct GraphRefresh {
    shared: Arc<SharedGraph>,
    cache_path: PathBuf,
    rebuild_lock: Arc<std::sync::Mutex<()>>,
}

impl GraphRefresh {
    pub fn new(shared: Arc<SharedGraph>, cache_path: PathBuf) -> Self {
        GraphRefresh {
            shared,
            cache_path,
            rebuild_lock: Arc::new(std::sync::Mutex::new(())),
        }
    }

    /// Rebuilds a fresh [`UnitGraph`] from the current contents of the
    /// on-disk cache (G4's already-implemented
    /// `UnitGraph::build_from_cache`, not new graph logic — R1 review
    /// finding) and swaps it into the shared backend. Synchronous and
    /// blocking (a real file open plus a full cache read): callers in
    /// `server.rs`/`main.rs` always run this inside
    /// `tokio::task::spawn_blocking`, never directly on an async
    /// worker thread (R6).
    ///
    /// Holds `rebuild_lock` for the full duration (cache open through
    /// `replace()`), so concurrent calls on different `GraphRefresh`
    /// clones are fully serialized rather than racing (R9) — see this
    /// struct's doc comment for why that ordering guarantee is what
    /// prevents a stale rebuild from clobbering a newer one.
    pub fn refresh(&self) -> Result<(), String> {
        let _guard = self
            .rebuild_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache = Cache::open(&self.cache_path).map_err(|e| e.to_string())?;
        let graph = UnitGraph::build_from_cache(&cache).map_err(|e| e.to_string())?;
        self.shared.replace(graph);
        Ok(())
    }
}

/// Wire shape for [`ScanStats`] (`source.register`/`source.rescan`'s
/// successful result) — a plain field-for-field mirror, since
/// `ScanStats` itself derives no `Serialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ScanStatsWire {
    pub files_seen: usize,
    pub files_ignored: usize,
    pub units_written: usize,
    pub links_written: usize,
    pub tags_written: usize,
    pub blocks_written: usize,
}

impl From<ScanStats> for ScanStatsWire {
    fn from(s: ScanStats) -> Self {
        ScanStatsWire {
            files_seen: s.files_seen,
            files_ignored: s.files_ignored,
            units_written: s.units_written,
            links_written: s.links_written,
            tags_written: s.tags_written,
            blocks_written: s.blocks_written,
        }
    }
}

/// Wire shape for one `source.list` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRowWire {
    pub name: String,
    pub path: String,
    pub status: String,
    pub last_scanned_at: Option<i64>,
}

impl From<SourceRow> for SourceRowWire {
    fn from(r: SourceRow) -> Self {
        SourceRowWire {
            name: r.name,
            path: r.path,
            status: status_str(r.status).to_string(),
            last_scanned_at: r.last_scanned_at,
        }
    }
}

fn status_str(status: SourceStatus) -> &'static str {
    match status {
        SourceStatus::Active => "active",
        SourceStatus::Stale => "stale",
        SourceStatus::Offline => "offline",
    }
}

/// `source.register` / `source.unregister` / `source.list` /
/// `source.rescan` (G1), behind a trait so tests substitute an
/// in-memory fake for PR-4's real [`SourceRegistry`]. Errors collapse
/// to a plain message (this server's dispatch layer maps every one to
/// [`crate::rpc::protocol::SOURCE_ERROR`] — none of this PR's
/// acceptance criteria require distinguishing *which* registry error
/// occurred over the wire).
pub trait SourceBackend: Send + Sync {
    fn register(&self, name: &str, root: &str) -> Result<ScanStatsWire, String>;
    fn unregister(&self, name: &str) -> Result<(), String>;
    fn list(&self) -> Result<Vec<SourceRowWire>, String>;
    fn rescan(&self, name: &str) -> Result<ScanStatsWire, String>;
}

impl SourceBackend for SourceRegistry {
    fn register(&self, name: &str, root: &str) -> Result<ScanStatsWire, String> {
        SourceRegistry::register(self, name, root)
            .map(ScanStatsWire::from)
            .map_err(registry_err_to_string)
    }
    fn unregister(&self, name: &str) -> Result<(), String> {
        SourceRegistry::unregister(self, name).map_err(registry_err_to_string)
    }
    fn list(&self) -> Result<Vec<SourceRowWire>, String> {
        SourceRegistry::list(self)
            .map(|rows| rows.into_iter().map(SourceRowWire::from).collect())
            .map_err(registry_err_to_string)
    }
    fn rescan(&self, name: &str) -> Result<ScanStatsWire, String> {
        SourceRegistry::rescan(self, name)
            .map(ScanStatsWire::from)
            .map_err(registry_err_to_string)
    }
}

fn registry_err_to_string(e: RegistryError) -> String {
    e.to_string()
}

/// Wire shape for one `link.preview_refactor` diff entry — a
/// field-for-field mirror of [`crate::refactor::LinkChange`], kept
/// separate (rather than deriving `Serialize` directly on that type
/// and reusing it) only to match this module's existing convention of
/// every wire shape living here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkChangeWire {
    pub file: String,
    pub line: usize,
    pub before: String,
    pub after: String,
}

impl From<crate::refactor::LinkChange> for LinkChangeWire {
    fn from(c: crate::refactor::LinkChange) -> Self {
        LinkChangeWire {
            file: c.file,
            line: c.line,
            before: c.before,
            after: c.after,
        }
    }
}

/// `link.preview_refactor`'s result shape: `{"changes": [...], "skipped": [...]}`.
/// R10 review finding: `skipped` names every referring file the
/// preview could not read or stat at all — never silently narrowing
/// `changes` with no indication anything was left out.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RefactorPreviewWire {
    pub changes: Vec<LinkChangeWire>,
    pub skipped: Vec<SkippedFileWire>,
}

/// One file [`crate::refactor::RefactorEngine::preview`] could not
/// read or stat — field-for-field mirror of
/// [`crate::refactor::SkippedFile`] (R10 review finding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedFileWire {
    pub file: String,
    pub error: String,
}

impl From<crate::refactor::SkippedFile> for SkippedFileWire {
    fn from(s: crate::refactor::SkippedFile) -> Self {
        SkippedFileWire {
            file: s.file,
            error: s.error,
        }
    }
}

/// One failed file in a `"partial"` `link.apply_refactor` result —
/// field-for-field mirror of [`crate::refactor::FailedFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedFileWire {
    pub file: String,
    pub error: String,
}

impl From<crate::refactor::FailedFile> for FailedFileWire {
    fn from(f: crate::refactor::FailedFile) -> Self {
        FailedFileWire {
            file: f.file,
            error: f.error,
        }
    }
}

/// `link.apply_refactor`'s result shape. Serializes to exactly
/// `{"status":"applied","files_changed":N}`,
/// `{"status":"aborted","reason":"..."}`, or (the permission-denied-
/// mid-apply edge case) `{"status":"partial","files_changed":N,"succeeded":[...],"failed":[...]}`
/// — `tephractl`'s own PR-7 render/CLI code only ever reads `"status"`
/// and (for `"applied"`) `"files_changed"`, so `"partial"`'s extra
/// `"succeeded"`/`"failed"` fields are additive and do not change
/// that contract. `"succeeded"` names exactly which files were
/// rewritten before the first failure (R5 review finding: the spec
/// requires the response to "enumerate exactly which files succeeded
/// vs failed", which a bare count could never do).
#[derive(Debug, Clone, Serialize)]
pub struct RefactorApplyWire {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Vec<FailedFileWire>>,
}

impl From<crate::refactor::ApplyOutcome> for RefactorApplyWire {
    fn from(outcome: crate::refactor::ApplyOutcome) -> Self {
        use crate::refactor::ApplyOutcome;
        match outcome {
            ApplyOutcome::Applied { files_changed } => RefactorApplyWire {
                status: "applied".to_string(),
                files_changed: Some(files_changed),
                reason: None,
                succeeded: None,
                failed: None,
            },
            ApplyOutcome::Aborted { reason } => RefactorApplyWire {
                status: "aborted".to_string(),
                files_changed: None,
                reason: Some(reason),
                succeeded: None,
                failed: None,
            },
            ApplyOutcome::Partial {
                files_changed,
                succeeded,
                failed,
            } => RefactorApplyWire {
                status: "partial".to_string(),
                files_changed: Some(files_changed),
                reason: None,
                succeeded: Some(succeeded),
                failed: Some(failed.into_iter().map(FailedFileWire::from).collect()),
            },
        }
    }
}

/// `link.preview_refactor` / `link.apply_refactor` (G7), behind a
/// trait for the same reason as [`GraphBackend`]/[`SourceBackend`]:
/// so a test substitutes an in-memory fake for the real
/// [`crate::refactor::RefactorEngine`] without a real graph, registry,
/// or filesystem.
pub trait RefactorBackend: Send + Sync {
    fn preview_refactor(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<RefactorPreviewWire, String>;
    fn apply_refactor(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<RefactorApplyWire, String>;
}

impl RefactorBackend for crate::refactor::RefactorEngine {
    fn preview_refactor(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<RefactorPreviewWire, String> {
        self.preview(source, old_path, new_path)
            .map(|result| RefactorPreviewWire {
                changes: result
                    .changes
                    .into_iter()
                    .map(LinkChangeWire::from)
                    .collect(),
                skipped: result
                    .skipped
                    .into_iter()
                    .map(SkippedFileWire::from)
                    .collect(),
            })
    }
    fn apply_refactor(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<RefactorApplyWire, String> {
        self.apply(source, old_path, new_path)
            .map(RefactorApplyWire::from)
    }
}

/// A [`RefactorBackend`] that always errors — for tests/fixtures that
/// construct an [`crate::rpc::server::AppState`] without exercising
/// `link.preview_refactor`/`link.apply_refactor` at all.
pub struct UnsupportedRefactor;

impl RefactorBackend for UnsupportedRefactor {
    fn preview_refactor(
        &self,
        _source: &str,
        _old_path: &str,
        _new_path: &str,
    ) -> Result<RefactorPreviewWire, String> {
        Err("refactor backend not configured for this test fixture".to_string())
    }
    fn apply_refactor(
        &self,
        _source: &str,
        _old_path: &str,
        _new_path: &str,
    ) -> Result<RefactorApplyWire, String> {
        Err("refactor backend not configured for this test fixture".to_string())
    }
}
