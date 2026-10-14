//! `tephra-graph`: the in-memory federated link graph and resolver
//! (G4) that sits between `tephra-cache`'s cached, parsed units
//! (PR-2/PR-3) and Tephra's future RPC/CLI surface (PR-6/PR-7).
//!
//! [`UnitGraph`] is keyed on `@source:path` federated identifiers
//! (the Plan N `UnitGraph` noun). It exposes four plain Rust library
//! functions — [`UnitGraph::resolve_link`] (`unit.resolve_link`),
//! [`UnitGraph::get_backlinks`] (`graph.get_backlinks`),
//! [`UnitGraph::get_forward_links`] (`graph.get_forward_links`), and
//! [`UnitGraph::get_edges`] (`graph.get_edges`) — against an
//! in-memory graph built from `tephra-cache` rows, independent of any
//! RPC transport.
//!
//! Out of scope for this crate (F3): RPC transport, authentication,
//! CLI plumbing, and on-disk link refactoring — all later PRs. This
//! crate contains no UI, webview, or RPC-transport code and depends
//! only on `tephra-cache` and `tephra-parser`; see
//! `tests/fence_no_ui_rpc_deps.rs` for the dependency-tree audit that
//! enforces this.
//!
//! Module layout:
//! - [`model`]: the frozen public shapes ([`UnitMetadata`], [`Edge`],
//!   [`EdgeTarget`]) and [`UnitGraph`] itself, plus its query-side
//!   methods ([`UnitGraph::get_backlinks`]/[`UnitGraph::get_forward_links`]/
//!   [`UnitGraph::get_edges`]).
//! - [`resolver`]: target resolution — [`UnitGraph::resolve_link`]
//!   and the shared in-source matching rule (E4) edge-building also
//!   uses.
//! - [`ingest`]: building a [`UnitGraph`] from `tephra-cache` rows
//!   ([`UnitGraph::build_from_cache`]) and updating it incrementally
//!   ([`UnitGraph::upsert_unit`]/[`UnitGraph::remove_unit`]/
//!   [`UnitGraph::apply_change_notification`]).

mod ingest;
mod model;
mod resolver;

pub use ingest::{ChangeNotification, GraphError, UnitFacts};
pub use model::{Edge, EdgeTarget, UnitGraph, UnitMetadata};
pub use resolver::{ResolveError, ResolveResult};

impl UnitGraph {
    /// `unit.resolve_link` (G4/GP5): resolves `raw` (a full
    /// `[[...]]` wikilink or its bare inner text, e.g.
    /// `"[[@atlas-repo:README]]"` or `"@atlas-repo:README"`) against
    /// this graph, returning the resolved unit's metadata and its own
    /// outgoing edges — the exact shape GP5 freezes for PR-6's RPC
    /// handler to return unmodified.
    ///
    /// `default_source` is the source a non-federated (no `@source:`
    /// prefix) target resolves against; ignored when `raw` itself
    /// carries a prefix. Pass `None` when resolving a link with no
    /// natural "current source" of its own (as in the GP5 example,
    /// which is always fully prefixed); every edge the graph resolves
    /// internally while building/updating always supplies the
    /// referring unit's own source instead.
    ///
    /// See [`ResolveError`] for every documented failure mode (E4's
    /// ambiguous tie-break, an unknown `@source` prefix, a target
    /// absent from the scoped source, missing source context) — this
    /// never panics and never silently guesses.
    pub fn resolve_link(
        &self,
        raw: &str,
        default_source: Option<&str>,
    ) -> Result<ResolveResult, ResolveError> {
        resolver::resolve_link_impl(self, raw, default_source)
    }
}
