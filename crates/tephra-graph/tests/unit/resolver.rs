//! `tests/unit/resolver.rs`: table-driven coverage of
//! [`tephra_graph::UnitGraph::resolve_link`]'s resolution rule (E4)
//! — unique match (bare name and basename-only), cross-source
//! `@source:path` targets, a tied/ambiguous shortest-path match, an
//! unresolved target, and an unknown `@source` prefix.

use tephra_graph::{ResolveError, UnitFacts, UnitGraph};

fn empty_facts() -> UnitFacts {
    UnitFacts {
        frontmatter_json: None,
        tags: Vec::new(),
        blocks: Vec::new(),
        links: Vec::new(),
        link_lines: Vec::new(),
    }
}

/// Builds the fixed graph every case in [`table_driven_resolve_link_cases`]
/// resolves against: two sources, `"notes"` and `"atlas-repo"`, with
/// one intentionally ambiguous pair of equal-depth, equal-length
/// `Shared.md` files in `"notes"`.
fn fixture_graph() -> UnitGraph {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Journal.md", empty_facts());
    graph.upsert_unit("notes", "teamA/Shared.md", empty_facts());
    graph.upsert_unit("notes", "teamB/Shared.md", empty_facts());
    graph.upsert_unit("atlas-repo", "README.md", empty_facts());
    graph
}

#[test]
fn table_driven_resolve_link_cases() {
    let graph = fixture_graph();

    struct Case {
        name: &'static str,
        raw: &'static str,
        default_source: Option<&'static str>,
        expect: Expect,
    }
    enum Expect {
        ResolvesTo(&'static str),
        Ambiguous,
        Unresolved,
        UnknownSource,
        NoSourceContext,
    }

    let cases = vec![
        Case {
            name: "bare name, unique, within default source",
            raw: "[[Journal]]",
            default_source: Some("notes"),
            expect: Expect::ResolvesTo("@notes:Journal.md"),
        },
        Case {
            name: "bare inner text without brackets resolves the same way",
            raw: "Journal",
            default_source: Some("notes"),
            expect: Expect::ResolvesTo("@notes:Journal.md"),
        },
        Case {
            name: "full path, unique, within default source",
            raw: "[[teamA/Shared]]",
            default_source: Some("notes"),
            expect: Expect::ResolvesTo("@notes:teamA/Shared.md"),
        },
        Case {
            name: "cross-source @source:path target, default source ignored",
            raw: "[[@atlas-repo:README]]",
            default_source: Some("notes"),
            expect: Expect::ResolvesTo("@atlas-repo:README.md"),
        },
        Case {
            name: "cross-source target with no default source given at all",
            raw: "[[@atlas-repo:README]]",
            default_source: None,
            expect: Expect::ResolvesTo("@atlas-repo:README.md"),
        },
        Case {
            name: "basename-only target ties between two equal-depth files (E4)",
            raw: "[[Shared]]",
            default_source: Some("notes"),
            expect: Expect::Ambiguous,
        },
        Case {
            name: "target matches nothing in the scoped source",
            raw: "[[NoSuchUnit]]",
            default_source: Some("notes"),
            expect: Expect::Unresolved,
        },
        Case {
            name: "@source prefix names a source the graph never registered",
            raw: "[[@ghost-source:Anything]]",
            default_source: Some("notes"),
            expect: Expect::UnknownSource,
        },
        Case {
            name: "no @source prefix and no default_source given",
            raw: "[[Journal]]",
            default_source: None,
            expect: Expect::NoSourceContext,
        },
        Case {
            name: "a target that exists only in a different source than default_source is unresolved, not found cross-source",
            raw: "[[README]]",
            default_source: Some("notes"),
            expect: Expect::Unresolved,
        },
    ];

    for case in cases {
        let result = graph.resolve_link(case.raw, case.default_source);
        match case.expect {
            Expect::ResolvesTo(id) => {
                let resolved = result.unwrap_or_else(|e| {
                    panic!("case \"{}\": expected Ok, got Err({e})", case.name)
                });
                assert_eq!(
                    resolved.unit.id, id,
                    "case \"{}\": resolved to the wrong unit",
                    case.name
                );
            }
            Expect::Ambiguous => match result {
                Err(ResolveError::Ambiguous { mut candidates, .. }) => {
                    candidates.sort();
                    assert_eq!(
                        candidates,
                        vec![
                            "@notes:teamA/Shared.md".to_string(),
                            "@notes:teamB/Shared.md".to_string(),
                        ],
                        "case \"{}\": expected every tied candidate listed, not just a count",
                        case.name
                    );
                }
                other => panic!(
                    "case \"{}\": expected Ambiguous, got {:?}",
                    case.name, other
                ),
            },
            Expect::Unresolved => match result {
                Err(ResolveError::Unresolved { .. }) => {}
                other => panic!(
                    "case \"{}\": expected Unresolved, got {:?}",
                    case.name, other
                ),
            },
            Expect::UnknownSource => match result {
                Err(ResolveError::UnknownSource(_)) => {}
                other => panic!(
                    "case \"{}\": expected UnknownSource, got {:?}",
                    case.name, other
                ),
            },
            Expect::NoSourceContext => match result {
                Err(ResolveError::NoSourceContext) => {}
                other => panic!(
                    "case \"{}\": expected NoSourceContext, got {:?}",
                    case.name, other
                ),
            },
        }
    }
}

#[test]
fn resolve_link_result_carries_the_resolved_units_own_forward_edges() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "One.md",
        UnitFacts {
            frontmatter_json: None,
            tags: Vec::new(),
            blocks: Vec::new(),
            links: vec![("Two".to_string(), "wikilink".to_string())],
            link_lines: Vec::new(),
        },
    );
    graph.upsert_unit("notes", "Two.md", empty_facts());

    let resolved = graph.resolve_link("[[One]]", Some("notes")).unwrap();
    assert_eq!(resolved.edges.len(), 1);
    assert_eq!(resolved.edges[0].from, "@notes:One.md");
}

#[test]
fn an_offline_source_is_still_resolvable_and_flagged_offline() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Journal.md", empty_facts());
    graph.set_source_offline("notes", true);

    let resolved = graph.resolve_link("[[Journal]]", Some("notes")).unwrap();
    assert!(resolved.unit.offline);
}

/// R6: a file whose name is stored NFD-normalized on disk (common on
/// macOS/HFS+/APFS — `cafe` + combining U+0301, byte-distinct from
/// the precomposed `é`) still resolves from an NFC-typed `[[café]]`
/// wikilink target, rather than silently failing with `Unresolved`
/// because the two strings differ byte-for-byte.
#[test]
fn an_nfd_normalized_stored_filename_resolves_from_an_nfc_typed_link_target() {
    let nfd_name = "cafe\u{0301} notes.md"; // "café notes.md", NFD form
    let nfc_target = "caf\u{00e9} notes"; // "café notes", NFC form

    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", nfd_name, empty_facts());

    let resolved = graph
        .resolve_link(&format!("[[{nfc_target}]]"), Some("notes"))
        .expect("an NFC-typed link should resolve an NFD-stored filename");
    assert_eq!(resolved.unit.id, format!("@notes:{nfd_name}"));
    // The stored rel_path itself is untouched — byte-exact, still NFD.
    assert_eq!(resolved.unit.rel_path, nfd_name);
}

/// E4's "resolves to the shortest relative path" branch, exercised
/// directly rather than only asserted via `candidates.len()`: a root
/// `Shared.md` (matched length 6) and a nested `teamA/Shared.md`
/// (matched length 12) are not a tie — the shorter one wins outright,
/// with no `Ambiguous` error at all.
#[test]
fn a_shorter_candidate_wins_outright_over_a_longer_one_rather_than_tying() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Shared.md", empty_facts());
    graph.upsert_unit("notes", "teamA/Shared.md", empty_facts());

    let resolved = graph
        .resolve_link("[[Shared]]", Some("notes"))
        .expect("the shorter root-level candidate should win outright");
    assert_eq!(resolved.unit.id, "@notes:Shared.md");
}

/// R1: a markdown-style link target with its file extension still on
/// it (`B.md`, as a real editor writes one) resolves exactly like the
/// bare-name wikilink form, not as a dangling edge.
#[test]
fn a_wikilink_target_with_a_file_extension_resolves_the_same_as_without_one() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "B.md", empty_facts());

    let resolved = graph
        .resolve_link("[[B.md]]", Some("notes"))
        .expect("a .md-suffixed wikilink target should resolve");
    assert_eq!(resolved.unit.id, "@notes:B.md");
}

/// R6: a target with a `#`/`^` anchor resolves by its target alone,
/// both for a plain heading anchor and a federated cross-source
/// target — the anchor text itself is never part of the match key.
#[test]
fn a_target_with_a_heading_or_block_anchor_resolves_by_its_target_alone() {
    let graph = fixture_graph();

    let resolved = graph
        .resolve_link("[[teamA/Shared#Some Heading]]", Some("notes"))
        .expect("the #anchor suffix must not prevent resolution");
    assert_eq!(resolved.unit.id, "@notes:teamA/Shared.md");

    // A block anchor names a block (G13), not just a file: with no
    // `^some-block` anywhere in the source the link is dangling (a
    // typed ID is never searched for in other files). The link still
    // reached the right source and file; it is the block that is gone.
    let err = graph
        .resolve_link("[[@atlas-repo:README#^some-block]]", Some("notes"))
        .expect_err("a block anchor with no such block is dangling");
    assert_eq!(
        err,
        ResolveError::Unresolved {
            source: "atlas-repo".to_string(),
            target: "README#^some-block".to_string(),
        }
    );
}

// ---- G13 block resolution, table-driven (PR-13.F1) ------------------
//
// One row per {situation} x {prose, code}. The files are real: parsed
// by `tephra-parser`, written through `tephra-cache`, read back by
// `UnitGraph::build_from_cache`.

mod block_resolution {
    use super::{ResolveError, UnitGraph};
    use crate::fixtures::{load_files_into_cache, open_temp_cache};
    use tephra_graph::{BlockLine, BlockRef};

    /// What a row expects `resolve_link` to give.
    #[derive(Debug)]
    enum Expect {
        /// Resolves to `unit`, with `block` (`None`: a link to the file).
        Block(&'static str, Option<(i64, bool)>),
        Unresolved,
        /// Ambiguous: the sorted `(unit, line)` occurrences.
        Ambiguous(&'static [(&'static str, i64)]),
    }

    /// A prose file with `^id` on `line` (preceded by `line - 1` blank
    /// lines, so the paragraph starts on that line).
    fn prose(id: &str, line: usize) -> String {
        format!("{}Text ^{id}\n", "\n".repeat(line - 1))
    }

    /// A Rust file whose `pub fn <name>` line (carrying `// ^id`) is
    /// `line`.
    fn code(id: &str, name: &str, line: usize) -> String {
        format!("{}pub fn {name}() {{}} // ^{id}\n", "\n".repeat(line - 1))
    }

    struct Row {
        name: &'static str,
        /// `(source, rel_path, content)`.
        files: Vec<(&'static str, &'static str, String)>,
        link: &'static str,
        expect: Expect,
    }

    fn rows() -> Vec<Row> {
        let mut rows = Vec::new();
        // The kind decides the extension, the content and the ID
        // shape of every row; the rules are the same for both.
        for (kind, ext, make) in [
            ("prose", "md", prose as fn(&str, usize) -> String),
            ("code", "rs", |id: &str, line: usize| code(id, "f", line)),
        ] {
            let f = |name: &str| -> &'static str {
                Box::leak(format!("{name}.{ext}").into_boxed_str())
            };
            let label =
                |s: &str| -> &'static str { Box::leak(format!("{kind}: {s}").into_boxed_str()) };
            let link = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
            let other = |name: &str, content: String| ("s", f(name), content);
            let unit = |name: &str| -> &'static str {
                Box::leak(format!("@s:{name}.{ext}").into_boxed_str())
            };

            rows.push(Row {
                name: label("in the named file"),
                files: vec![other("A", make("abc123", 4)), other("B", make("abc123", 2))],
                link: link(format!("[[A.{ext}#^abc123]]")),
                // Duplicates across files are allowed: the named file wins.
                expect: Expect::Block(unit("A"), Some((4, false))),
            });
            rows.push(Row {
                name: label("moved once"),
                files: vec![
                    other("Old", "nothing\n".to_string()),
                    other("New", make("abc123", 3)),
                ],
                link: link(format!("[[Old.{ext}#^abc123]]")),
                expect: Expect::Block(unit("New"), Some((3, true))),
            });
            rows.push(Row {
                name: label("named file missing, ID once elsewhere"),
                files: vec![other("New", make("abc123", 3))],
                link: link(format!("[[Deleted.{ext}#^abc123]]")),
                expect: Expect::Block(unit("New"), Some((3, true))),
            });
            rows.push(Row {
                name: label("moved twice"),
                files: vec![
                    other("Old", "nothing\n".to_string()),
                    other("B", make("abc123", 2)),
                    other("C", make("abc123", 5)),
                ],
                link: link(format!("[[Old.{ext}#^abc123]]")),
                expect: Expect::Ambiguous(Box::leak(Box::new([(unit("B"), 2), (unit("C"), 5)]))),
            });
            rows.push(Row {
                name: label("missing everywhere"),
                files: vec![other("Old", "nothing\n".to_string())],
                link: link(format!("[[Old.{ext}#^abc123]]")),
                expect: Expect::Unresolved,
            });
            rows.push(Row {
                name: label("typed ID missing from its file"),
                files: vec![other("A", "nothing\n".to_string())],
                link: link(format!("[[A.{ext}#^summary]]")),
                expect: Expect::Unresolved,
            });
            rows.push(Row {
                name: label("typed ID elsewhere in the source"),
                files: vec![
                    other("A", "nothing\n".to_string()),
                    other("B", make("summary", 2)),
                ],
                link: link(format!("[[A.{ext}#^summary]]")),
                expect: Expect::Unresolved,
            });
            rows.push(Row {
                name: label("typed ID with hyphen and digit, elsewhere"),
                files: vec![
                    other("A", "nothing\n".to_string()),
                    other("B", make("my-note-2", 2)),
                ],
                link: link(format!("[[A.{ext}#^my-note-2]]")),
                expect: Expect::Unresolved,
            });
            rows.push(Row {
                name: label("cross-source: ID only in another source"),
                files: vec![
                    ("s", f("Old"), "nothing\n".to_string()),
                    ("t", f("New"), make("abc123", 3)),
                ],
                link: link(format!("[[@s:Old.{ext}#^abc123]]")),
                expect: Expect::Unresolved,
            });
            rows.push(Row {
                name: label("cross-source link to the named file in another source"),
                files: vec![("t", f("New"), make("abc123", 3))],
                link: link(format!("[[@t:New.{ext}#^abc123]]")),
                expect: Expect::Block(
                    Box::leak(format!("@t:New.{ext}").into_boxed_str()),
                    Some((3, false)),
                ),
            });
            rows.push(Row {
                name: label("duplicate in the named file"),
                files: vec![other("A", {
                    // Two occurrences: lines 1 and 3.
                    let one = make("abc123", 1);
                    let two = make("abc123", 2);
                    format!("{one}{two}")
                })],
                link: link(format!("[[A.{ext}#^abc123]]")),
                expect: Expect::Ambiguous(Box::leak(Box::new([(unit("A"), 1), (unit("A"), 3)]))),
            });
            rows.push(Row {
                name: label("heading anchor"),
                files: vec![other("A", make("abc123", 2))],
                link: link(format!("[[A.{ext}#Heading]]")),
                expect: Expect::Block(unit("A"), None),
            });
            rows.push(Row {
                name: label("symbol-name anchor (F3)"),
                files: vec![other("A", make("abc123", 2))],
                link: link(format!("[[A.{ext}#f]]")),
                expect: Expect::Block(unit("A"), None),
            });
        }
        rows
    }

    #[test]
    fn table_driven_block_resolution_follows_g13_e7_e8() {
        for row in rows() {
            let dir = tempfile::tempdir().expect("tempdir");
            let cache = open_temp_cache(&dir);
            let mut sources: Vec<&str> = row.files.iter().map(|(s, _, _)| *s).collect();
            sources.sort();
            sources.dedup();
            for source in sources {
                let files: Vec<(&str, &str)> = row
                    .files
                    .iter()
                    .filter(|(s, _, _)| *s == source)
                    .map(|(_, p, c)| (*p, c.as_str()))
                    .collect();
                load_files_into_cache(&cache, source, &files);
            }
            let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
            let got = graph.resolve_link(row.link, Some("s"));

            match (&row.expect, got) {
                (Expect::Block(unit, block), Ok(result)) => {
                    assert_eq!(result.unit.id, *unit, "{}", row.name);
                    let want = block.map(|(line, moved)| BlockRef {
                        id: result
                            .block
                            .as_ref()
                            .map(|b| b.id.clone())
                            .unwrap_or_default(),
                        line,
                        moved,
                    });
                    assert_eq!(result.block, want, "{}", row.name);
                }
                (Expect::Unresolved, Err(ResolveError::Unresolved { .. })) => {}
                (
                    Expect::Ambiguous(want),
                    Err(ResolveError::Ambiguous {
                        lines, candidates, ..
                    }),
                ) => {
                    let want: Vec<BlockLine> = want
                        .iter()
                        .map(|(unit, line)| BlockLine {
                            unit: unit.to_string(),
                            line: *line,
                        })
                        .collect();
                    assert_eq!(lines, want, "{}", row.name);
                    let mut units: Vec<String> = want.iter().map(|l| l.unit.clone()).collect();
                    units.dedup();
                    assert_eq!(candidates, units, "{}", row.name);
                }
                (expect, got) => panic!("{}: expected {expect:?}, got {got:?}", row.name),
            }
        }
    }

    #[test]
    fn a_block_link_resolution_never_panics_on_a_missing_named_file_or_empty_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_temp_cache(&dir);
        load_files_into_cache(&cache, "s", &[]);
        let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
        assert!(matches!(
            graph.resolve_link("[[Nope#^abc123]]", Some("s")),
            Err(ResolveError::Unresolved { .. })
        ));
    }
}
