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

use crate::model::{federated_id, NodeFacts};
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

/// De-duplication key for one resolved edge target: every raw link
/// that resolves to the *same* real unit collapses to one edge (R2 —
/// e.g. a note containing `[[B]]` twice, or `[[A]]`/`[[#H]]` both
/// resolving to the note's own self-edge), discarding every `to_raw`
/// but the one kept (the lexicographically smallest `(kind, to_raw)`
/// pair, for a deterministic, input-order-independent survivor).
///
/// A dangling edge is *not* collapsed just because another edge from
/// the same unit is also dangling — two different unresolved targets
/// (`[[Foo]]` and `[[Bar]]`, neither existing) are still two distinct,
/// meaningful dangling edges, not duplicates of each other — so a
/// dangling edge's key additionally includes its own `(kind, to_raw)`,
/// collapsing only an exact repeat of the identical raw link.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DedupKey {
    Resolved(String, String),
    Dangling(String, String, String),
}

fn dedup_key(edge: &Edge) -> DedupKey {
    match &edge.to {
        EdgeTarget::Resolved(id) => DedupKey::Resolved(edge.from.clone(), id.clone()),
        EdgeTarget::Dangling => {
            DedupKey::Dangling(edge.from.clone(), edge.kind.clone(), edge.to_raw.clone())
        }
    }
}

/// Collapses `edges` by [`dedup_key`] (R2), keeping, for each key, the
/// edge with the lexicographically smallest `(kind, to_raw)` so the
/// survivor is deterministic regardless of input order.
fn dedup_edges(edges: Vec<Edge>) -> Vec<Edge> {
    let mut by_key: HashMap<DedupKey, Edge> = HashMap::new();
    for edge in edges {
        let key = dedup_key(&edge);
        match by_key.get(&key) {
            Some(existing)
                if (existing.kind.as_str(), existing.to_raw.as_str())
                    <= (edge.kind.as_str(), edge.to_raw.as_str()) => {}
            _ => {
                by_key.insert(key, edge);
            }
        }
    }
    by_key.into_values().collect()
}

/// Rebuilds `forward_index`/`backward_index`/`dangling_index` from
/// `edges` in one O(edges) pass — pure bookkeeping (hash-map
/// insertion), never a resolution call, so this is always cheap
/// regardless of how the edge list itself was produced (a full
/// rebuild or [`UnitGraph::recompute_scoped`]'s targeted recompute).
fn rebuild_indices(graph: &mut UnitGraph) {
    let mut forward_index: HashMap<String, Vec<usize>> = HashMap::new();
    let mut backward_index: HashMap<String, Vec<usize>> = HashMap::new();
    let mut dangling_index: HashMap<(String, String), Vec<usize>> = HashMap::new();

    for (i, edge) in graph.edges.iter().enumerate() {
        forward_index.entry(edge.from.clone()).or_default().push(i);
        match &edge.to {
            EdgeTarget::Resolved(to_id) => {
                backward_index.entry(to_id.clone()).or_default().push(i);
            }
            EdgeTarget::Dangling => {
                if let Some(from_source) = graph.nodes.get(&edge.from).map(|n| n.source.clone()) {
                    let index = graph.index();
                    if let Some(key) = edge_target_scope_key(
                        &index,
                        &edge.from,
                        &from_source,
                        &edge.to_raw,
                        &edge.kind,
                    ) {
                        dangling_index.entry(key).or_default().push(i);
                    }
                }
            }
        }
    }

    graph.forward_index = forward_index;
    graph.backward_index = backward_index;
    graph.dangling_index = dangling_index;
}

impl UnitGraph {
    /// Inserts/replaces one node's facts and its stem/basename index
    /// entries (no edge resolution/rebuild — callers batching
    /// multiple changes call [`UnitGraph::rebuild_edges`] or
    /// [`UnitGraph::recompute_scoped`] once at the end).
    fn insert_node(&mut self, source: &str, rel_path: &str, facts: UnitFacts) {
        self.register_source(source);
        let id = federated_id(source, rel_path);

        // If this id already existed at a different stem (shouldn't
        // happen — `rel_path` is part of the id — but kept defensive)
        // its old index entries are removed first via `remove_node`'s
        // own bookkeeping path, called by every caller that reuses an
        // id for a different path.
        self.by_source
            .entry(source.to_string())
            .or_default()
            .insert(id.clone());

        let stem = normalize_key(strip_markdown_extension(rel_path));
        let basename = basename_of(&stem).to_string();
        self.stem_index
            .entry(source.to_string())
            .or_default()
            .entry(stem)
            .or_default()
            .push(id.clone());
        self.basename_index
            .entry(source.to_string())
            .or_default()
            .entry(basename)
            .or_default()
            .push(id.clone());

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
    /// scratch and rebuilds the forward/backward/dangling indices.
    /// Used for the initial [`UnitGraph::build_from_cache`] (there is
    /// no smaller scope to recompute — every node is new) and as the
    /// one-time cost [`UnitGraph::recompute_scoped`]'s own candidate
    /// gathering never needs to pay again on a later edit. Resolution
    /// itself goes through `stem_index`/`basename_index` (O(1)
    /// average per link, R3) rather than scanning every unit in a
    /// link's scope source, so this full rebuild is O(total links),
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

        self.edges = dedup_edges(edges);
        rebuild_indices(self);
    }

    /// Recomputes edges scoped to exactly what `touched_ids` (ids
    /// just inserted, updated, or removed — [`UnitGraph::nodes`] has
    /// already been updated by the time this runs) could possibly
    /// affect (R3's fix for the quadratic incremental-update cost),
    /// rather than re-resolving every link in the graph:
    ///
    /// - every edge each touched id itself authors is dropped and, if
    ///   the id still exists (an insert or update, not a removal),
    ///   recomputed fresh from its current `links`;
    /// - if a touched id no longer exists (a removal), every edge
    ///   that used to resolve *to* it (found via `backward_index`, not
    ///   a scan) flips to [`EdgeTarget::Dangling`] instead of vanishing
    ///   — the removed node's path/name might still matter to a GP7
    ///   caller via [`UnitGraph::get_backlinks`]'s dangling-edge
    ///   lookup;
    /// - if a touched id now exists (an insert, or a rename landing on
    ///   a path nothing resolved to before), every currently-dangling
    ///   edge whose own match key equals its stem or basename (found
    ///   via `dangling_index`, not a scan) is re-resolved — it may now
    ///   newly resolve to the touched id.
    ///
    /// Every other edge in the graph — the overwhelming majority in a
    /// large source — is left untouched, copied across as-is rather
    /// than re-resolved, so one edit's cost scales with how many
    /// edges that specific change could plausibly affect, not with
    /// the size of the source it lives in. See
    /// `tests/integration/watcher_update.rs`'s scaling test.
    pub(crate) fn recompute_scoped(&mut self, touched_ids: &[String]) {
        let mut superseded: HashSet<usize> = HashSet::new();
        let mut fresh: Vec<Edge> = Vec::new();

        for tid in touched_ids {
            // Every edge this id used to author is replaced wholesale
            // below (dropped here, regenerated fresh if the id still
            // exists) — never left stale alongside a new version.
            if let Some(idxs) = self.forward_index.get(tid) {
                superseded.extend(idxs.iter().copied());
            }

            match self.nodes.get(tid).cloned() {
                Some(facts) => {
                    // Insert or update: (re)resolve this id's own
                    // links fresh.
                    let index = self.index();
                    for (to_raw, kind) in &facts.links {
                        let to = resolve_edge_target(&index, tid, &facts.source, to_raw, kind);
                        let target_offline = matches!(&to, EdgeTarget::Resolved(to_id)
                            if self.nodes.get(to_id).is_some_and(|n| self.offline_sources.contains(&n.source)));
                        let offline =
                            self.offline_sources.contains(&facts.source) || target_offline;
                        fresh.push(Edge {
                            from: tid.clone(),
                            to,
                            to_raw: to_raw.clone(),
                            kind: kind.clone(),
                            offline,
                        });
                    }

                    // Insert, or a rename landing on a path nothing
                    // resolved to before: any currently-dangling edge
                    // whose match key equals this id's stem/basename
                    // might now resolve.
                    let stem = normalize_key(strip_markdown_extension(&facts.rel_path));
                    let basename = basename_of(&stem).to_string();
                    let mut keys = vec![(facts.source.clone(), stem.clone())];
                    if basename != stem {
                        keys.push((facts.source.clone(), basename));
                    }
                    for key in keys {
                        if let Some(idxs) = self.dangling_index.get(&key).cloned() {
                            for i in idxs {
                                if superseded.contains(&i) {
                                    continue;
                                }
                                let old = self.edges[i].clone();
                                let from_source = self
                                    .nodes
                                    .get(&old.from)
                                    .map(|n| n.source.clone())
                                    .unwrap_or_default();
                                let to = resolve_edge_target(
                                    &index,
                                    &old.from,
                                    &from_source,
                                    &old.to_raw,
                                    &old.kind,
                                );
                                let target_offline = matches!(&to, EdgeTarget::Resolved(to_id)
                                    if self.nodes.get(to_id).is_some_and(|n| self.offline_sources.contains(&n.source)));
                                let offline =
                                    self.offline_sources.contains(&from_source) || target_offline;
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
                None => {
                    // Removal: every edge that used to resolve to
                    // this id flips to dangling instead of vanishing
                    // (R4/GP7 — `get_backlinks` can still find it by
                    // the old path via `dangling_index`).
                    if let Some(idxs) = self.backward_index.get(tid).cloned() {
                        for i in idxs {
                            if superseded.contains(&i) {
                                continue;
                            }
                            let mut edge = self.edges[i].clone();
                            let from_offline = self
                                .nodes
                                .get(&edge.from)
                                .is_some_and(|n| self.offline_sources.contains(&n.source));
                            edge.to = EdgeTarget::Dangling;
                            edge.offline = from_offline;
                            superseded.insert(i);
                            fresh.push(edge);
                        }
                    }
                }
            }
        }

        let mut edges: Vec<Edge> = self
            .edges
            .iter()
            .enumerate()
            .filter(|(i, _)| !superseded.contains(i))
            .map(|(_, e)| e.clone())
            .collect();
        edges.extend(fresh);

        self.edges = dedup_edges(edges);
        rebuild_indices(self);
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
        if source_row.status == SourceStatus::Offline {
            self.offline_sources.insert(note.source.clone());
        } else {
            self.offline_sources.remove(&note.source);
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
