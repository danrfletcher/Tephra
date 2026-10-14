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

    let resolved = graph
        .resolve_link("[[@atlas-repo:README#^some-block]]", Some("notes"))
        .expect("a federated target with a block anchor should still resolve");
    assert_eq!(resolved.unit.id, "@atlas-repo:README.md");
}
