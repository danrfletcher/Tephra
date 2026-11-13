//! `tests/unit/block_links.rs` (PR-13.F1, G13/G16, E7/E8): block links
//! `[[path#^id]]`, their backlinks and the line a link reports. Every
//! graph here is built from real files: parsed by `tephra-parser`,
//! written through `tephra-cache`'s typed API and read back by
//! `UnitGraph::build_from_cache`, so the IDs, lines and `to_raw`
//! shapes are exactly what a scan produces.

use tephra_graph::{BlockLine, BlockRef, EdgeTarget, ResolveError, UnitGraph};

use crate::fixtures::{load_files_into_cache, open_temp_cache};

/// Builds a graph of `source` holding `files`, plus `others`
/// (`(source, files)`), each of which is its own source.
fn graph_of(
    files: &[(&str, &str)],
    others: &[(&str, &[(&str, &str)])],
) -> (tempfile::TempDir, UnitGraph) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_files_into_cache(&cache, "s", files);
    for (name, files) in others {
        load_files_into_cache(&cache, name, files);
    }
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
    (dir, graph)
}

fn block(id: &str, line: i64, moved: bool) -> Option<BlockRef> {
    Some(BlockRef {
        id: id.to_string(),
        line,
        moved,
    })
}

#[test]
fn a_block_link_resolves_to_the_block_in_the_named_file() {
    let (_dir, graph) = graph_of(
        &[
            ("A.md", "# A\n\nA claim. ^abc123\n"),
            ("B.md", "see [[A#^abc123]]\n"),
        ],
        &[],
    );
    let result = graph.resolve_link("[[A#^abc123]]", Some("s")).unwrap();
    assert_eq!(result.unit.id, "@s:A.md");
    assert_eq!(result.block, block("abc123", 3, false));
}

#[test]
fn a_heading_anchor_and_a_plain_link_carry_no_block() {
    let (_dir, graph) = graph_of(&[("A.md", "# Heading\n\nText ^abc123\n")], &[]);
    assert_eq!(
        graph
            .resolve_link("[[A#Heading]]", Some("s"))
            .unwrap()
            .block,
        None
    );
    assert_eq!(graph.resolve_link("[[A]]", Some("s")).unwrap().block, None);
}

#[test]
fn a_block_link_to_a_missing_typed_id_is_dangling_even_if_another_file_has_it() {
    // `^summary` is not a Tephra-format ID (not 6 chars), so it is
    // never searched for in other files.
    let (_dir, graph) = graph_of(
        &[("A.md", "nothing here\n"), ("B.md", "Text ^summary\n")],
        &[],
    );
    let err = graph.resolve_link("[[A#^summary]]", Some("s")).unwrap_err();
    assert!(matches!(err, ResolveError::Unresolved { .. }), "{err:?}");
    // The file exists: the message says the block is missing, and not
    // "elsewhere" because a typed ID is never searched for there.
    assert_eq!(
        err.to_string(),
        "no block ^summary in \"A\" in source \"s\""
    );
}

#[test]
fn a_moved_tephra_id_is_found_in_the_one_other_file_of_the_source() {
    let (_dir, graph) = graph_of(
        &[
            ("Old.md", "the block moved out of here\n"),
            ("New.md", "\nline two\n\nMoved text ^abc123\n"),
        ],
        &[],
    );
    let result = graph.resolve_link("[[Old#^abc123]]", Some("s")).unwrap();
    assert_eq!(result.unit.id, "@s:New.md");
    assert_eq!(result.block, block("abc123", 4, true));
}

#[test]
fn a_moved_id_is_not_searched_for_in_another_source() {
    let (_dir, graph) = graph_of(
        &[("Old.md", "gone\n")],
        &[("t", &[("New.md", "Moved ^abc123\n")])],
    );
    let err = graph
        .resolve_link("[[Old#^abc123]]", Some("s"))
        .unwrap_err();
    assert!(matches!(err, ResolveError::Unresolved { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "no block ^abc123 in \"Old\" or elsewhere in source \"s\""
    );
}

#[test]
fn an_id_twice_in_the_named_file_is_ambiguous_and_lists_both_lines() {
    let (_dir, graph) = graph_of(&[("A.md", "one ^abc123\n\ntwo ^abc123\n")], &[]);
    let err = graph.resolve_link("[[A#^abc123]]", Some("s")).unwrap_err();
    match err {
        ResolveError::Ambiguous {
            candidates, lines, ..
        } => {
            assert_eq!(candidates, vec!["@s:A.md"]);
            assert_eq!(
                lines,
                vec![
                    BlockLine {
                        unit: "@s:A.md".into(),
                        line: 1
                    },
                    BlockLine {
                        unit: "@s:A.md".into(),
                        line: 3
                    },
                ]
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn a_moved_id_in_two_files_is_ambiguous_and_lists_every_occurrence() {
    let (_dir, graph) = graph_of(
        &[
            ("Old.md", "x\n"),
            ("B.md", "a\n\nb ^abc123\n"),
            ("C.md", "c ^abc123\n"),
        ],
        &[],
    );
    let err = graph
        .resolve_link("[[Old#^abc123]]", Some("s"))
        .unwrap_err();
    match err {
        ResolveError::Ambiguous {
            candidates, lines, ..
        } => {
            assert_eq!(candidates, vec!["@s:B.md", "@s:C.md"]);
            assert_eq!(
                lines,
                vec![
                    BlockLine {
                        unit: "@s:B.md".into(),
                        line: 3
                    },
                    BlockLine {
                        unit: "@s:C.md".into(),
                        line: 1
                    },
                ]
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn a_block_id_in_a_code_file_resolves_to_the_definition_line() {
    let (_dir, graph) = graph_of(
        &[(
            "lib.rs",
            "// header\n\n/// Docs.\npub enum Reason { // ^k3f9a2\n    A,\n}\n",
        )],
        &[("n", &[("note.md", "[[@s:lib.rs#^k3f9a2]]\n")])],
    );
    let result = graph
        .resolve_link("[[@s:lib.rs#^k3f9a2]]", Some("n"))
        .unwrap();
    assert_eq!(result.unit.id, "@s:lib.rs");
    assert_eq!(result.block, block("k3f9a2", 4, false));
}

#[test]
fn a_link_reports_the_line_it_is_on() {
    let (_dir, graph) = graph_of(
        &[
            ("A.md", "# A\n\nfirst [[B]]\n\nsecond [[B]] and [x](B.md)\n"),
            ("B.md", "b\n"),
        ],
        &[],
    );
    let lines: Vec<Option<i64>> = graph
        .get_backlinks("@s:B.md")
        .iter()
        .map(|e| e.line)
        .collect();
    // Line 5 holds two links to B on one line: one row for the pair.
    assert_eq!(lines, vec![Some(3), Some(5)]);
}

#[test]
fn a_link_in_a_code_comment_reports_the_comment_line() {
    let (_dir, graph) = graph_of(
        &[
            ("B.md", "b\n"),
            (
                "lib.rs",
                "fn main() {}\n\n// see [[B]] for why\nfn x() {}\n",
            ),
        ],
        &[],
    );
    let backlinks = graph.get_backlinks("@s:B.md");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].from, "@s:lib.rs");
    assert_eq!(backlinks[0].line, Some(3));
}

#[test]
fn two_links_to_a_unit_on_different_lines_are_two_backlinks_but_one_forward_link() {
    let (_dir, graph) = graph_of(&[("A.md", "[[B]]\n\n[[B]]\n"), ("B.md", "b\n")], &[]);
    assert_eq!(graph.get_backlinks("@s:B.md").len(), 2);
    assert_eq!(graph.get_forward_links("@s:A.md").len(), 1);
    assert_eq!(graph.get_edges().len(), 1);
    assert_eq!(graph.get_backlinks_all("@s:B.md").len(), 2);
}

#[test]
fn block_backlinks_list_only_links_to_that_block() {
    let (_dir, graph) = graph_of(
        &[
            ("A.md", "first ^abc123\n\nsecond ^def456\n"),
            ("L1.md", "to first [[A#^abc123]]\n"),
            ("L2.md", "to second [[A#^def456]]\n"),
            ("L3.md", "to the file [[A]]\n\nand again [[A#^abc123]]\n"),
        ],
        &[],
    );
    let to_first: Vec<(String, Option<i64>)> = graph
        .get_backlinks("@s:A.md#^abc123")
        .into_iter()
        .map(|e| (e.from, e.line))
        .collect();
    assert_eq!(
        to_first,
        vec![
            ("@s:L1.md".to_string(), Some(1)),
            ("@s:L3.md".to_string(), Some(3))
        ]
    );
    // The unit id still returns every link to the unit, anchored or not.
    assert_eq!(graph.get_backlinks("@s:A.md").len(), 4);
}

#[test]
fn a_link_written_to_the_old_file_is_a_backlink_of_the_moved_block() {
    let (_dir, graph) = graph_of(
        &[
            ("Old.md", "nothing left\n"),
            ("New.md", "Moved text ^abc123\n"),
            ("Ref.md", "\nsee [[Old#^abc123]]\n"),
        ],
        &[],
    );
    let backlinks = graph.get_backlinks("@s:New.md#^abc123");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].from, "@s:Ref.md");
    assert_eq!(backlinks[0].line, Some(2));
    // The link is to Old.md's anchor, which no longer holds the block.
    assert!(graph.get_backlinks("@s:Old.md#^abc123").is_empty());
}

#[test]
fn an_ambiguous_or_dangling_block_link_is_nobodys_backlink() {
    let (_dir, graph) = graph_of(
        &[
            ("A.md", "x ^abc123\n"),
            ("B.md", "y ^abc123\n"),
            ("Ref.md", "[[Gone#^abc123]] and [[A#^zzz999]]\n"),
        ],
        &[],
    );
    assert!(graph.get_backlinks("@s:A.md#^abc123").is_empty());
    assert!(graph.get_backlinks("@s:B.md#^abc123").is_empty());
}

#[test]
fn a_block_backlink_for_a_unit_not_in_the_graph_is_empty() {
    let (_dir, graph) = graph_of(&[("A.md", "x ^abc123\n"), ("R.md", "[[A#^abc123]]\n")], &[]);
    assert!(graph.get_backlinks("@s:Nope.md#^abc123").is_empty());
}

#[test]
fn block_backlinks_follow_a_live_upsert_and_remove() {
    let (_dir, mut graph) = graph_of(&[("A.md", "x ^abc123\n"), ("R.md", "[[A#^abc123]]\n")], &[]);
    assert_eq!(graph.get_backlinks("@s:A.md#^abc123").len(), 1);

    // The block moves to B.md: A.md loses it, B.md gains it.
    graph.upsert_unit("s", "A.md", Default::default());
    assert!(graph.get_backlinks("@s:A.md#^abc123").is_empty());
    graph.upsert_unit(
        "s",
        "B.md",
        tephra_graph::UnitFacts {
            blocks: vec![tephra_graph::BlockFact {
                id: "abc123".into(),
                line: 7,
            }],
            ..Default::default()
        },
    );
    let moved = graph.get_backlinks("@s:B.md#^abc123");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].from, "@s:R.md");
    assert_eq!(moved[0].line, Some(1));
    assert_eq!(
        graph
            .resolve_link("[[A#^abc123]]", Some("s"))
            .unwrap()
            .block,
        block("abc123", 7, true)
    );

    // Removing the linking file drops its backlink.
    graph.remove_unit("s", "R.md");
    assert!(graph.get_backlinks("@s:B.md#^abc123").is_empty());
}

#[test]
fn a_markdown_block_link_is_a_block_backlink_too() {
    let (_dir, graph) = graph_of(
        &[
            ("A.md", "x ^abc123\n"),
            ("R.md", "a\n\n[t](A.md#^abc123)\n"),
        ],
        &[],
    );
    let backlinks = graph.get_backlinks("@s:A.md#^abc123");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].line, Some(3));
    assert!(matches!(backlinks[0].to, EdgeTarget::Resolved(_)));
}

#[test]
fn a_missing_file_is_still_worded_as_no_unit_and_a_block_miss_as_no_block() {
    let (_dir, graph) = graph_of(&[("A.md", "text\n")], &[]);
    let err = graph.resolve_link("[[Nope]]", Some("s")).unwrap_err();
    assert_eq!(err.to_string(), "no unit in source \"s\" matches \"Nope\"");
    let err = graph
        .resolve_link("[[Nope#Heading]]", Some("s"))
        .unwrap_err();
    assert_eq!(err.to_string(), "no unit in source \"s\" matches \"Nope\"");
    let err = graph
        .resolve_link("[[Nope#^abc123]]", Some("s"))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "no block ^abc123 in \"Nope\" or elsewhere in source \"s\""
    );
}
