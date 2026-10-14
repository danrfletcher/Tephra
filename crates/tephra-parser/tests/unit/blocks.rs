//! `tests/unit/blocks.rs`: `^block-id` anchor extraction and
//! duplicate block IDs within one file flagged as a diagnostic.

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
fn trailing_block_anchor_is_extracted_with_its_line_number() {
    let content = "A paragraph that ends in a block anchor. ^my-block\n\nMore text.";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "my-block");
    assert_eq!(unit.blocks[0].line, 1);
}

#[test]
fn duplicate_block_ids_within_one_file_are_flagged() {
    let content = fixture("duplicate-blocks.md");
    let unit = parse_unit(Path::new("duplicate-blocks.md"), &content);

    assert_eq!(unit.blocks.len(), 2);
    assert!(unit.blocks.iter().all(|b| b.block_id == "dup-id"));

    let diag = unit
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::DuplicateBlockId)
        .expect("duplicate block id diagnostic");
    assert_eq!(diag.line, unit.blocks[1].line);
}

#[test]
fn anchor_is_kept_when_the_line_starts_with_inline_code() {
    // R6: the caret itself sits outside the excluded (code) range,
    // even though the line *starts* inside one — must not be
    // dropped just because `line_start` falls in `excluded_ranges`.
    let content = "`npm install` fetches deps ^install";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "install");
}

#[test]
fn anchor_is_kept_when_the_line_starts_with_inline_html() {
    let content = "<span>x</span> text ^spanid";
    let unit = parse_unit(Path::new("b.md"), content);

    assert_eq!(unit.blocks.len(), 1);
    assert_eq!(unit.blocks[0].block_id, "spanid");
}

#[test]
fn anchor_inside_a_multiline_inline_code_span_is_not_recorded() {
    // R6: a caret that merely *looks* standalone on its own line is
    // still inside the excluded range when that range (an inline
    // code span here) started on an earlier line and closes later.
    let content = "x `foo ^incode\nbar` tail";
    let unit = parse_unit(Path::new("b.md"), content);

    assert!(
        unit.blocks.is_empty(),
        "expected no block anchors, got {:?}",
        unit.blocks
    );
}

#[test]
fn anchor_inside_a_multiline_html_comment_is_not_recorded() {
    let content = "y <!-- note ^hid\n--> after";
    let unit = parse_unit(Path::new("b.md"), content);

    assert!(
        unit.blocks.is_empty(),
        "expected no block anchors, got {:?}",
        unit.blocks
    );
}
