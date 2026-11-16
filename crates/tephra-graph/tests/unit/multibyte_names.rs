//! `tests/unit/multibyte_names.rs` (E26): file names whose multi-byte
//! characters sit close to the extension used to panic ("not a char
//! boundary") in `strip_markdown_extension` while a unit was ingested
//! or a link resolved. Every graph here is built by the real
//! `UnitGraph::upsert_unit` / `build_from_cache` path and queried through
//! `resolve_link`: no mocks.

use tephra_graph::{EdgeTarget, ResolveError, UnitFacts, UnitGraph};

use crate::fixtures::{load_files_into_cache, open_temp_cache};

fn empty_facts() -> UnitFacts {
    UnitFacts {
        frontmatter_json: None,
        tags: Vec::new(),
        blocks: Vec::new(),
        links: Vec::new(),
        link_lines: Vec::new(),
    }
}

/// The names that crashed on `dev`: a multi-byte character within the
/// last 1 to 9 bytes, ending in something other than `.md` /
/// `.markdown` (or in `.md` after such a character).
const CRASH_CLASS: [&str; 12] = [
    "Résumé—v2.docx",
    "Todo…ab",
    "日本x",
    "Plan…ab.md",
    "Plan—.markdown",
    "Notes😀",
    "Notes😀ab",
    "Résumé—v2.pdf",
    "naïve.txt",
    "日本語.org",
    "dé/日本/Todo…ab",
    "dé/日本/Résumé—v2.docx",
];

/// The name a bare `[[...]]` link uses for `rel_path`: a Markdown file is
/// linked without its extension, every other file by its full name.
fn link_name(rel_path: &str) -> &str {
    rel_path
        .strip_suffix(".markdown")
        .or_else(|| rel_path.strip_suffix(".md"))
        .unwrap_or(rel_path)
}

#[test]
fn every_crash_class_name_registers_and_resolves() {
    let mut graph = UnitGraph::new();
    for name in CRASH_CLASS {
        graph.upsert_unit("notes", name, empty_facts());
    }
    for name in CRASH_CLASS {
        let raw = format!("[[{}]]", link_name(name));
        let result = graph
            .resolve_link(&raw, Some("notes"))
            .unwrap_or_else(|e| panic!("{raw} did not resolve: {e}"));
        assert_eq!(result.unit.id, format!("@notes:{name}"), "{raw}");
        // With the full path and the source prefix too.
        let raw = format!("[[@notes:{}]]", link_name(name));
        let result = graph.resolve_link(&raw, None).unwrap();
        assert_eq!(result.unit.id, format!("@notes:{name}"), "{raw}");
    }
}

#[test]
fn a_nested_multibyte_path_resolves_like_its_ascii_twin() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "dé/日本/Résumé—v2.docx", empty_facts());
    graph.upsert_unit("notes", "de/jp/Resume-v2.docx", empty_facts());
    // Whatever shapes of link resolve for the ASCII path must resolve
    // the same way for the multi-byte one (the resolver matches the
    // basename and the full path).
    for (ascii, multibyte) in [
        ("[[Resume-v2.docx]]", "[[Résumé—v2.docx]]"),
        ("[[de/jp/Resume-v2.docx]]", "[[dé/日本/Résumé—v2.docx]]"),
        ("[[jp/Resume-v2.docx]]", "[[日本/Résumé—v2.docx]]"),
    ] {
        let a = graph.resolve_link(ascii, Some("notes"));
        let m = graph.resolve_link(multibyte, Some("notes"));
        assert_eq!(a.is_ok(), m.is_ok(), "{ascii} vs {multibyte}");
    }
    for raw in ["[[Résumé—v2.docx]]", "[[dé/日本/Résumé—v2.docx]]"] {
        let result = graph.resolve_link(raw, Some("notes")).unwrap();
        assert_eq!(result.unit.id, "@notes:dé/日本/Résumé—v2.docx", "{raw}");
    }
}

#[test]
fn nfc_and_nfd_names_resolve_to_the_same_unit() {
    let nfc = "Résumé—v2.docx";
    let nfd = "Re\u{301}sume\u{301}—v2.docx";
    assert_ne!(nfc, nfd);

    // A file stored decomposed, as macOS keeps it, linked either way.
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", nfd, empty_facts());
    let a = graph
        .resolve_link(&format!("[[{nfc}]]"), Some("notes"))
        .unwrap();
    let b = graph
        .resolve_link(&format!("[[{nfd}]]"), Some("notes"))
        .unwrap();
    assert_eq!(a.unit.id, b.unit.id);

    // And the other way round, for a Markdown file.
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Re\u{301}sume\u{301}—v2.md", empty_facts());
    for raw in [
        "[[Résumé—v2]]",
        "[[Re\u{301}sume\u{301}—v2]]",
        "[[Résumé—v2.MD]]",
    ] {
        let result = graph.resolve_link(raw, Some("notes")).unwrap();
        assert_eq!(result.unit.id, "@notes:Re\u{301}sume\u{301}—v2.md", "{raw}");
    }

    // One composed and one decomposed copy are the same name: reported
    // as a tie, never silently resolved to one of them.
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "a/Résumé.md", empty_facts());
    graph.upsert_unit("notes", "b/Re\u{301}sume\u{301}.md", empty_facts());
    let err = graph.resolve_link("[[Résumé]]", Some("notes")).unwrap_err();
    assert!(matches!(err, ResolveError::Ambiguous { .. }), "{err:?}");
}

#[test]
fn ambiguity_between_multibyte_names_is_still_reported() {
    let mut graph = UnitGraph::new();
    for name in ["a/Todo…ab.md", "b/Todo…ab.md", "a/日本x", "b/日本x"] {
        graph.upsert_unit("notes", name, empty_facts());
    }
    for (raw, expected) in [
        (
            "[[Todo…ab]]",
            vec!["@notes:a/Todo…ab.md", "@notes:b/Todo…ab.md"],
        ),
        ("[[日本x]]", vec!["@notes:a/日本x", "@notes:b/日本x"]),
    ] {
        match graph.resolve_link(raw, Some("notes")) {
            Err(ResolveError::Ambiguous { candidates, .. }) => {
                assert_eq!(candidates, expected, "{raw}")
            }
            other => panic!("{raw}: expected Ambiguous, got {other:?}"),
        }
    }
    // The full path breaks the tie.
    let result = graph.resolve_link("[[b/Todo…ab]]", Some("notes")).unwrap();
    assert_eq!(result.unit.id, "@notes:b/Todo…ab.md");
}

#[test]
fn an_unresolved_multibyte_target_is_unresolved_not_a_panic() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit("notes", "Todo…ab", empty_facts());
    for raw in ["[[Todo…ac]]", "[[Résumé—v9.docx]]", "[[日本]]", "[[…]]"] {
        let err = graph.resolve_link(raw, Some("notes")).unwrap_err();
        assert!(
            matches!(err, ResolveError::Unresolved { .. }),
            "{raw}: {err:?}"
        );
    }
}

/// `[[Todo…ab#^k3f9a2|plan]]`: a bare multi-byte target with an anchor
/// and an alias, built from real files so the edge is what a scan makes.
#[test]
fn a_block_link_with_anchor_and_alias_to_a_multibyte_target_resolves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_files_into_cache(
        &cache,
        "s",
        &[
            ("Todo…ab.md", "# Todo\n\nThe plan. ^k3f9a2\n"),
            ("Résumé—v2.docx", "binary-ish text\n"),
            (
                "Journal—.md",
                "See [[Todo…ab#^k3f9a2|plan]] and [[Résumé—v2.docx]].\n",
            ),
        ],
    );
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let result = graph
        .resolve_link("[[Todo…ab#^k3f9a2|plan]]", Some("s"))
        .unwrap();
    assert_eq!(result.unit.id, "@s:Todo…ab.md");
    let block = result.block.expect("the block resolves");
    assert_eq!(
        (block.id.as_str(), block.line, block.moved),
        ("k3f9a2", 3, false)
    );

    let backlinks = graph.get_backlinks("@s:Todo…ab.md");
    assert_eq!(backlinks.len(), 1, "{backlinks:?}");
    assert_eq!(backlinks[0].from, "@s:Journal—.md");
    assert_eq!(backlinks[0].to_raw, "Todo…ab#^k3f9a2|plan");
    assert!(matches!(&backlinks[0].to, EdgeTarget::Resolved(id) if id == "@s:Todo…ab.md"));

    let backlinks = graph.get_backlinks("@s:Résumé—v2.docx");
    assert_eq!(backlinks.len(), 1, "{backlinks:?}");
}

/// A file whose `.md` is preceded by a multi-byte character is
/// ingested, and a link typed with the extension finds it.
#[test]
fn a_multibyte_markdown_file_is_found_with_and_without_its_extension() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = open_temp_cache(&dir);
    load_files_into_cache(
        &cache,
        "s",
        &[
            ("Plan…ab.md", "# Plan\n"),
            (
                "Linker.md",
                "[[Plan…ab]] [[Plan…ab.md]] [x](Plan%E2%80%A6ab.md)\n",
            ),
        ],
    );
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");
    for raw in ["[[Plan…ab]]", "[[Plan…ab.md]]", "[[Plan…AB.md]]"] {
        let found = graph.resolve_link(raw, Some("s"));
        if raw.contains("AB") {
            // Matching is case-sensitive; only the extension's case is free.
            assert!(found.is_err(), "{raw}");
        } else {
            assert_eq!(found.unwrap().unit.id, "@s:Plan…ab.md", "{raw}");
        }
    }
    // Two links in one file to the same unit are one backlink row, the
    // same as for an ASCII name.
    let backlinks = graph.get_backlinks("@s:Plan…ab.md");
    assert_eq!(backlinks.len(), 1, "{backlinks:?}");
    assert_eq!(backlinks[0].from, "@s:Linker.md");
}
