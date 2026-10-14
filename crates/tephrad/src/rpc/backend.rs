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
#[derive(Clone)]
pub struct GraphRefresh {
    shared: Arc<SharedGraph>,
    cache_path: PathBuf,
}

impl GraphRefresh {
    pub fn new(shared: Arc<SharedGraph>, cache_path: PathBuf) -> Self {
        GraphRefresh { shared, cache_path }
    }

    /// Rebuilds a fresh [`UnitGraph`] from the current contents of the
    /// on-disk cache (G4's already-implemented
    /// `UnitGraph::build_from_cache`, not new graph logic — R1 review
    /// finding) and swaps it into the shared backend. Synchronous and
    /// blocking (a real file open plus a full cache read): callers in
    /// `server.rs`/`main.rs` always run this inside
    /// `tokio::task::spawn_blocking`, never directly on an async
    /// worker thread (R6).
    pub fn refresh(&self) -> Result<(), String> {
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
