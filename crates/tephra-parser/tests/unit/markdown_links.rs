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
