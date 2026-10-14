//! `tests/unit/markdown_links.rs` (R1): a markdown-style
//! `[text](target)` link resolves with its file extension still on
//! it, a leading `./`, percent-encoding, and — the part most prone to
//! silent breakage — relative to the *linking file's own directory*,
//! not the source root. Built from the real `tephra_parser` markdown
//! pass (not hand-written `UnitFacts::links` tuples) for the
//! extension/encoding cases, so this exercises the exact `to_raw`
//! shape a real file produces.

use tephra_graph::{EdgeTarget, UnitFacts, UnitGraph};

fn markdown_links_in(path: &str, body: &str) -> Vec<(String, String)> {
    let parsed = tephra_parser::parse_unit_bytes(std::path::Path::new(path), body.as_bytes())
        .expect("fixture body is valid UTF-8");
    parsed
        .to_new_links(1)
        .iter()
        .map(|l| (l.to_raw.to_string(), l.kind.to_string()))
        .collect()
}

fn empty_facts() -> UnitFacts {
    UnitFacts::default()
}

#[test]
fn markdown_links_with_extensions_dot_slash_and_percent_encoding_all_resolve() {
    let body = "# A\n\n\
        [x](B.md) and again [z](./B.md), plus [y](sub/C.md) and \
        [w](caf%C3%A9%20notes.md).\n";
    let links = markdown_links_in("A.md", body);
    assert_eq!(
        links.len(),
        4,
        "sanity: the real parser found all four links"
    );

    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        UnitFacts {
            links,
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "B.md", empty_facts());
    graph.upsert_unit("notes", "sub/C.md", empty_facts());
    graph.upsert_unit("notes", "café notes.md", empty_facts());

    let forward = graph.get_forward_links("@notes:A.md");
    assert!(
        forward
            .iter()
            .all(|e| matches!(e.to, EdgeTarget::Resolved(_))),
        "every markdown link should resolve, none dangling: {forward:?}"
    );

    // [x](B.md) and [z](./B.md) both resolve to the exact same unit,
    // so they collapse to one edge (R2) — three distinct resolved
    // targets, not four edges.
    assert_eq!(forward.len(), 3, "{forward:?}");
    let targets: std::collections::HashSet<String> = forward
        .iter()
        .map(|e| match &e.to {
            EdgeTarget::Resolved(id) => id.clone(),
            EdgeTarget::Dangling => unreachable!(),
        })
        .collect();
    assert!(targets.contains("@notes:B.md"));
    assert!(targets.contains("@notes:sub/C.md"));
    assert!(targets.contains("@notes:café notes.md"));
}

/// R15: when the percent-decoded bytes aren't valid UTF-8,
/// `percent_decode` must fall back to the original, still-encoded
/// text unchanged rather than lossily replacing the invalid bytes
/// with U+FFFD, exactly as its own doc comment promises. `%FF` alone
/// decodes to one invalid UTF-8 byte, so this proves the fallback
/// through the public API: a markdown link to `caf%FF.md` resolves
/// against a unit whose literal rel_path still carries that exact,
/// un-decoded escape — a lossy decode would instead produce a
/// replacement character no real file's name would ever contain, so
/// the link would stay dangling.
#[test]
fn a_markdown_link_with_invalid_percent_encoding_falls_back_to_the_original_text_unchanged() {
    let body = "# A\n\nSee [x](caf%FF.md).\n";
    let links = markdown_links_in("A.md", body);
    assert_eq!(links.len(), 1);

    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        UnitFacts {
            links,
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "caf%FF.md", empty_facts());

    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:caf%FF.md".to_string()),
        "a malformed percent-escape must fall back to the original text unchanged (R15), \
         not a lossily-decoded replacement character matching no real file"
    );
}

/// R1's core fix: a bare (no leading `./`) relative markdown target
/// resolves against the *linking file's own directory*, not the
/// source root — even when a same-named file also exists at the
/// root, and even though the root file would otherwise look like the
/// "shortest" E4 candidate.
#[test]
fn a_markdown_link_resolves_relative_to_its_own_directory_not_the_source_root() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/A.md",
        UnitFacts {
            links: vec![(
                "Sibling.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "sub/Sibling.md", empty_facts());
    // A root-level file of the same bare name: the directory-relative
    // sibling must win, not this one, even though it's "shorter".
    graph.upsert_unit("notes", "Sibling.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:sub/Sibling.md".to_string())
    );
}

/// A `../`-relative markdown target resolves against the parent of
/// the linking file's own directory.
#[test]
fn a_markdown_link_with_dot_dot_resolves_relative_to_the_parent_directory() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/A.md",
        UnitFacts {
            links: vec![(
                "../Root.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "Root.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Root.md".to_string())
    );
}

/// A markdown link's `#anchor` suffix is stripped before matching,
/// the same as a wikilink's.
#[test]
fn a_markdown_link_with_a_heading_anchor_resolves_by_its_target_alone() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "A.md",
        UnitFacts {
            links: vec![(
                "B.md#Some-Heading".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "B.md", empty_facts());

    let forward = graph.get_forward_links("@notes:A.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:B.md".to_string())
    );
}

/// R20: a same-file anchor-only markdown link (`[x](#heading)`) from
/// a root-level file names no other file at all — it must resolve to
/// the referring unit itself, exactly like a wikilink's empty-target
/// self-reference (`[[#Heading]]`), not dangle just because `""` (the
/// anchor stripped away) doesn't match anything in `stem_index`.
#[test]
fn a_markdown_anchor_only_link_from_a_root_level_file_is_a_self_reference() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "Root.md",
        UnitFacts {
            links: vec![(
                "#heading".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );

    let forward = graph.get_forward_links("@notes:Root.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Root.md".to_string()),
        "R20: an anchor-only markdown link must be a self-edge, not dangling"
    );
}

/// R20: the same self-reference case from a file inside a
/// subdirectory. Before the fix, stripping `#heading` left the empty
/// string, which `join_relative` then joined onto the linking file's
/// own directory (`sub`) to produce the bogus key `"sub"` — resolving
/// to an unrelated `sub.md` file rather than staying a self-edge.
/// This test seeds exactly that unrelated file to prove the fix
/// doesn't silently resolve to it.
#[test]
fn a_markdown_anchor_only_link_from_a_subdirectory_file_is_a_self_reference_not_the_parent_named_file(
) {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/A.md",
        UnitFacts {
            links: vec![(
                "#heading".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    // The unrelated file the pre-fix bug would have resolved to.
    graph.upsert_unit("notes", "sub.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:sub/A.md".to_string()),
        "R20: an anchor-only markdown link must stay a self-edge regardless of the \
         linking file's own directory depth, never resolve to an unrelated file \
         named after that directory"
    );
}

/// R21: a relative markdown link from a root-level file must look up
/// its exact target path (`stem_index`), never fall back to
/// `basename_index` the way a bare wikilink does — so a link to a
/// path that doesn't exist at the source root dangles, even when a
/// same-named file exists somewhere else in the source.
#[test]
fn a_root_level_markdown_link_to_a_nonexistent_root_file_dangles_even_when_a_same_named_file_exists_elsewhere(
) {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "N.md",
        UnitFacts {
            links: vec![(
                "Shared.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    // Only a nested Shared.md exists; no file named Shared.md exists
    // at the source root.
    graph.upsert_unit("notes", "teamA/Shared.md", empty_facts());

    let forward = graph.get_forward_links("@notes:N.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Dangling,
        "R21: a root-level markdown link to a path that doesn't exist must dangle, \
         not silently resolve to an unrelated same-named file elsewhere in the source"
    );
}

/// R21: the same rule for a `../`-relative link that climbs back to
/// the source root — it must also look up the exact joined path, not
/// fall back to a basename match, even though the joined path (with
/// no `/` in it) looks identical to a root-level link's key.
#[test]
fn a_dot_dot_relative_markdown_link_to_a_nonexistent_root_file_dangles_even_when_a_same_named_file_exists_elsewhere(
) {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/K.md",
        UnitFacts {
            links: vec![(
                "../Shared.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "teamA/Shared.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/K.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Dangling,
        "R21: a ../-relative markdown link to a path that doesn't exist must dangle, \
         not silently resolve to an unrelated same-named file elsewhere in the source"
    );
}

/// R22: a `..` that climbs past the source root, from a root-level
/// linking file, must dangle rather than silently resolving to an
/// unrelated in-source file that happens to share the remaining path
/// components. Before the fix, `join_relative`'s `stack.pop()` on an
/// empty stack was a no-op, so `../other/README.md` from a root file
/// resolved to `other/README.md`.
#[test]
fn a_dot_dot_past_the_source_root_from_a_root_level_file_dangles() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "N.md",
        UnitFacts {
            links: vec![(
                "../other/README.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    // The unrelated file the pre-fix bug would have resolved to.
    graph.upsert_unit("notes", "other/README.md", empty_facts());

    let forward = graph.get_forward_links("@notes:N.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Dangling,
        "R22: a ../ past the source root must dangle, not resolve to an unrelated \
         in-source file"
    );
}

/// R22: the same rule for a linking file nested one directory deep,
/// where a *single* `..` only reaches back to the source root (still
/// valid) but a *second* `..` climbs past it. Before the fix this
/// collapsed to `README.md` at the root, an unrelated file.
#[test]
fn a_double_dot_dot_past_the_source_root_from_a_nested_file_dangles() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/N.md",
        UnitFacts {
            links: vec![(
                "../../README.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    // The unrelated file the pre-fix bug would have resolved to.
    graph.upsert_unit("notes", "README.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/N.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Dangling,
        "R22: a ../.. that climbs past the source root from a nested file must dangle, \
         not resolve to an unrelated root-level file"
    );
}

/// R22: a root-absolute markdown target (`/docs/X.md`, standard in
/// GitHub-style READMEs, which the `atlas-repo` code source is) is
/// resolved from the *source root*, not from the linking file's own
/// directory — from a root-level file this must behave identically
/// to a bare relative link, resolving to `docs/X.md`.
#[test]
fn a_root_absolute_markdown_link_from_a_root_level_file_resolves_from_the_source_root() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "N.md",
        UnitFacts {
            links: vec![(
                "/docs/X.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "docs/X.md", empty_facts());

    let forward = graph.get_forward_links("@notes:N.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:docs/X.md".to_string())
    );
}

/// R22: the same root-absolute target from a file nested in a
/// subdirectory must resolve to the exact same unit as from the root
/// — never joined onto the linking file's own directory. Before the
/// fix, the leading `/` was simply dropped and the remainder joined
/// onto `from_dir`, so this dangled (`sub/docs/X`) instead of
/// matching `docs/X.md`.
#[test]
fn a_root_absolute_markdown_link_from_a_subdirectory_file_still_resolves_from_the_source_root() {
    let mut graph = UnitGraph::new();
    graph.upsert_unit(
        "notes",
        "sub/A.md",
        UnitFacts {
            links: vec![(
                "/docs/X.md".to_string(),
                tephra_parser::LINK_KIND_MARKDOWN.to_string(),
            )],
            ..Default::default()
        },
    );
    graph.upsert_unit("notes", "docs/X.md", empty_facts());

    let forward = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:docs/X.md".to_string()),
        "R22: a root-absolute markdown link must resolve from the source root \
         regardless of the linking file's own directory depth"
    );
}
