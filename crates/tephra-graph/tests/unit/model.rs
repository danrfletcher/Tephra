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
    // R4/GP7: querying backlinks on the now-removed id still finds
    // the edge that still intends to target it (now dangling) — see
    // `get_backlinks_on_a_renamed_notes_old_id_still_finds_every_impacted_incoming_link`
    // for the full cross-source rename scenario this enables.
    let stale_backlinks = graph.get_backlinks("@notes:Two.md");
    assert_eq!(stale_backlinks.len(), 1);
    assert_eq!(stale_backlinks[0].from, "@notes:One.md");
    assert_eq!(stale_backlinks[0].to, EdgeTarget::Dangling);
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

/// R2: a note that links to the same target twice (identical raw
/// text) produces exactly one edge, and the target has exactly one
/// backlink, not two.
#[test]
fn a_duplicate_identical_link_is_represented_once_not_twice() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        facts(&[("B", "wikilink"), ("B", "wikilink")], &[], &[]),
    );
    graph.upsert_unit("notes", "B.md", facts(&[], &[], &[]));

    assert_eq!(graph.get_forward_links("@notes:A.md").len(), 1);
    assert_eq!(graph.get_backlinks("@notes:B.md").len(), 1);
    assert_eq!(graph.get_edges().len(), 1);
}

/// R2: three differently-spelled self-references — a same-file
/// anchor-only link, a bare self-name, and a self-name with an
/// anchor — all resolve to the exact same (from, to) self-edge, so
/// they collapse to one, not three.
#[test]
fn three_different_spellings_of_a_self_reference_collapse_to_one_edge() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        facts(
            &[("A", "wikilink"), ("#H", "wikilink"), ("A#H", "wikilink")],
            &[],
            &[],
        ),
    );

    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:A.md".to_string())
    );
    assert_eq!(graph.get_edges().len(), 1);
}

/// R8: a unit's tags and blocks are replaced, not accumulated, on an
/// update — the same guarantee `updating_a_unit_replaces_its_old_links_rather_than_accumulating_them`
/// already proves for links.
#[test]
fn updating_a_unit_replaces_its_old_tags_and_blocks_rather_than_accumulating_them() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "One.md", facts(&[], &["old-tag"], &["old-block"]));
    assert_eq!(
        graph.unit("@notes:One.md").unwrap().tags,
        vec!["old-tag".to_string()]
    );

    graph.upsert_unit("notes", "One.md", facts(&[], &["new-tag"], &["new-block"]));

    let unit = graph.unit("@notes:One.md").unwrap();
    assert_eq!(unit.tags, vec!["new-tag".to_string()]);
    assert_eq!(unit.blocks, vec!["new-block".to_string()]);
}

/// R8: removing a unit removes its tags/blocks along with the node
/// itself — there is nowhere left to read them back from.
#[test]
fn removing_a_unit_leaves_no_trace_of_its_tags_or_blocks() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "One.md", facts(&[], &["tag"], &["block"]));
    assert!(graph.unit("@notes:One.md").is_some());

    graph.remove_unit("notes", "One.md");

    assert!(graph.unit("@notes:One.md").is_none());
}

/// GP7 (partial): a renamed note's incoming links, from both its own
/// source and a federated cross-source reference, are still
/// discoverable via `get_backlinks` queried with the *old* id after
/// the watcher's remove+upsert pair has applied the rename — the
/// exact candidate set PR-8's refactor preview must consume (R4).
#[test]
fn get_backlinks_on_a_renamed_notes_old_id_still_finds_every_impacted_incoming_link() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "OldDoc.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "N1.md", facts(&[("OldDoc", "wikilink")], &[], &[]));
    graph.upsert_unit(
        "atlas-repo",
        "R1.md",
        facts(&[("@notes:OldDoc", "wikilink")], &[], &[]),
    );

    assert_eq!(graph.get_backlinks("@notes:OldDoc.md").len(), 2);

    // The watcher applies the rename as remove-old, then upsert-new.
    graph.remove_unit("notes", "OldDoc.md");
    graph.upsert_unit("notes", "NewDoc.md", facts(&[], &[], &[]));

    // The old id is gone as a node, but every link that still names
    // it (now dangling) remains discoverable through it, across both
    // sources.
    let stale_backlinks = graph.get_backlinks("@notes:OldDoc.md");
    assert_eq!(stale_backlinks.len(), 2, "{stale_backlinks:?}");
    let mut froms: Vec<&str> = stale_backlinks.iter().map(|e| e.from.as_str()).collect();
    froms.sort();
    assert_eq!(froms, vec!["@atlas-repo:R1.md", "@notes:N1.md"]);
    assert!(stale_backlinks.iter().all(|e| e.to == EdgeTarget::Dangling));

    // The new id has no backlinks yet — nothing has been repointed at
    // it (that's PR-8's `link.apply_refactor` job, out of scope here).
    assert!(graph.get_backlinks("@notes:NewDoc.md").is_empty());
}

/// R9: re-upserting the exact same path (a watcher's debounced edit
/// of a file whose own rel_path didn't change) must not leave a
/// stale duplicate of this id behind in its own stem/basename index
/// entry. Before the fix, a second `upsert_unit` for the same path
/// left `[id, id]` in both indices, so the id became "ambiguous with
/// itself" and anything that later tried to link to it went
/// dangling.
#[test]
fn re_upserting_the_same_path_does_not_make_it_ambiguous_with_itself() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "B.md", facts(&[], &[], &[]));

    // Resolves fine after the first upsert.
    assert_eq!(
        graph
            .resolve_link("[[B]]", Some("notes"))
            .expect("unique after the first upsert")
            .unit
            .id,
        "@notes:B.md"
    );

    // A second upsert of the exact same path — the scenario a
    // watcher's edit of B.md itself produces.
    graph.upsert_unit("notes", "B.md", facts(&[], &[], &[]));

    assert_eq!(
        graph
            .resolve_link("[[B]]", Some("notes"))
            .expect("must still resolve uniquely after a re-upsert, not go ambiguous with itself")
            .unit
            .id,
        "@notes:B.md"
    );

    // And a brand-new unit linking to it afterwards must resolve, not
    // dangle.
    graph.upsert_unit("notes", "A.md", facts(&[("B", "wikilink")], &[], &[]));
    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:B.md".to_string())
    );
}

/// R9: several re-upserts in a row (repeated edits) still leave
/// exactly one index entry, not one per edit.
#[test]
fn many_re_upserts_of_the_same_path_still_resolve_uniquely() {
    let mut graph = UnitGraph::new();
    for _ in 0..5 {
        graph.upsert_unit("notes", "B.md", facts(&[], &[], &[]));
    }

    graph.upsert_unit("notes", "A.md", facts(&[("B", "wikilink")], &[], &[]));
    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:B.md".to_string())
    );
}

/// R11: `get_backlinks` for a *live* unit must never include a
/// dangling edge just because that edge's unresolved raw target
/// happens to share the unit's basename with some other, unrelated
/// unit elsewhere in the source. Unlike the GP7 stale-id case (both
/// `teamA/Shared.md` and `teamB/Shared.md` below are live nodes, not
/// a removed/renamed one), an edge that merely ties on basename with
/// one of them never actually resolved to either — reporting it as
/// one of their backlinks would contradict `get_backlinks`'s own
/// contract ("edges whose target resolved to id").
#[test]
fn get_backlinks_for_a_live_unit_never_includes_an_edge_that_only_shares_its_basename() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "teamA/Shared.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "teamB/Shared.md", facts(&[], &[], &[]));
    graph.upsert_unit("notes", "N.md", facts(&[("Shared", "wikilink")], &[], &[]));

    // The ambiguous tie means N's own link is dangling, not resolved
    // to either team's file.
    let forward = graph.get_forward_links("@notes:N.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(forward[0].to, EdgeTarget::Dangling);

    // Neither teamA's nor teamB's own backlinks may include this
    // dangling edge: it never actually resolved to either of them.
    assert!(
        graph.get_backlinks("@notes:teamA/Shared.md").is_empty(),
        "{:?}",
        graph.get_backlinks("@notes:teamA/Shared.md")
    );
    assert!(
        graph.get_backlinks("@notes:teamB/Shared.md").is_empty(),
        "{:?}",
        graph.get_backlinks("@notes:teamB/Shared.md")
    );
}

/// R17: once a unit that authored edges is removed and its edges'
/// slots are reused by brand-new edges, a stale-id `get_backlinks`
/// lookup (GP7's own query shape) must never surface the *new*,
/// unrelated edge that merely happens to now live in the slot the
/// removed unit's edge used to occupy. Before the fix, `remove_edge`
/// recomputed the edge's `match_index` key by looking up `edge.from`
/// in `nodes` — but by the time it ran, `remove_node` had already
/// deleted that very node, so the key lookup silently returned
/// `None`, the prune was skipped, and the stale slot number lingered
/// in `match_index` for the next `insert_edge` to hand to a
/// completely different edge.
#[test]
fn a_stale_id_lookup_after_remove_and_slot_reuse_never_returns_an_unrelated_edge() {
    let mut graph = UnitGraph::new();

    // X.md links to a target ("Ghost") that never exists — a
    // dangling edge, indexed under match_index[("notes", "ghost"...)].
    graph.upsert_unit("notes", "X.md", facts(&[("Ghost", "wikilink")], &[], &[]));
    assert_eq!(graph.get_backlinks("@notes:Ghost.md").len(), 1);

    // X.md (and its one authored edge) is removed entirely.
    graph.remove_unit("notes", "X.md");
    assert!(graph.unit("@notes:X.md").is_none());

    // A brand-new, unrelated unit is upserted afterwards. If the
    // free'd slot from X.md's removed edge got reused for Y.md's own
    // edge, and the stale match_index entry wasn't pruned, a lookup
    // on the long-gone "Ghost" id would incorrectly find Y's edge.
    graph.upsert_unit("notes", "Y.md", facts(&[("Other", "wikilink")], &[], &[]));

    let stale_lookup = graph.get_backlinks("@notes:Ghost.md");
    assert!(
        stale_lookup.is_empty(),
        "R17: a stale id's backlinks must not resurrect an unrelated edge through a \
         reused match_index slot: {stale_lookup:?}"
    );

    // Y's own edge is still reachable through its own correct key.
    let y_backlinks = graph.get_backlinks("@notes:Other.md");
    assert_eq!(y_backlinks.len(), 1);
    assert_eq!(y_backlinks[0].from, "@notes:Y.md");
}
