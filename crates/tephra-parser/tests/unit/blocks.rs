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
