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
];

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
        let md_path = fixtures_dir().join(name);
        let content = std::fs::read_to_string(&md_path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", md_path.display()));

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
