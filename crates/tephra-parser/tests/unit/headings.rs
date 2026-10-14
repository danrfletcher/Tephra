//! `tests/unit/headings.rs`: h1-h6 levels, heading-vs-tag
//! disambiguation at line start, duplicate heading text within one
//! file.

use std::path::Path;

use tephra_parser::parse_unit;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/units/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read fixture")
}

#[test]
fn all_six_heading_levels_are_captured() {
    let content = fixture("heading-levels-and-tag.md");
    let unit = parse_unit(Path::new("heading-levels-and-tag.md"), &content);

    for level in 1..=6u8 {
        assert!(
            unit.headings.iter().any(|h| h.level == level),
            "missing heading level {level}"
        );
    }
}

#[test]
fn bare_hash_immediately_followed_by_text_is_a_tag_not_a_heading() {
    let content = fixture("heading-levels-and-tag.md");
    let unit = parse_unit(Path::new("heading-levels-and-tag.md"), &content);

    assert!(
        unit.headings.iter().all(|h| h.text != "justtag"),
        "#justtag must never be recognized as a heading"
    );
    assert!(unit.tags.iter().any(|t| t.tag == "justtag"));
}

#[test]
fn duplicate_heading_text_is_recorded_once_per_occurrence() {
    let content = fixture("heading-levels-and-tag.md");
    let unit = parse_unit(Path::new("heading-levels-and-tag.md"), &content);

    let repeated: Vec<_> = unit
        .headings
        .iter()
        .filter(|h| h.text == "Repeated")
        .collect();
    assert_eq!(
        repeated.len(),
        2,
        "both occurrences of a duplicated heading must be kept"
    );
    assert_ne!(repeated[0].line, repeated[1].line);
}
