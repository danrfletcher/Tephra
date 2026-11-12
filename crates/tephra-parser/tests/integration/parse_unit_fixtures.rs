//! `tests/integration/parse_unit_fixtures.rs`: snapshot tests over a
//! corpus of realistic fixture notes under `tests/fixtures/units/*.md`,
//! asserting the full `ParsedUnit` against committed golden JSON
//! snapshots. No mocks: `parse_unit` does no I/O, so this is the real
//! code path end to end for each fixture.
//!
//! Regenerate every snapshot with:
//!   UPDATE_SNAPSHOTS=1 cargo test -p tephra-parser --test integration

use std::path::{Path, PathBuf};

use tephra_parser::parse_unit;

/// Every fixture covered by this snapshot corpus — see this crate's
/// README for the "at least one file per edge case" mapping.
const FIXTURES: &[&str] = &[
    "golden-path.md",
    "empty.md",
    "frontmatter-only.md",
    "unterminated-frontmatter.md",
    "malformed-frontmatter.md",
    "empty-links.md",
    "heading-levels-and-tag.md",
    "nested-unicode-tags-with-fence.md",
    "duplicate-blocks.md",
    "federated-wikilink.md",
    // G3: every prose format.
    "golden.txt",
    "golden.org",
    "golden.rst",
    "golden.adoc",
    "duplicate-blocks.org",
    "utf16-bom.txt",
];

/// A fixture's text. `utf16-bom.txt` is UTF-16LE with a BOM on disk
/// (G2: decoded before parsing, so it parses like its UTF-8 copy);
/// every other fixture is UTF-8.
fn read_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let (pairs, _) = rest.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16(&units).expect("valid UTF-16 fixture");
    }
    String::from_utf8(bytes).expect("valid UTF-8 fixture")
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/units")
}

fn snapshots_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/units/__snapshots__")
}

#[test]
fn every_fixture_matches_its_committed_golden_snapshot() {
    let update = std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1");
    if update {
        std::fs::create_dir_all(snapshots_dir()).expect("create snapshots dir");
    }

    let mut failures = Vec::new();

    for name in FIXTURES {
        let content = read_fixture(name);

        // parse_unit is given a stable, fixture-relative path (not an
        // absolute, machine-specific one) so the snapshot is portable.
        let relative_path = Path::new("tests/fixtures/units").join(name);
        let parsed = parse_unit(&relative_path, &content);
        let actual = serde_json::to_string_pretty(&parsed).expect("serialize ParsedUnit");

        let snapshot_path = snapshots_dir().join(format!("{name}.json"));

        if update {
            std::fs::write(&snapshot_path, &actual).expect("write snapshot");
            continue;
        }

        let expected = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|e| {
            panic!(
                "missing snapshot {} ({e}) — run with UPDATE_SNAPSHOTS=1 to generate it",
                snapshot_path.display()
            )
        });

        if actual.trim_end() != expected.trim_end() {
            failures.push(format!(
                "snapshot mismatch for {name}:\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn reparsing_a_fixture_twice_is_byte_identical() {
    let md_path = fixtures_dir().join("golden-path.md");
    let content = std::fs::read_to_string(&md_path).expect("read golden-path.md");
    let relative_path = Path::new("tests/fixtures/units/golden-path.md");

    let first = parse_unit(relative_path, &content);
    let second = parse_unit(relative_path, &content);
    assert_eq!(first, second);
}

#[test]
fn the_utf16_bom_fixture_parses_the_same_as_its_utf8_copy() {
    let decoded = read_fixture("utf16-bom.txt");
    assert!(decoded.contains("Café"), "decoded text: {decoded:?}");
    let path = Path::new("tests/fixtures/units/utf16-bom.txt");
    let from_utf16 = parse_unit(path, &decoded);
    // The same text, written as plain UTF-8.
    let utf8_copy = decoded.clone();
    assert_eq!(from_utf16, parse_unit(path, &utf8_copy));
    assert_eq!(from_utf16.wikilinks[0].raw, "Café Plan");
    assert_eq!(from_utf16.blocks[0].block_id, "u16-1");
}
