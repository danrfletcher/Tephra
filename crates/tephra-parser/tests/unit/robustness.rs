//! Remaining edge & failure cases not tied to one specific extraction
//! category: an empty file parses without error, and non-UTF-8 bytes
//! produce a typed error rather than a panic. The multi-megabyte,
//! bounded-time/bounded-memory case lives in `tests/unit/large_file.rs`
//! (see `tephra_parser::RECOMMENDED_MAX_FILE_BYTES`'s doc comment).

use std::path::Path;

use tephra_parser::{parse_unit, parse_unit_bytes};

#[test]
fn empty_file_parses_to_an_empty_parsed_unit_without_error_or_panic() {
    let unit = parse_unit(Path::new("empty.md"), "");

    assert!(unit.frontmatter.is_none());
    assert!(unit.headings.is_empty());
    assert!(unit.wikilinks.is_empty());
    assert!(unit.markdown_links.is_empty());
    assert!(unit.tags.is_empty());
    assert!(unit.blocks.is_empty());
    assert!(unit.diagnostics.is_empty());
}

#[test]
fn non_utf8_bytes_produce_a_typed_error_not_a_panic() {
    let invalid = vec![0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72]; // "foo" + invalid bytes + "bar"
    let result = parse_unit_bytes(Path::new("binary.md"), &invalid);

    let err = result.expect_err("non-UTF-8 input must be a typed error, not a panic or Ok");
    assert_eq!(err.valid_up_to, 3);
}

#[test]
fn valid_utf8_bytes_parse_the_same_as_the_str_entry_point() {
    let content = "# Heading\n\nSome #tag here.";
    let via_bytes = parse_unit_bytes(Path::new("x.md"), content.as_bytes()).expect("valid utf8");
    let via_str = parse_unit(Path::new("x.md"), content);
    assert_eq!(via_bytes, via_str);
}
