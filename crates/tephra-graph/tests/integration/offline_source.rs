//! `tests/integration/offline_source.rs` (spec: "mark a source stale
//! via an in-memory `SourceRegistry` test double from PR-4, confirm
//! graph queries degrade to the offline-marker behavior instead of
//! failing or omitting edges"). The double below stands in for the
//! shape of PR-4's `tephrad::registry::SourceRegistry` (it reports a
//! source's current lifecycle status) without this crate depending
//! on the `tephrad` crate itself — `tephra-graph` sits strictly
//! between `tephra-cache`/`tephra-parser` and later PRs (F3), never
//! the other way, so wiring to a *real* `SourceRegistry` is this
//! file's mock, not a `tephrad` dependency.

use std::collections::HashMap;

use tempfile::tempdir;

use crate::fixtures::{load_federated_fixtures, open_temp_cache};
use tephra_cache::SourceStatus;
use tephra_graph::{EdgeTarget, UnitGraph};

/// An in-memory stand-in for PR-4's `SourceRegistry`: tracks each
/// known source's current lifecycle status, independent of
/// `tephra-cache`'s own `sources` table, the way a real
/// `SourceRegistry` watching the filesystem would notice a source go
/// offline before (or instead of) the cache's row being updated.
struct MockSourceRegistry {
    statuses: HashMap<String, SourceStatus>,
}

impl MockSourceRegistry {
    fn new() -> Self {
        MockSourceRegistry {
            statuses: HashMap::new(),
        }
    }

    fn mark(&mut self, source: &str, status: SourceStatus) {
        self.statuses.insert(source.to_string(), status);
    }

    fn is_offline(&self, source: &str) -> bool {
        self.statuses.get(source) == Some(&SourceStatus::Offline)
    }
}

/// Applies every source this mock registry knows about onto `graph`
/// via [`UnitGraph::set_source_offline`] — the real, supported way a
/// caller observing PR-4's registry would keep a `UnitGraph` in
/// sync, without the registry or the graph ever touching the
/// filesystem.
fn sync_registry_into_graph(registry: &MockSourceRegistry, graph: &mut UnitGraph) {
    for source in registry.statuses.keys() {
        graph.set_source_offline(source, registry.is_offline(source));
    }
}

#[test]
fn marking_a_source_offline_through_the_registry_double_degrades_queries_without_dropping_edges() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);

    let mut graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
    assert!(!graph.unit("@atlas-repo:README.md").unwrap().offline);

    let mut registry = MockSourceRegistry::new();
    registry.mark("atlas-repo", SourceStatus::Offline);
    sync_registry_into_graph(&registry, &mut graph);

    // The node is still present and queryable — never dropped, never
    // an error — just annotated.
    let readme = graph
        .unit("@atlas-repo:README.md")
        .expect("unit must still be queryable while its source is offline");
    assert!(readme.offline);

    // Forward and backward edges touching the now-offline source are
    // still returned in full, each annotated offline, not omitted.
    let forward = graph.get_forward_links("@atlas-repo:README.md");
    assert_eq!(forward.len(), 2, "both outgoing edges remain");
    assert!(forward.iter().all(|e| e.offline));

    let backlinks = graph.get_backlinks("@atlas-repo:README.md");
    assert_eq!(backlinks.len(), 2, "both incoming edges remain");
    assert!(backlinks.iter().all(|e| e.offline));

    // The online "notes" source's own edges that don't touch
    // atlas-repo are unaffected.
    let journal_self_link = graph
        .get_forward_links("@notes:Journal.md")
        .into_iter()
        .find(|e| e.to == EdgeTarget::Resolved("@notes:Journal.md".to_string()))
        .expect("Journal's self-link to itself");
    assert!(!journal_self_link.offline);
}

#[test]
fn a_source_the_registry_double_never_mentions_is_treated_as_online() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);

    let mut graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
    let registry = MockSourceRegistry::new();
    sync_registry_into_graph(&registry, &mut graph);

    assert!(!graph.is_source_offline("notes"));
    assert!(!graph.is_source_offline("atlas-repo"));
}

#[test]
fn remounting_a_source_through_the_registry_double_clears_the_offline_annotation() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);

    let mut graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
    let mut registry = MockSourceRegistry::new();
    registry.mark("atlas-repo", SourceStatus::Offline);
    sync_registry_into_graph(&registry, &mut graph);
    assert!(graph.unit("@atlas-repo:README.md").unwrap().offline);

    registry.mark("atlas-repo", SourceStatus::Active);
    sync_registry_into_graph(&registry, &mut graph);

    assert!(!graph.unit("@atlas-repo:README.md").unwrap().offline);
    assert!(graph
        .get_forward_links("@atlas-repo:README.md")
        .iter()
        .all(|e| !e.offline));
}
