//! Builds and incrementally updates a [`crate::UnitGraph`] from
//! `tephra-cache` rows (G4: "built from the PR-2 cache and PR-3
//! parsed units"). Everything here is a plain function/method over
//! owned data plus a borrowed `&tephra_cache::Cache` — no filesystem
//! walking, no watcher, no RPC (F3): the caller (PR-4's registry
//! today, a later PR's broadcaster/RPC layer eventually) already has
//! a `Cache` open and knows which source/paths changed.

use std::collections::HashMap;
use std::fmt;

use tephra_cache::{Cache, CacheError, SourceStatus};

use crate::model::{federated_id, NodeFacts};
use crate::resolver::resolve_edge_target;
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
/// on, never the other way around. A real caller (a later PR) is
/// responsible for turning a `DaemonEvent`'s absolute `PathBuf`s into
/// these source-relative strings before calling
/// [`UnitGraph::apply_change_notification`].
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

impl UnitGraph {
    /// Inserts/replaces one node's facts (no resolution, no index
    /// rebuild — callers batching multiple changes call
    /// [`UnitGraph::rebuild_edges`] once at the end).
    fn insert_node(&mut self, source: &str, rel_path: &str, facts: UnitFacts) {
        self.register_source(source);
        let id = federated_id(source, rel_path);
        self.by_source
            .entry(source.to_string())
            .or_default()
            .insert(id.clone());
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

    /// Removes one node's facts (no index rebuild — see
    /// [`UnitGraph::insert_node`]).
    fn remove_node(&mut self, source: &str, rel_path: &str) {
        let id = federated_id(source, rel_path);
        self.nodes.remove(&id);
        if let Some(set) = self.by_source.get_mut(source) {
            set.remove(&id);
        }
    }

    /// Inserts or replaces one unit's facts, then recomputes the
    /// edge layer. For a caller applying many changes at once,
    /// prefer [`UnitGraph::apply_change_notification`] or
    /// [`UnitGraph::build_from_cache`], which batch the (potentially
    /// O(edges)) recompute into a single call rather than paying it
    /// once per unit.
    pub fn upsert_unit(&mut self, source: &str, rel_path: &str, facts: UnitFacts) {
        self.insert_node(source, rel_path, facts);
        self.rebuild_edges();
    }

    /// Removes one unit and recomputes the edge layer — see
    /// [`UnitGraph::upsert_unit`]'s note on batching.
    pub fn remove_unit(&mut self, source: &str, rel_path: &str) {
        self.remove_node(source, rel_path);
        self.rebuild_edges();
    }

    /// Recomputes every edge from the current `nodes` map and rebuilds
    /// the forward/backward indices. This never re-reads
    /// `tephra-cache` or re-parses any file's content — it only
    /// re-derives the *resolution* of links the graph's own nodes
    /// already carry in memory (`NodeFacts::links`), which is why an
    /// edit to one unit ([`UnitGraph::upsert_unit`]) reflects in the
    /// edges without the "full graph rebuild" (re-ingest from the
    /// database) GP6's incremental-update contract rules out.
    ///
    /// This does re-resolve every node's links on every call, not
    /// just the changed node's — necessary for correctness, since
    /// adding, renaming, or removing one unit can retroactively
    /// resolve (or dangle) another unit's otherwise-unrelated link
    /// that happens to target the same name. [`UnitGraph::get_backlinks`]
    /// stays bounded-time regardless (it looks up the resulting
    /// index, never re-derives it) — only *updates* pay this cost,
    /// matching the spec's large-fan-in requirement, which is phrased
    /// in terms of query time, not update time.
    pub(crate) fn rebuild_edges(&mut self) {
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort();

        let mut edges: Vec<Edge> = Vec::new();
        for id in ids {
            let facts = self.nodes.get(id).expect("id came from nodes.keys()");
            for (to_raw, kind) in &facts.links {
                let to = resolve_edge_target(
                    &self.nodes,
                    &self.by_source,
                    &self.known_sources,
                    id,
                    &facts.source,
                    to_raw,
                    kind,
                );
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

        let mut forward_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut backward_index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, edge) in edges.iter().enumerate() {
            forward_index.entry(edge.from.clone()).or_default().push(i);
            if let EdgeTarget::Resolved(to_id) = &edge.to {
                backward_index.entry(to_id.clone()).or_default().push(i);
            }
        }

        self.edges = edges;
        self.forward_index = forward_index;
        self.backward_index = backward_index;
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
    /// longer present is removed) and recomputes the edge layer once,
    /// rather than re-ingesting every source from scratch.
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

        for rel_path in &note.rel_paths {
            match cache.get_unit_by_rel_path(source_row.id, rel_path)? {
                Some(unit) => {
                    let facts = load_unit_facts(cache, unit.id, unit.frontmatter_json)?;
                    self.insert_node(&note.source, rel_path, facts);
                }
                None => self.remove_node(&note.source, rel_path),
            }
        }

        self.rebuild_edges();
        Ok(())
    }
}
