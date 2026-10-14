//! Builds and incrementally updates a [`crate::UnitGraph`] from
//! `tephra-cache` rows (G4: "built from the PR-2 cache and PR-3
//! parsed units"). Everything here is a plain function/method over
//! owned data plus a borrowed `&tephra_cache::Cache` — no filesystem
//! walking, no watcher, no RPC (F3): the caller (PR-4's registry
//! today, a later PR's broadcaster/RPC layer eventually) already has
//! a `Cache` open and knows which source/paths changed.

use std::collections::{HashMap, HashSet};
use std::fmt;

use tephra_cache::{Cache, CacheError, SourceStatus};

use crate::model::{federated_id, parse_federated_id, NodeFacts};
use crate::resolver::{
    basename_of, edge_target_scope_key, normalize_key, resolve_edge_target,
    strip_markdown_extension,
};
use crate::{Edge, EdgeTarget, UnitGraph};

/// One unit's facts in the shape [`UnitGraph::upsert_unit`] takes:
/// everything [`crate::UnitMetadata`] needs, plus its own outgoing
/// links as `(to_raw, kind)` pairs (unresolved — resolution happens
/// against the live graph, not at the call site).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitFacts {
    pub frontmatter_json: Option<String>,
    pub tags: Vec<String>,
    pub blocks: Vec<String>,
    pub links: Vec<(String, String)>,
}

/// A stand-in for one of PR-4's `DaemonEvent::SourceChanged`
/// notifications, scoped to this crate's own vocabulary (a source
/// name plus the paths that changed *within* it, relative to that
/// source's root) rather than depending on `tephrad`'s concrete event
/// type — `tephra-graph` must stay a leaf library PR-4/PR-6 depend
/// on, never the other way — see `tests/fixtures/mod.rs`. A real
/// caller (a later PR) is responsible for turning a `DaemonEvent`'s
/// absolute `PathBuf`s into these source-relative strings before
/// calling [`UnitGraph::apply_change_notification`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeNotification {
    pub source: String,
    pub rel_paths: Vec<String>,
}

#[derive(Debug)]
pub enum GraphError {
    Cache(CacheError),
    /// [`UnitGraph::apply_change_notification`] was given a source
    /// name the cache itself has no `sources` row for.
    UnknownSource(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Cache(e) => write!(f, "{e}"),
            GraphError::UnknownSource(name) => write!(f, "no source named \"{name}\" in cache"),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<CacheError> for GraphError {
    fn from(e: CacheError) -> Self {
        GraphError::Cache(e)
    }
}

/// Reads unit `unit_id`'s tags/blocks/links out of `cache` into a
/// [`UnitFacts`], the shared step [`UnitGraph::build_from_cache`] and
/// [`UnitGraph::apply_change_notification`] both use.
fn load_unit_facts(
    cache: &Cache,
    unit_id: i64,
    frontmatter_json: Option<String>,
) -> Result<UnitFacts, GraphError> {
    let tags = cache
        .tags_for_unit(unit_id)?
        .into_iter()
        .map(|t| t.tag)
        .collect();
    let blocks = cache
        .blocks_for_unit(unit_id)?
        .into_iter()
        .map(|b| b.block_id)
        .collect();
    let links = cache
        .links_from_unit(unit_id)?
        .into_iter()
        .map(|l| (l.to_raw, l.kind))
        .collect();
    Ok(UnitFacts {
        frontmatter_json,
        tags,
        blocks,
        links,
    })
}

/// The `(scope_source, normalized_target_key)` [`UnitGraph::match_index`]
/// key `edge` would be indexed under, or `None` when `edge` has no
/// sensible key (its target is an absolute URL / anchor-only
/// self-link) — used only by [`insert_edge`], at a point where
/// `edge.from` always still has a node (R17: see
/// [`crate::UnitGraph::edge_keys`] for why [`remove_edge`] must *not*
/// call this — by the time a removed unit's own authored edges are
/// superseded, `edge.from`'s node is already gone, so recomputing the
/// key here would silently return `None` and skip the `match_index`
/// prune, leaving a stale slot number behind for [`insert_edge`] to
/// later hand to an unrelated edge via `free_slots`).
fn edge_match_key(graph: &UnitGraph, edge: &Edge) -> Option<(String, String)> {
    let from_source = graph.nodes.get(&edge.from)?.source.clone();
    let index = graph.index();
    edge_target_scope_key(&index, &edge.from, &from_source, &edge.to_raw, &edge.kind)
}

/// Inserts `edge` into a free (or newly appended) slot in
/// `graph.edges`, updating `forward_index`/`backward_index`/
/// `match_index`/`edge_keys` for just this one edge (R13:
/// O(1)-amortized, never a whole-graph rebuild) — the counterpart to
/// [`remove_edge`]. The match key is computed once, here, and stored
/// in `graph.edge_keys` alongside the edge (R17) so `remove_edge` can
/// prune `match_index` correctly even after `edge.from`'s node has
/// since been removed.
fn insert_edge(graph: &mut UnitGraph, edge: Edge) -> usize {
    let key = edge_match_key(graph, &edge);
    let to = edge.to.clone();
    let from = edge.from.clone();

    let slot = match graph.free_slots.pop() {
        Some(s) => {
            graph.edges[s] = Some(edge);
            graph.edge_keys[s] = key.clone();
            s
        }
        None => {
            graph.edges.push(Some(edge));
            graph.edge_keys.push(key.clone());
            graph.edges.len() - 1
        }
    };

    graph.forward_index.entry(from).or_default().push(slot);
    if let EdgeTarget::Resolved(to_id) = to {
        graph.backward_index.entry(to_id).or_default().push(slot);
    }
    if let Some(key) = key {
        graph.match_index.entry(key).or_default().push(slot);
    }
    slot
}

/// Removes the edge currently at `slot` (a no-op if the slot is
/// already empty), pruning its `forward_index`/`backward_index`/
/// `match_index` entries and freeing the slot for reuse — the
/// counterpart to [`insert_edge`], same O(1)-amortized cost (R13).
/// Prunes `match_index` using the key stored in `graph.edge_keys` at
/// insert time (R17), *not* by recomputing it from `edge.from`'s
/// node: that node may already be gone by the time this runs (a
/// removed unit's own authored edges are superseded before
/// `recompute_scoped` finishes), and recomputing from a missing node
/// would silently skip the prune, leaving the slot number behind in
/// `match_index` for [`insert_edge`] to later hand to a completely
/// unrelated edge.
fn remove_edge(graph: &mut UnitGraph, slot: usize) {
    let Some(edge) = graph.edges[slot].take() else {
        return;
    };
    let key = graph.edge_keys[slot].take();
    if let Some(v) = graph.forward_index.get_mut(&edge.from) {
        v.retain(|&i| i != slot);
    }
    if let EdgeTarget::Resolved(to_id) = &edge.to {
        if let Some(v) = graph.backward_index.get_mut(to_id) {
            v.retain(|&i| i != slot);
        }
    }
    if let Some(key) = key {
        if let Some(v) = graph.match_index.get_mut(&key) {
            v.retain(|&i| i != slot);
        }
    }
    graph.free_slots.push(slot);
}

impl UnitGraph {
    /// Inserts/replaces one node's facts and its stem/basename index
    /// entries (no edge resolution/rebuild — callers batching
    /// multiple changes call [`UnitGraph::rebuild_edges`] or
    /// [`UnitGraph::recompute_scoped`] once at the end).
    fn insert_node(&mut self, source: &str, rel_path: &str, facts: UnitFacts) {
        self.register_source(source);
        let id = federated_id(source, rel_path);

        self.by_source
            .entry(source.to_string())
            .or_default()
            .insert(id.clone());

        let stem = normalize_key(strip_markdown_extension(rel_path));
        let basename = basename_of(&stem).to_string();

        // Re-upserting the same path (an edit) must not duplicate
        // this id's own entry in either index (R9): a bare re-insert
        // of an already-indexed id would otherwise leave `[id, id]`
        // behind, making every future lookup under this exact
        // stem/basename see a spurious two-way tie with itself.
        let stem_bucket = self
            .stem_index
            .entry(source.to_string())
            .or_default()
            .entry(stem)
            .or_default();
        if !stem_bucket.contains(&id) {
            stem_bucket.push(id.clone());
        }
        let basename_bucket = self
            .basename_index
            .entry(source.to_string())
            .or_default()
            .entry(basename)
            .or_default();
        if !basename_bucket.contains(&id) {
            basename_bucket.push(id.clone());
        }

        self.nodes.insert(
            id,
            NodeFacts {
                source: source.to_string(),
                rel_path: rel_path.to_string(),
                frontmatter_json: facts.frontmatter_json,
                tags: facts.tags,
                blocks: facts.blocks,
                links: facts.links,
            },
        );
    }

    /// Removes one node's facts and its stem/basename index entries
    /// (no edge rebuild — see [`UnitGraph::insert_node`]).
    fn remove_node(&mut self, source: &str, rel_path: &str) {
        let id = federated_id(source, rel_path);
        self.nodes.remove(&id);
        if let Some(set) = self.by_source.get_mut(source) {
            set.remove(&id);
        }
        let stem = normalize_key(strip_markdown_extension(rel_path));
        let basename = basename_of(&stem).to_string();
        if let Some(m) = self.stem_index.get_mut(source) {
            if let Some(v) = m.get_mut(&stem) {
                v.retain(|existing| existing != &id);
            }
        }
        if let Some(m) = self.basename_index.get_mut(source) {
            if let Some(v) = m.get_mut(&basename) {
                v.retain(|existing| existing != &id);
            }
        }
    }

    /// Inserts or replaces one unit's facts, then recomputes only the
    /// edges this change could possibly affect (R3) — see
    /// [`UnitGraph::recompute_scoped`]. For a caller applying many
    /// changes at once, prefer
    /// [`UnitGraph::apply_change_notification`] or
    /// [`UnitGraph::build_from_cache`], which batch the recompute
    /// into a single call rather than paying it once per unit.
    pub fn upsert_unit(&mut self, source: &str, rel_path: &str, facts: UnitFacts) {
        let id = federated_id(source, rel_path);
        self.insert_node(source, rel_path, facts);
        self.recompute_scoped(&[id]);
    }

    /// Removes one unit and recomputes only the edges this change
    /// could possibly affect — see [`UnitGraph::upsert_unit`]'s note
    /// on batching.
    pub fn remove_unit(&mut self, source: &str, rel_path: &str) {
        let id = federated_id(source, rel_path);
        self.remove_node(source, rel_path);
        self.recompute_scoped(&[id]);
    }

    /// Recomputes every edge from the current `nodes` map from
    /// scratch and rebuilds the forward/backward/match indices from
    /// nothing. Used for the initial [`UnitGraph::build_from_cache`]
    /// (there is no smaller scope to recompute — every node is new)
    /// and as the one-time cost [`UnitGraph::recompute_scoped`]'s own
    /// candidate gathering never needs to pay again on a later edit.
    /// Resolution itself goes through `stem_index`/`basename_index`
    /// (O(1) average per link, R3) rather than scanning every unit in
    /// a link's scope source, so this full rebuild is O(total links),
    /// not O(total links × units-in-source).
    pub(crate) fn rebuild_edges(&mut self) {
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort();

        let mut edges: Vec<Edge> = Vec::new();
        for id in ids {
            let facts = self.nodes.get(id).expect("id came from nodes.keys()");
            let index = self.index();
            for (to_raw, kind) in &facts.links {
                let to = resolve_edge_target(&index, id, &facts.source, to_raw, kind);
                let target_offline = matches!(&to, EdgeTarget::Resolved(to_id)
                    if self.nodes.get(to_id).is_some_and(|n| self.offline_sources.contains(&n.source)));
                let offline = self.offline_sources.contains(&facts.source) || target_offline;
                edges.push(Edge {
                    from: id.clone(),
                    to,
                    to_raw: to_raw.clone(),
                    kind: kind.clone(),
                    offline,
                });
            }
        }

        // No storage-time de-duplication (R16): every raw link keeps
        // its own indexed [`Edge`] record (and its own `match_index`
        // entry), even when it currently resolves to the same target
        // as another raw link on the same unit — collapsing duplicate
        // query-output rows happens only in `crate::model`'s
        // `get_edges`/`get_forward_links`/`get_backlinks`, never here,
        // so an edge that currently shares a resolution with another
        // can still be found and re-resolved independently once a
        // later insert/remove makes the two diverge again.
        self.edges = Vec::new();
        self.free_slots = Vec::new();
        self.forward_index = HashMap::new();
        self.backward_index = HashMap::new();
        self.match_index = HashMap::new();
        self.edge_keys = Vec::new();
        for edge in edges {
            insert_edge(self, edge);
        }
    }

    /// Recomputes edges scoped to exactly what `touched_ids` (ids
    /// just inserted, updated, or removed — [`UnitGraph::nodes`] has
    /// already been updated by the time this runs) could possibly
    /// affect (R3's fix for the quadratic incremental-update cost),
    /// rather than re-resolving every link in the graph, and updates
    /// `forward_index`/`backward_index`/`match_index`/the edge store
    /// in place for only the superseded and fresh edges (R13's fix
    /// for the still-linear-per-edit cost: no cloning or re-indexing
    /// of every edge in the graph on every call):
    ///
    /// - every edge each touched id itself authors is dropped and, if
    ///   the id still exists (an insert or update, not a removal),
    ///   recomputed fresh from its current `links`;
    /// - every edge anywhere in the graph — resolved *or* dangling —
    ///   whose own match key equals a touched id's stem or basename
    ///   (found via [`UnitGraph::match_index`], not a scan) is
    ///   re-resolved against the now-current node set (R10): this
    ///   covers an insert that should pull an edge away from a
    ///   longer already-resolved candidate, an insert that turns a
    ///   unique resolution into a tie, and a removal that lets a tie
    ///   collapse back to its one remaining candidate — not only the
    ///   "already dangling, might now resolve" and "resolved to the
    ///   removed id, flip to dangling" cases the pre-fix version
    ///   handled.
    ///
    /// Every other edge in the graph — the overwhelming majority in a
    /// large source — is left completely untouched: not cloned, not
    /// re-indexed, not even read. See
    /// `tests/integration/watcher_update.rs`'s scaling test.
    ///
    /// Gathers every touched id's own forward_index slots into
    /// `superseded` in one pass over all of `touched_ids` before the
    /// match_index re-resolution pass runs at all (R19), so a
    /// multi-path batch (several entries in `touched_ids` at once —
    /// a folder delete, a git checkout, a multi-file save) gives the
    /// same result regardless of which order `touched_ids` lists
    /// them in, matching a fresh [`UnitGraph::build_from_cache`] on
    /// the same cache state.
    pub(crate) fn recompute_scoped(&mut self, touched_ids: &[String]) {
        let mut superseded: HashSet<usize> = HashSet::new();
        let mut fresh: Vec<Edge> = Vec::new();

        // Every touched id's own forward_index slots are collected
        // into `superseded` *first*, for every touched id, before the
        // match_index re-resolution pass below ever runs (R19). A
        // batched notification can list several paths together (a
        // folder delete, a git checkout, a multi-file save), and the
        // match_index pass for one touched id must never re-queue a
        // *stale* copy of an edge some other touched id in the same
        // batch already authored and is about to replace — e.g.
        // ["Y.md", "X.md"] where X.md (listed second) used to contain
        // `[[Y]]`: processing Y.md's match_index hit before reaching
        // X.md used to re-resolve and re-insert X's *old* edge from
        // its pre-change `to_raw`, with no match key of its own to
        // ever let a later change remove it, because X's own
        // forward_index slots weren't superseded yet at that point in
        // the single combined loop this replaces. Doing this gathering
        // pass up front, covering every touched id regardless of
        // order, makes the result independent of `touched_ids`'
        // ordering, matching a fresh `build_from_cache` on the same
        // cache state.
        for tid in touched_ids {
            if let Some(idxs) = self.forward_index.get(tid) {
                superseded.extend(idxs.iter().copied());
            }
        }

        for tid in touched_ids {
            if let Some(facts) = self.nodes.get(tid).cloned() {
                // Insert or update: (re)resolve this id's own links
                // fresh.
                let index = self.index();
                for (to_raw, kind) in &facts.links {
                    let to = resolve_edge_target(&index, tid, &facts.source, to_raw, kind);
                    let target_offline = matches!(&to, EdgeTarget::Resolved(to_id)
                        if self.nodes.get(to_id).is_some_and(|n| self.offline_sources.contains(&n.source)));
                    let offline = self.offline_sources.contains(&facts.source) || target_offline;
                    fresh.push(Edge {
                        from: tid.clone(),
                        to,
                        to_raw: to_raw.clone(),
                        kind: kind.clone(),
                        offline,
                    });
                }
            }

            // Whether `tid` now exists (an insert, or a rename
            // landing on a path nothing resolved to before) or no
            // longer exists (a removal), every edge in the graph —
            // resolved *or* dangling — whose own match key equals
            // this id's stem or basename might now resolve
            // differently (R10). The key is derived from `tid`'s own
            // path, parsed back out of the id string itself, so this
            // works identically whether `tid` currently has a node or
            // not.
            if let Some((source, rel_path)) = parse_federated_id(tid) {
                let stem = normalize_key(strip_markdown_extension(rel_path));
                let basename = basename_of(&stem).to_string();
                let mut keys = vec![(source.to_string(), stem.clone())];
                if basename != stem {
                    keys.push((source.to_string(), basename));
                }
                for key in keys {
                    let Some(idxs) = self.match_index.get(&key).cloned() else {
                        continue;
                    };
                    for i in idxs {
                        if superseded.contains(&i) {
                            continue;
                        }
                        let Some(old) = self.edges[i].clone() else {
                            continue;
                        };
                        let from_source = self
                            .nodes
                            .get(&old.from)
                            .map(|n| n.source.clone())
                            .unwrap_or_default();
                        let to = {
                            let index = self.index();
                            resolve_edge_target(
                                &index,
                                &old.from,
                                &from_source,
                                &old.to_raw,
                                &old.kind,
                            )
                        };
                        let target_offline = matches!(&to, EdgeTarget::Resolved(to_id)
                            if self.nodes.get(to_id).is_some_and(|n| self.offline_sources.contains(&n.source)));
                        let offline = self.offline_sources.contains(&from_source) || target_offline;
                        superseded.insert(i);
                        fresh.push(Edge {
                            from: old.from,
                            to,
                            to_raw: old.to_raw,
                            kind: old.kind,
                            offline,
                        });
                    }
                }
            }
        }

        // No storage-time de-duplication here either (R16): `fresh`
        // is inserted as-is, one `Edge` per raw link, even when two
        // of them currently resolve to the same target (collapsed
        // only at query time, see `crate::model::dedup_for_query`).
        for i in superseded {
            remove_edge(self, i);
        }
        for edge in fresh {
            insert_edge(self, edge);
        }
    }

    /// Builds a fresh [`UnitGraph`] from every source and unit
    /// currently in `cache` (G4: "built from the PR-2 cache and PR-3
    /// parsed units"). An offline source's units are included too
    /// (E2) — [`crate::UnitMetadata::offline`]/[`Edge::offline`]
    /// reflect that rather than the source being dropped.
    pub fn build_from_cache(cache: &Cache) -> Result<Self, GraphError> {
        let mut graph = UnitGraph::new();

        for source in cache.list_sources()? {
            graph.register_source(&source.name);
            if source.status == SourceStatus::Offline {
                graph.offline_sources.insert(source.name.clone());
            }

            for with_status in cache.units_with_status_for_source(&source.name)? {
                let unit = with_status.unit;
                let facts = load_unit_facts(cache, unit.id, unit.frontmatter_json)?;
                graph.insert_node(&source.name, &unit.rel_path, facts);
            }
        }

        graph.rebuild_edges();
        Ok(graph)
    }

    /// Applies one simulated PR-4 change notification (GP6: "feed a
    /// change-notification event into `UnitGraph`'s incremental-update
    /// path"): re-reads only the named `rel_paths` within `note.source`
    /// fresh from `cache` (a file now present is upserted, a file no
    /// longer present is removed) and recomputes only the edges this
    /// batch of changes could affect (R3), rather than re-ingesting or
    /// re-resolving every source from scratch.
    pub fn apply_change_notification(
        &mut self,
        cache: &Cache,
        note: &ChangeNotification,
    ) -> Result<(), GraphError> {
        let source_row = cache
            .get_source(&note.source)?
            .ok_or_else(|| GraphError::UnknownSource(note.source.clone()))?;

        self.register_source(&note.source);
        let was_offline = self.offline_sources.contains(&note.source);
        let now_offline = source_row.status == SourceStatus::Offline;
        if now_offline {
            self.offline_sources.insert(note.source.clone());
        } else {
            self.offline_sources.remove(&note.source);
        }
        if was_offline != now_offline {
            // R12: a status flip must re-annotate every edge in the
            // graph, not only the ones this batch's `recompute_scoped`
            // happens to touch below — otherwise a unit whose source
            // just went offline/online keeps a stale `Edge::offline`
            // on an edge this notification doesn't otherwise mention
            // (E2: one query giving two different answers about
            // whether the source is offline).
            self.recompute_offline_flags();
        }

        let mut touched_ids = Vec::with_capacity(note.rel_paths.len());
        for rel_path in &note.rel_paths {
            touched_ids.push(federated_id(&note.source, rel_path));
            match cache.get_unit_by_rel_path(source_row.id, rel_path)? {
                Some(unit) => {
                    let facts = load_unit_facts(cache, unit.id, unit.frontmatter_json)?;
                    self.insert_node(&note.source, rel_path, facts);
                }
                None => self.remove_node(&note.source, rel_path),
            }
        }

        self.recompute_scoped(&touched_ids);
        Ok(())
    }
}
