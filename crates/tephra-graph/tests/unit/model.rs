//! `tests/unit/model.rs`: `UnitGraph` node/edge construction from
//! fixture parsed units (in-memory `UnitFacts`, no real filesystem or
//! `Cache` — the pure-construction layer). Tag, block, forward-link,
//! and backlink indices stay consistent after insert, update, and
//! remove.

use tephra_graph::{EdgeTarget, UnitFacts, UnitGraph};

fn facts(links: &[(&str, &str)], tags: &[&str], blocks: &[&str]) -> UnitFacts {
    UnitFacts {
        frontmatter_json: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        blocks: blocks.iter().map(|b| b.to_string()).collect(),
        links: links
            .iter()
            .map(|(raw, kind)| (raw.to_string(), kind.to_string()))
            .collect(),
    }
}

#[test]
fn inserting_a_unit_creates_a_node_carrying_its_tags_and_blocks() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "One.md",
        facts(&[], &["project", "urgent"], &["block1"]),
    );

    let unit = graph.unit("@notes:One.md").expect("node exists");
    assert_eq!(unit.source, "notes");
    assert_eq!(unit.rel_path, "One.md");
    assert_eq!(unit.tags, vec!["project".to_string(), "urgent".to_string()]);
    assert_eq!(unit.blocks, vec!["block1".to_string()]);
    assert!(!unit.offline);
}

#[test]
fn inserting_two_linked_units_creates_one_resolved_edge_in_both_indices() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "One.md", facts(&[("Two", "wikilink")], &[], &[]));
    graph.upsert_unit("notes", "Two.md", facts(&[], &[], &[]));

    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Two.md".to_string())
    );

    let backlinks = graph.get_backlinks("@notes:Two.md");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].from, "@notes:One.md");

    // No stray edges show up on the wrong side.
    assert!(graph.get_backlinks("@notes:One.md").is_empty());
    assert!(graph.get_forward_links("@notes:Two.md").is_empty());
}

#[test]
fn updating_a_unit_replaces_its_old_links_rather_than_accumulating_them() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Two.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "Three.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "One.md", facts(&[("Two", "wikilink")], &[], &[]));
    assert_eq!(graph.get_forward_links("@notes:One.md").len(), 1);

    // Editing One.md to link to Three instead of Two: the old edge to
    // Two must disappear, not linger alongside the new one.
    graph.upsert_unit("notes", "One.md", facts(&[("Three", "wikilink")], &[], &[]));

    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Three.md".to_string())
    );
    assert!(graph.get_backlinks("@notes:Two.md").is_empty());
    assert_eq!(graph.get_backlinks("@notes:Three.md").len(), 1);
}

#[test]
fn removing_a_unit_removes_its_node_and_every_edge_that_targeted_it() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Two.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "One.md", facts(&[("Two", "wikilink")], &[], &[]));
    assert!(graph.unit("@notes:Two.md").is_some());
    assert_eq!(graph.get_backlinks("@notes:Two.md").len(), 1);

    graph.remove_unit("notes", "Two.md");

    assert!(graph.unit("@notes:Two.md").is_none());
    // The dangling edge is still represented (never silently
    // resolved as if current) but now targets nothing.
    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(forward[0].to, EdgeTarget::Dangling);
    assert!(graph.get_backlinks("@notes:Two.md").is_empty());
}

#[test]
fn a_renamed_target_retroactively_resolves_a_previously_dangling_edge() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "One.md", facts(&[("Two", "wikilink")], &[], &[]));
    assert_eq!(
        graph.get_forward_links("@notes:One.md")[0].to,
        EdgeTarget::Dangling
    );

    // Two.md didn't exist yet; now it's added (e.g. moved into place
    // under its new name) — the existing edge must update without
    // re-upserting One.md.
    graph.upsert_unit("notes", "Two.md", facts(&[], &[], &[]));

    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Two.md".to_string())
    );
}

#[test]
fn a_self_referential_link_is_exactly_one_edge_in_both_directions() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "Journal.md",
        facts(&[("Journal", "wikilink")], &[], &[]),
    );

    let forward = graph.get_forward_links("@notes:Journal.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Journal.md".to_string())
    );

    let backlinks = graph.get_backlinks("@notes:Journal.md");
    assert_eq!(backlinks.len(), 1);

    let all_edges = graph.get_edges();
    assert_eq!(
        all_edges.len(),
        1,
        "a self-link is one edge, not duplicated"
    );
}

#[test]
fn an_empty_target_wikilink_resolves_to_the_referring_unit_itself() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "Journal.md",
        facts(&[("#Heading", "wikilink")], &[], &[]),
    );

    let forward = graph.get_forward_links("@notes:Journal.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Journal.md".to_string())
    );
}
