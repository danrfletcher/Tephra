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

use serde::Serialize;

use tephra_cache::{SourceRow, SourceStatus};
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
