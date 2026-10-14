//! The frozen public shapes `UnitGraph` returns (G4/GP5): node
//! metadata, edges, and the `UnitGraph` struct itself plus its
//! query-side methods. Mutation (`upsert_unit`/`remove_unit`/
//! `build_from_cache`/`apply_change_notification`) lives in
//! `crate::ingest`; target resolution lives in `crate::resolver`.
//! Split this way so the shape PR-6's RPC handler re-exposes
//! unmodified (GP5) is defined in exactly one place, not scattered
//! across the mutation and resolution logic that fills it in.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::resolver::{basename_of, normalize_key, strip_markdown_extension};

/// One unit's identity and derived metadata, keyed by its federated
/// `@source:path` id. This is the exact shape [`crate::ResolveResult`]
/// and [`crate::Edge`] embed, and the one GP5 freezes for PR-6's RPC
/// handler to return unmodified.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnitMetadata {
    /// The federated identifier, `"@<source>:<rel_path>"`.
    pub id: String,
    pub source: String,
    /// The unit's path exactly as stored in `tephra-cache`'s
    /// `units.rel_path` — no case-folding, no Unicode normalization,
    /// so a non-ASCII or space-containing path round-trips byte for
    /// byte. (Resolution internally normalizes a *copy* of this for
    /// matching purposes, R6 — the stored value itself is untouched.)
    pub rel_path: String,
    pub frontmatter_json: Option<String>,
    pub tags: Vec<String>,
    pub blocks: Vec<String>,
    /// Whether this unit's owning source is currently marked offline
    /// (E2/P4): a cached-but-possibly-stale unit is still returned,
    /// never dropped, with this flag set instead.
    pub offline: bool,
}

/// What a wikilink/markdown-link edge's `to_raw` resolved to.
///
/// Serialized externally-tagged with `rename_all = "snake_case"`
/// (`{"resolved":"@source:path"}` or the plain string `"dangling"`) —
/// the frozen GP5 wire shape; see `tests/unit/gp5_contract.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeTarget {
    /// Resolved to exactly one unit, named by its federated id.
    Resolved(String),
    /// Unresolved: zero matches, an ambiguous tie, an unknown
    /// `@source` prefix, or (for a markdown link) an absolute URL.
    /// Represented once as a dangling edge rather than dropped (one
    /// of the "deleted/renamed unit ... surfaces as a dangling link"
    /// and "unknown @source prefix never panics or silently drops"
    /// edge cases) — [`EdgeTarget`] deliberately does not distinguish
    /// *why* an edge is dangling; [`crate::UnitGraph::resolve_link`]
    /// is the API that reports the specific reason, for a caller that
    /// is resolving one link directly rather than reading it back off
    /// an already-built edge.
    Dangling,
}

/// One forward/backward link in the graph, derived from a single
/// `tephra-cache` `links` row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: EdgeTarget,
    /// The link's raw text exactly as parsed (`WikiLink::raw` or
    /// `MarkdownLink::target`), kept so a caller can show *why* an
    /// edge is dangling or ambiguous without re-deriving it.
    pub to_raw: String,
    /// `tephra_parser::LINK_KIND_WIKILINK` or `LINK_KIND_MARKDOWN`.
    pub kind: String,
    /// `true` if either endpoint belongs to a currently-offline
    /// source (E2/P4): the edge is still returned, annotated, rather
    /// than dropped or erroring.
    pub offline: bool,
}

/// One unit's facts as held internally: everything [`UnitMetadata`]
/// needs, plus its own outgoing links in raw (unresolved) form —
/// resolution happens lazily, on [`UnitGraph::rebuild_edges`], not at
/// insert time, so a later unit's insertion can retroactively resolve
/// an earlier dangling edge that targeted it.
#[derive(Debug, Clone)]
pub(crate) struct NodeFacts {
    pub source: String,
    pub rel_path: String,
    pub frontmatter_json: Option<String>,
    pub tags: Vec<String>,
    pub blocks: Vec<String>,
    /// `(to_raw, kind)` pairs, one per outgoing link this unit's
    /// content contains.
    pub links: Vec<(String, String)>,
}

/// The federated id of a unit: `"@<source>:<rel_path>"`, matching the
/// `[[@source:path]]` wikilink prefix syntax users type (minus the
/// brackets) — see the Plan N `UnitGraph` noun definition.
pub(crate) fn federated_id(source: &str, rel_path: &str) -> String {
    format!("@{source}:{rel_path}")
}

/// Splits a federated id (`"@<source>:<rel_path>"`) back into its two
/// parts, or `None` if `id` isn't in that shape at all. Used by
/// [`UnitGraph::get_backlinks`] to recover the `(source, rel_path)`
/// a *stale* id (one with no current node, e.g. the pre-rename path
/// in GP7's scenario) still names, so a dangling-edge lookup can be
/// scoped to the right source without requiring the node to exist.
fn parse_federated_id(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix('@')?.split_once(':')
}

/// The in-memory federated link graph (G4): nodes keyed by federated
/// `@source:path` id, edges derived from each node's own outgoing
/// links and resolved against the *current* node set every time the
/// graph's content changes (see [`UnitGraph::rebuild_edges`]).
///
/// Everything here is plain owned data — no database handle, no
/// filesystem path, no RPC/transport type anywhere in this struct or
/// its methods (F3): building and updating a `UnitGraph` from
/// `tephra-cache` rows is `crate::ingest`'s job, called by a caller
/// that already has a `tephra_cache::Cache` open.
pub struct UnitGraph {
    pub(crate) nodes: HashMap<String, NodeFacts>,
    /// Every source name this graph knows about, including one with
    /// zero parsed units — kept distinct from "any source name that
    /// happens to appear on a node" so a freshly registered, still-
    /// empty source is never mistaken for an unknown `@source` prefix
    /// (the specific edge case this graph must tell apart).
    pub(crate) known_sources: HashSet<String>,
    pub(crate) offline_sources: HashSet<String>,
    /// Source name -> set of federated ids belonging to it, kept in
    /// sync with `nodes` on every insert/remove.
    pub(crate) by_source: HashMap<String, HashSet<String>>,
    /// Source name -> normalized full stem (NFC, extension stripped)
    /// -> ids sharing that exact stem (at most one in practice, since
    /// `rel_path` is unique per source, but kept as a `Vec` for
    /// robustness). The O(1)-average candidate lookup
    /// [`crate::resolver::resolve_in_scope`] uses for a `/`-containing
    /// target, instead of scanning every unit in the source (R3) —
    /// the root cause of the pre-fix quadratic incremental-update
    /// cost.
    pub(crate) stem_index: HashMap<String, HashMap<String, Vec<String>>>,
    /// Source name -> normalized basename (the final path component
    /// of the stem) -> ids sharing that basename, possibly several
    /// (E4's ambiguous-tie case). The O(1)-average candidate lookup
    /// for a bare (no `/`) target.
    pub(crate) basename_index: HashMap<String, HashMap<String, Vec<String>>>,
    pub(crate) edges: Vec<Edge>,
    /// Federated id -> indices into `edges` whose `from` is that id.
    pub(crate) forward_index: HashMap<String, Vec<usize>>,
    /// Federated id -> indices into `edges` whose `to` resolved to
    /// that id. Kept as a real index (not a linear scan over `edges`)
    /// so [`UnitGraph::get_backlinks`] stays bounded-time even for a
    /// single unit with well over 1,000 incoming edges.
    pub(crate) backward_index: HashMap<String, Vec<usize>>,
    /// `(scope_source, normalized_target_key)` -> indices into `edges`
    /// of currently-[`EdgeTarget::Dangling`] edges whose own computed
    /// match key equals that pair (see
    /// [`crate::resolver::edge_target_scope_key`]) — the key *every*
    /// edge, resolved or not, would be looked up under if a unit
    /// existed at that path/name. Lets two things stay bounded-time
    /// rather than rescanning every edge in the graph:
    /// - a unit inserted or renamed into place can find which
    ///   previously-dangling edges might now resolve to it (R3's
    ///   scoped incremental update);
    /// - [`UnitGraph::get_backlinks`], queried with a *stale* id (one
    ///   with no current node — GP7's renamed-note scenario), can
    ///   still surface every edge that still intends to target that
    ///   path/name even though it no longer resolves (R4).
    pub(crate) dangling_index: HashMap<(String, String), Vec<usize>>,
}

impl Default for UnitGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl UnitGraph {
    /// An empty graph: zero registered sources, zero units, zero
    /// edges. Every query verb below returns an empty (never `null`,
    /// never an error) result against it.
    pub fn new() -> Self {
        UnitGraph {
            nodes: HashMap::new(),
            known_sources: HashSet::new(),
            offline_sources: HashSet::new(),
            by_source: HashMap::new(),
            stem_index: HashMap::new(),
            basename_index: HashMap::new(),
            edges: Vec::new(),
            forward_index: HashMap::new(),
            backward_index: HashMap::new(),
            dangling_index: HashMap::new(),
        }
    }

    /// Registers `source` as known to this graph even with zero
    /// parsed units yet — so a query against it degrades to "no
    /// matches" rather than "unknown source" (the "freshly registered
    /// source with zero parsed units" edge case). Idempotent.
    pub fn register_source(&mut self, source: &str) {
        self.known_sources.insert(source.to_string());
        self.by_source.entry(source.to_string()).or_default();
        self.stem_index.entry(source.to_string()).or_default();
        self.basename_index.entry(source.to_string()).or_default();
    }

    /// Marks `source` offline/online (E2/P4) and recomputes every
    /// edge's `offline` annotation accordingly. Does not drop or
    /// otherwise touch the source's cached nodes/edges either way, and
    /// — since toggling a source's status never changes what resolves
    /// to what — never re-resolves a single link either; only
    /// [`Edge::offline`]/[`UnitMetadata::offline`] change (R3: not a
    /// rebuild of any kind, just an O(edges) annotation pass).
    pub fn set_source_offline(&mut self, source: &str, offline: bool) {
        self.register_source(source);
        if offline {
            self.offline_sources.insert(source.to_string());
        } else {
            self.offline_sources.remove(source);
        }
        self.recompute_offline_flags();
    }

    pub(crate) fn recompute_offline_flags(&mut self) {
        let nodes = &self.nodes;
        let offline_sources = &self.offline_sources;
        for edge in &mut self.edges {
            let from_offline = nodes
                .get(&edge.from)
                .is_some_and(|n| offline_sources.contains(&n.source));
            let to_offline = matches!(&edge.to, EdgeTarget::Resolved(to_id)
                if nodes.get(to_id).is_some_and(|n| offline_sources.contains(&n.source)));
            edge.offline = from_offline || to_offline;
        }
    }

    pub fn is_source_offline(&self, source: &str) -> bool {
        self.offline_sources.contains(source)
    }

    /// Borrows the four index structures [`crate::resolver::resolve_in_scope`]/
    /// [`crate::resolver::resolve_edge_target`] read, bundled as one
    /// [`crate::resolver::GraphIndex`] — the shared borrow every
    /// resolution call site (`crate::ingest`'s rebuild/recompute paths,
    /// and [`UnitGraph::resolve_link`]) builds instead of repeating
    /// four separate field borrows each.
    pub(crate) fn index(&self) -> crate::resolver::GraphIndex<'_> {
        crate::resolver::GraphIndex {
            nodes: &self.nodes,
            stem_index: &self.stem_index,
            basename_index: &self.basename_index,
            known_sources: &self.known_sources,
        }
    }

    /// Every source name this graph currently knows about (G4's
    /// federated namespace set), in no particular order.
    pub fn known_sources(&self) -> Vec<String> {
        self.known_sources.iter().cloned().collect()
    }

    /// The metadata for one unit, by federated id, or `None` if no
    /// such unit is currently in the graph (never an error — a stale
    /// id is simply absent).
    pub fn unit(&self, id: &str) -> Option<UnitMetadata> {
        self.nodes.get(id).map(|f| UnitMetadata {
            id: id.to_string(),
            source: f.source.clone(),
            rel_path: f.rel_path.clone(),
            frontmatter_json: f.frontmatter_json.clone(),
            tags: f.tags.clone(),
            blocks: f.blocks.clone(),
            offline: self.offline_sources.contains(&f.source),
        })
    }

    /// `graph.get_forward_links` (G4): every edge authored by `id`,
    /// sorted for deterministic output, with every duplicate (R2 —
    /// the same `(from, to)` pair reached via more than one raw link,
    /// e.g. `[[A]]` and `[[#H]]` on a note named `A` both resolving to
    /// a self-edge) already collapsed to one row by construction (see
    /// `crate::ingest`'s edge de-duplication). Empty (never `null`,
    /// never an error) if `id` has no outgoing edges or doesn't exist
    /// at all.
    pub fn get_forward_links(&self, id: &str) -> Vec<Edge> {
        let mut out: Vec<Edge> = self
            .forward_index
            .get(id)
            .map(|idxs| idxs.iter().map(|&i| self.edges[i].clone()).collect())
            .unwrap_or_default();
        out.sort_by(edge_sort_key);
        out
    }

    /// `graph.get_backlinks` (G4): every edge whose target resolved
    /// to `id`, sorted for deterministic output and already
    /// de-duplicated (R2 — see [`UnitGraph::get_forward_links`]).
    /// Looked up through [`UnitGraph::backward_index`], so this stays
    /// bounded-time regardless of how many thousands of edges target
    /// `id` (no linear scan over every edge in the graph).
    ///
    /// Also includes every currently-[`EdgeTarget::Dangling`] edge
    /// whose own match key equals `id`'s path or basename (R4/GP7):
    /// when `id` names a unit no longer in the graph — most
    /// pointedly, the pre-rename path of a note PR-4's watcher just
    /// moved — the edges that used to resolve to it flip to dangling
    /// on removal rather than vanishing from `get_backlinks`
    /// altogether, so a refactor-preview caller (PR-8) can still find
    /// every impacted incoming link across every source by querying
    /// the *old* id, exactly as GP7 requires. This lookup is a no-op
    /// when `id` currently does exist as a real node: any link that
    /// would match its path/basename already resolved (so is not
    /// dangling) rather than being left dangling alongside a live
    /// match. Empty (never `null`, never an error) if nothing targets
    /// `id` at all.
    pub fn get_backlinks(&self, id: &str) -> Vec<Edge> {
        let mut idxs: Vec<usize> = self.backward_index.get(id).cloned().unwrap_or_default();

        if let Some((source, rel_path)) = parse_federated_id(id) {
            let stem = normalize_key(strip_markdown_extension(rel_path));
            let basename = basename_of(&stem).to_string();
            if let Some(v) = self.dangling_index.get(&(source.to_string(), stem.clone())) {
                idxs.extend(v.iter().copied());
            }
            if basename != stem {
                if let Some(v) = self.dangling_index.get(&(source.to_string(), basename)) {
                    idxs.extend(v.iter().copied());
                }
            }
        }

        idxs.sort_unstable();
        idxs.dedup();
        let mut out: Vec<Edge> = idxs.iter().map(|&i| self.edges[i].clone()).collect();
        out.sort_by(edge_sort_key);
        out
    }

    /// `graph.get_edges` (G4): every edge in the graph, sorted for
    /// deterministic output and de-duplicated (R2). Empty (never
    /// `null`) for an empty graph.
    pub fn get_edges(&self) -> Vec<Edge> {
        let mut out = self.edges.clone();
        out.sort_by(edge_sort_key);
        out
    }
}

fn edge_sort_key(a: &Edge, b: &Edge) -> std::cmp::Ordering {
    let to_key = |e: &Edge| match &e.to {
        EdgeTarget::Resolved(id) => (0u8, id.clone()),
        EdgeTarget::Dangling => (1u8, String::new()),
    };
    (a.from.clone(), to_key(a), a.kind.clone(), a.to_raw.clone()).cmp(&(
        b.from.clone(),
        to_key(b),
        b.kind.clone(),
        b.to_raw.clone(),
    ))
}
