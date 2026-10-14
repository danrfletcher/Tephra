//! Spec requirement: "end-to-end: build a `UnitGraph` from a vault
//! fixture plus a federated-repo fixture (two sources, cross-source
//! wikilinks), and assert resolution, backlinks, and forward links
//! are correct." Fixtures seeded through PR-2's cache loader
//! (`tephra_cache::Cache`'s typed insert API) and parsed by PR-3's
//! real `tephra_parser::parse_unit_bytes`, not hand-built SQL or
//! synthetic graph data — see `tests/fixtures/mod.rs`.

use tempfile::tempdir;

use crate::fixtures::{load_federated_fixtures, open_temp_cache};
use tephra_graph::{EdgeTarget, UnitGraph};

#[test]
fn builds_a_federated_graph_from_two_real_sources_and_resolves_across_them() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);

    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let mut sources = graph.known_sources();
    sources.sort();
    assert_eq!(sources, vec!["atlas-repo".to_string(), "notes".to_string()]);

    // README.md (atlas-repo) links to one.md (same source) and
    // @notes:Journal (cross-source).
    let readme_forward = graph.get_forward_links("@atlas-repo:README.md");
    assert_eq!(readme_forward.len(), 2);
    let resolved_targets: Vec<&str> = readme_forward
        .iter()
        .filter_map(|e| match &e.to {
            EdgeTarget::Resolved(id) => Some(id.as_str()),
            EdgeTarget::Dangling => None,
        })
        .collect();
    assert!(resolved_targets.contains(&"@atlas-repo:one.md"));
    assert!(resolved_targets.contains(&"@notes:Journal.md"));

    // one.md and Journal.md both link to README.md: a real
    // backlink round-trip plus a cross-source backlink.
    let readme_backlinks = graph.get_backlinks("@atlas-repo:README.md");
    assert_eq!(readme_backlinks.len(), 2);
    let mut backlink_froms: Vec<&str> = readme_backlinks.iter().map(|e| e.from.as_str()).collect();
    backlink_froms.sort();
    assert_eq!(
        backlink_froms,
        vec!["@atlas-repo:one.md", "@notes:Journal.md"]
    );

    // Journal.md's own outgoing links: cross-source to README, a
    // self-link, and a dangling target. The self-link by name
    // ([[Journal]]) and the same-file anchor self-link ([[#Journal]])
    // both resolve to the exact same (from, to) pair, so they
    // collapse to one edge (R2: "a self-reference is represented once
    // per direction, no duplicate edges") rather than two — three
    // edges total, exactly one dangling.
    let journal_forward = graph.get_forward_links("@notes:Journal.md");
    assert_eq!(journal_forward.len(), 3);
    let dangling_count = journal_forward
        .iter()
        .filter(|e| e.to == EdgeTarget::Dangling)
        .count();
    assert_eq!(dangling_count, 1, "exactly the [[Does Not Exist]] link");

    // Journal.md's #tags made it through the real parser into the
    // graph's node metadata.
    let journal = graph
        .unit("@notes:Journal.md")
        .expect("journal node exists");
    assert_eq!(journal.tags, vec!["journal".to_string()]);
}

#[test]
fn an_ambiguous_basename_target_in_the_repo_fixture_is_reported_as_ambiguous() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let result = graph.resolve_link("[[Guide]]", Some("atlas-repo"));
    match result {
        Err(tephra_graph::ResolveError::Ambiguous { mut candidates, .. }) => {
            candidates.sort();
            assert_eq!(
                candidates,
                vec![
                    "@atlas-repo:api/Guide.md".to_string(),
                    "@atlas-repo:lib/Guide.md".to_string(),
                ],
                "E4: every tied candidate is listed, not just the count"
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn a_non_ascii_space_containing_filename_resolves_by_its_bare_name() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let resolved = graph
        .resolve_link("[[café notes]]", Some("notes"))
        .expect("should resolve the unicode-named fixture file");
    assert_eq!(resolved.unit.id, "@notes:unicode/café notes.md");

    // And the real block anchor parsed out of that file is present
    // on the node's metadata.
    assert_eq!(resolved.unit.blocks, vec!["unicode-block".to_string()]);
}

#[test]
fn an_offline_source_marks_both_its_units_and_their_edges_offline() {
    let dir = tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_federated_fixtures(&cache);
    cache
        .transition_source_status("atlas-repo", tephra_cache::SourceStatus::Offline)
        .expect("active -> offline is a legal transition");

    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let readme = graph
        .unit("@atlas-repo:README.md")
        .expect("still present while offline");
    assert!(readme.offline);

    // The cross-source edge from the (online) notes source into the
    // now-offline atlas-repo source is annotated, not dropped.
    let journal_forward = graph.get_forward_links("@notes:Journal.md");
    let to_readme = journal_forward
        .iter()
        .find(|e| e.to == EdgeTarget::Resolved("@atlas-repo:README.md".to_string()))
        .expect("edge to README still present");
    assert!(to_readme.offline);
}
