//! `tests/unit/markdown_links.rs`: inline links, reference-style
//! links, bare URLs vs. relative paths, empty `[]()`.

use std::path::Path;

use tephra_parser::{parse_unit, DiagnosticKind};

#[test]
fn inline_link_destination_and_text_are_captured() {
    let content = "See [the project](https://example.com/project) for details.";
    let unit = parse_unit(Path::new("inline.md"), content);

    assert_eq!(unit.markdown_links.len(), 1);
    assert_eq!(unit.markdown_links[0].text, "the project");
    assert_eq!(unit.markdown_links[0].target, "https://example.com/project");
}

#[test]
fn reference_style_link_resolves_to_its_definition_url() {
    let content = "See [the project][proj] for details.\n\n[proj]: ./relative/path.md\n";
    let unit = parse_unit(Path::new("reference.md"), content);

    assert_eq!(unit.markdown_links.len(), 1);
    assert_eq!(unit.markdown_links[0].text, "the project");
    assert_eq!(unit.markdown_links[0].target, "./relative/path.md");
}

#[test]
fn bare_absolute_url_and_relative_path_destinations_are_both_captured_verbatim() {
    let content = "[abs](https://example.com/x) and [rel](../sibling/note.md).";
    let unit = parse_unit(Path::new("mixed.md"), content);

    assert_eq!(unit.markdown_links.len(), 2);
    assert_eq!(unit.markdown_links[0].target, "https://example.com/x");
    assert_eq!(unit.markdown_links[1].target, "../sibling/note.md");
}

/// R9 review finding: a titled link's recorded span must cover only
/// the destination text (`OldDoc.md`), never the following `"title"`
/// — `tephrad::refactor` rewrites by this exact byte range, and a
/// span that spilled into the title could never match the expected
/// replacement text.
#[test]
fn titled_inline_link_span_excludes_the_title() {
    let content = "See [doc](OldDoc.md \"The title\") for more.";
    let unit = parse_unit(Path::new("titled.md"), content);

    assert_eq!(unit.markdown_links.len(), 1);
    let link = &unit.markdown_links[0];
    assert_eq!(link.target, "OldDoc.md");
    let (start, end) = link.span.expect("span recorded");
    assert_eq!(&content[start..end], "OldDoc.md");
}

/// R9 review finding: an angle-bracket destination's recorded span
/// must exclude the `<`/`>` delimiters themselves, since those are
/// not part of the destination text `tephrad::refactor` compares
/// against.
#[test]
fn angle_bracket_destination_span_excludes_the_brackets() {
    let content = "See [doc](<Old Doc.md>) for more.";
    let unit = parse_unit(Path::new("angle.md"), content);

    assert_eq!(unit.markdown_links.len(), 1);
    let link = &unit.markdown_links[0];
    assert_eq!(link.target, "Old Doc.md");
    let (start, end) = link.span.expect("span recorded");
    assert_eq!(&content[start..end], "Old Doc.md");
}

/// R9 review finding: a bare (non-angle-bracket) destination
/// containing its own balanced, unescaped `(...)` must still have
/// its whole destination captured by the span — not truncated at the
/// destination's own inner `(`, which the previous `rfind('(')`-based
/// implementation mistook for the link's opening paren.
#[test]
fn destination_containing_balanced_parens_is_captured_in_full() {
    let content = "See [doc](Old(2).md) for more.";
    let unit = parse_unit(Path::new("parens.md"), content);

    assert_eq!(unit.markdown_links.len(), 1);
    let link = &unit.markdown_links[0];
    assert_eq!(link.target, "Old(2).md");
    let (start, end) = link.span.expect("span recorded");
    assert_eq!(&content[start..end], "Old(2).md");
}

#[test]
fn empty_markdown_link_is_a_diagnostic_never_silently_dropped_and_never_a_panic() {
    let content = "An empty link: [](). Still parses.";
    let unit = parse_unit(Path::new("empty.md"), content);

    assert!(unit.markdown_links.is_empty());
    assert!(unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::EmptyMarkdownLink));
}
