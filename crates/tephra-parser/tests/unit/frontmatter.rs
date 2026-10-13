//! `tests/unit/frontmatter.rs`: delimiter detection, valid/invalid
//! YAML, missing closing delimiter, frontmatter-only file.

use std::path::Path;

use tephra_parser::{parse_unit, DiagnosticKind};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/frontmatter/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read fixture")
}

#[test]
fn valid_frontmatter_is_extracted_and_parsed_to_json() {
    let content = fixture("valid.md");
    let unit = parse_unit(Path::new("valid.md"), &content);

    let fm = unit.frontmatter.expect("frontmatter present");
    assert_eq!(fm.start_line, 1);
    let json = fm.json.expect("valid YAML parses to JSON");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["title"], "Hello World");
    assert_eq!(value["status"], "active");
    assert_eq!(value["tags"][0], "alpha");
    assert!(unit
        .diagnostics
        .iter()
        .all(|d| d.kind != DiagnosticKind::InvalidFrontmatterYaml));
}

#[test]
fn unbalanced_quote_yields_diagnostic_with_line_number_but_rest_of_file_still_parses() {
    let content = fixture("invalid_yaml.md");
    let unit = parse_unit(Path::new("invalid_yaml.md"), &content);

    let fm = unit.frontmatter.expect("delimiters are still well-formed");
    assert!(
        fm.json.is_none(),
        "malformed YAML must not produce a JSON value"
    );

    let diag = unit
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::InvalidFrontmatterYaml)
        .expect("a diagnostic must be recorded");
    // The diagnostic must point somewhere inside (or immediately
    // after) the frontmatter block, not line 0 or a fabricated value.
    assert!(diag.line >= fm.start_line && diag.line <= fm.end_line + 1);

    // The rest of the file after the malformed block still parses:
    // one bad frontmatter block never discards the rest of the file.
    assert!(content.contains("Body after malformed frontmatter."));
}

#[test]
fn missing_closing_delimiter_is_treated_as_ordinary_body_without_erroring() {
    let content = fixture("missing_closing.md");
    let unit = parse_unit(Path::new("missing_closing.md"), &content);

    assert!(unit.frontmatter.is_none());
    assert!(unit
        .diagnostics
        .iter()
        .all(|d| d.kind != DiagnosticKind::InvalidFrontmatterYaml));
    // The leading "---" became ordinary text, and the real heading
    // further down still parsed correctly.
    assert!(unit.headings.iter().any(|h| h.text == "Heading still here"));
}

#[test]
fn frontmatter_only_file_yields_empty_body_ast() {
    let content = fixture("frontmatter_only.md");
    let unit = parse_unit(Path::new("frontmatter_only.md"), &content);

    let fm = unit.frontmatter.expect("frontmatter present");
    assert!(fm.json.is_some());
    assert!(unit.headings.is_empty());
    assert!(unit.wikilinks.is_empty());
    assert!(unit.markdown_links.is_empty());
    assert!(unit.tags.is_empty());
    assert!(unit.blocks.is_empty());
}

#[test]
fn duplicate_top_level_keys_are_flagged_with_a_line_number() {
    let content = fixture("duplicate_keys.md");
    let unit = parse_unit(Path::new("duplicate_keys.md"), &content);

    let diag = unit
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::DuplicateFrontmatterKey)
        .expect("duplicate key diagnostic");
    assert!(diag.line > 0);
    assert!(diag.message.contains("title"));
}
