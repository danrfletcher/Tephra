//! `tests/unit/wikilinks.rs`: plain, aliased, anchored,
//! block-anchored, and `@source:path`-prefixed forms; empty `[[]]`;
//! unterminated `[[` at EOF.

use std::path::Path;

use tephra_parser::{parse_unit, DiagnosticKind};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/units/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read fixture")
}

#[test]
fn plain_aliased_anchored_block_anchored_and_federated_forms_are_distinguished() {
    let content = fixture("federated-wikilink.md");
    let unit = parse_unit(Path::new("federated-wikilink.md"), &content);
    assert_eq!(unit.wikilinks.len(), 6);

    let plain = &unit.wikilinks[0];
    assert_eq!(plain.target, "Note");
    assert!(plain.source_prefix.is_none());
    assert!(plain.anchor.is_none());
    assert!(plain.alias.is_none());

    let heading_anchor = &unit.wikilinks[1];
    assert_eq!(heading_anchor.target, "Note");
    assert_eq!(heading_anchor.anchor.as_deref(), Some("Heading"));

    let block_anchor = &unit.wikilinks[2];
    assert_eq!(block_anchor.target, "Note");
    assert_eq!(block_anchor.anchor.as_deref(), Some("^block-anchor"));

    let aliased = &unit.wikilinks[3];
    assert_eq!(aliased.target, "Note");
    assert_eq!(aliased.alias.as_deref(), Some("Alias Text"));

    let federated = &unit.wikilinks[4];
    assert_eq!(federated.source_prefix.as_deref(), Some("atlas-repo"));
    assert_eq!(federated.target, "README");
    assert!(federated.anchor.is_none());
    assert!(federated.alias.is_none());

    let federated_full = &unit.wikilinks[5];
    assert_eq!(federated_full.source_prefix.as_deref(), Some("atlas-repo"));
    assert_eq!(federated_full.target, "README");
    assert_eq!(federated_full.anchor.as_deref(), Some("Section"));
    assert_eq!(federated_full.alias.as_deref(), Some("Display"));
}

#[test]
fn empty_wikilink_is_a_diagnostic_never_silently_dropped_and_never_a_panic() {
    let content = fixture("empty-links.md");
    let unit = parse_unit(Path::new("empty-links.md"), &content);

    assert!(unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::EmptyWikilink));
    assert!(unit.wikilinks.iter().all(|w| !w.raw.is_empty()));
}

#[test]
fn unterminated_double_bracket_at_eof_is_flagged_not_panicking() {
    let content = fixture("empty-links.md");
    let unit = parse_unit(Path::new("empty-links.md"), &content);

    assert!(unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::UnterminatedWikilink));
}

#[test]
fn an_unterminated_wikilink_literally_at_the_very_end_of_file_does_not_panic() {
    let content = "no closing brackets here [[Dangling";
    let unit = parse_unit(Path::new("eof.md"), content);
    assert!(unit
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::UnterminatedWikilink));
}

#[test]
fn a_wikilink_inside_an_inline_code_span_is_never_a_wikilink() {
    let unit = parse_unit(Path::new("t.md"), "See `[[InCode]]` for details.");
    assert!(unit.wikilinks.is_empty(), "got {:?}", unit.wikilinks);
}

#[test]
fn a_wikilink_inside_an_indented_code_block_is_never_a_wikilink() {
    let content = "Some text.\n\n    #indented [[Ind]] ^ind\n\nMore text.\n";
    let unit = parse_unit(Path::new("t.md"), content);
    assert!(unit.wikilinks.is_empty(), "got {:?}", unit.wikilinks);
}

#[test]
fn a_wikilink_inside_an_html_comment_is_never_a_wikilink() {
    let content = "Before.\n\n<!-- #hidden [[Hid]] -->\n\nAfter.\n";
    let unit = parse_unit(Path::new("t.md"), content);
    assert!(unit.wikilinks.is_empty(), "got {:?}", unit.wikilinks);
}
