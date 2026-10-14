//! `tests/unit/query.rs`: `get_backlinks`/`get_forward_links`/
//! `get_edges` against a fixed in-memory graph — shape, sort order,
//! and the empty-graph / nonexistent-id edge cases.

use tephra_graph::{EdgeTarget, UnitFacts, UnitGraph};

fn facts_with_links(links: &[&str]) -> UnitFacts {
    UnitFacts {
        frontmatter_json: None,
        tags: Vec::new(),
        blocks: Vec::new(),
        links: links
            .iter()
            .map(|raw| (raw.to_string(), "wikilink".to_string()))
            .collect(),
    }
}

fn empty_facts() -> UnitFacts {
    facts_with_links(&[])
}

#[test]
fn an_empty_graph_answers_every_query_with_an_empty_result_not_an_error() {
    let graph = UnitGraph::new();
    assert_eq!(graph.get_edges(), Vec::new());
    assert_eq!(graph.get_forward_links("@nothing:Nowhere.md"), Vec::new());
    assert_eq!(graph.get_backlinks("@nothing:Nowhere.md"), Vec::new());
    assert!(graph.unit("@nothing:Nowhere.md").is_none());
}

#[test]
fn querying_an_id_with_no_edges_in_a_nonempty_graph_is_empty_not_an_error() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Lonely.md", empty_facts());
    assert!(graph.get_forward_links("@notes:Lonely.md").is_empty());
    assert!(graph.get_backlinks("@notes:Lonely.md").is_empty());
}

#[test]
fn get_backlinks_handles_a_unit_with_many_incoming_edges() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Hub.md", empty_facts());
    for i in 0..50 {
        graph.upsert_unit("notes", &format!("Spoke{i}.md"), facts_with_links(&["Hub"]));
    }

    let backlinks = graph.get_backlinks("@notes:Hub.md");
    assert_eq!(backlinks.len(), 50);
    assert!(graph.get_forward_links("@notes:Hub.md").is_empty());

    // Every spoke is represented exactly once, and only once.
    let mut froms: Vec<&str> = backlinks.iter().map(|e| e.from.as_str()).collect();
    froms.sort();
    froms.dedup();
    assert_eq!(froms.len(), 50);
}

#[test]
fn get_edges_returns_every_edge_in_the_graph_exactly_once_each() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "A.md", facts_with_links(&["B"]));
    graph.upsert_unit("notes", "B.md", facts_with_links(&["A", "C"]));

    let edges = graph.get_edges();
    // A->B, B->A, B->C(dangling): three edges total.
    assert_eq!(edges.len(), 3);
    let dangling_count = edges
        .iter()
        .filter(|e| e.to == EdgeTarget::Dangling)
        .count();
    assert_eq!(dangling_count, 1);
}

#[test]
fn get_edges_is_returned_in_a_stable_sorted_order_across_calls() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "A.md", facts_with_links(&["B", "C"]));
    graph.upsert_unit("notes", "B.md", empty_facts());
    graph.upsert_unit("notes", "C.md", empty_facts());

    let first = graph.get_edges();
    let second = graph.get_edges();
    assert_eq!(first, second);
    assert!(first.len() >= 2);

    // Not just equal across calls — actually sorted: shuffling the
    // result and re-sorting it the same way must be a no-op.
    let mut resorted = first.clone();
    resorted.sort_by(|a, b| {
        (
            a.from.as_str(),
            format!("{:?}", a.to),
            a.kind.as_str(),
            a.to_raw.as_str(),
        )
            .cmp(&(
                b.from.as_str(),
                format!("{:?}", b.to),
                b.kind.as_str(),
                b.to_raw.as_str(),
            ))
    });
    assert_eq!(
        first, resorted,
        "get_edges' own output must already be sorted"
    );
}

/// R2: two unresolved links from the same unit to two genuinely
/// different, nonexistent targets are *not* collapsed into one
/// dangling edge just because both are dangling — only an exact
/// repeat of the identical raw link collapses.
#[test]
fn two_different_dangling_targets_from_the_same_unit_both_survive() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "A.md", facts_with_links(&["Foo", "Bar"]));

    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 2, "{forward:?}");
    assert!(forward.iter().all(|e| e.to == EdgeTarget::Dangling));
    let mut raws: Vec<&str> = forward.iter().map(|e| e.to_raw.as_str()).collect();
    raws.sort();
    assert_eq!(raws, vec!["Bar", "Foo"]);
}

/// R2: `get_edges`, `get_forward_links`, and `get_backlinks` are all
/// de-duplicated against the exact same underlying edge set — a
/// duplicate link shows up exactly once in every one of them, not
/// just in `get_edges`.
#[test]
fn every_query_verb_agrees_on_the_same_deduplicated_edge_count() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "A.md", facts_with_links(&["B", "B", "B"]));
    graph.upsert_unit("notes", "B.md", empty_facts());

    assert_eq!(graph.get_edges().len(), 1);
    assert_eq!(graph.get_forward_links("@notes:A.md").len(), 1);
    assert_eq!(graph.get_backlinks("@notes:B.md").len(), 1);
}

/// T1 review finding: `get_backlinks`'s query-display dedup
/// deliberately ignores `kind`/`to_raw` for a resolved target (R16/R2
/// — right for a display RPC, one row per referring unit), but PR-8's
/// link refactor engine needs to find and rewrite *every* raw
/// occurrence, including a file that links to the same target via two
/// different link kinds. `get_backlinks_all` must not collapse those.
#[test]
fn get_backlinks_all_does_not_collapse_different_kinds_to_the_same_target() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        UnitFacts {
            frontmatter_json: None,
            tags: Vec::new(),
            blocks: Vec::new(),
            links: vec![
                ("Hub".to_string(), "wikilink".to_string()),
                ("Hub.md".to_string(), "markdown".to_string()),
            ],
        },
    );
    graph.upsert_unit("notes", "Hub.md", empty_facts());

    // get_backlinks collapses both raw occurrences into one row,
    // exactly as G4's display RPC intends.
    assert_eq!(graph.get_backlinks("@notes:Hub.md").len(), 1);

    // get_backlinks_all must see both distinct raw occurrences.
    let all = graph.get_backlinks_all("@notes:Hub.md");
    assert_eq!(all.len(), 2, "{all:?}");
    let mut kinds: Vec<&str> = all.iter().map(|e| e.kind.as_str()).collect();
    kinds.sort();
    assert_eq!(kinds, vec!["markdown", "wikilink"]);
}

/// T1: an exact repeat of the identical raw link still has its own
/// indexed `Edge` record (R16: no storage-time dedup) — `get_backlinks_all`
/// returns both; collapsing an exact repeat down to one rewrite
/// dispatch is the refactor engine's own job
/// (`crate::refactor::compute_changes`), not this graph's.
#[test]
fn get_backlinks_all_still_returns_both_records_of_an_exact_repeated_raw_link() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "A.md", facts_with_links(&["B", "B"]));
    graph.upsert_unit("notes", "B.md", empty_facts());

    assert_eq!(graph.get_backlinks("@notes:B.md").len(), 1);
    assert_eq!(graph.get_backlinks_all("@notes:B.md").len(), 2);
}
