//! `tests/integration/code_fixtures.rs`: golden snapshots of
//! `parse_code_unit` over one fixture per bundled grammar under
//! `tests/fixtures/code/<lang>/`. No mocks: the real parse, end to
//! end, for each fixture.
//!
//! Regenerate every snapshot with:
//!   UPDATE_SNAPSHOTS=1 cargo test -p tephra-parser --test integration

use std::path::{Path, PathBuf};

use tephra_parser::{parse_code_unit, Language};

/// `(directory, file)` under `tests/fixtures/code/`. The language is
/// not listed: it comes from `Language::from_path`, the same table the
/// scanner uses.
const FIXTURES: &[(&str, &str)] = &[
    ("python", "sample.py"),
    ("typescript", "sample.ts"),
    ("tsx", "sample.tsx"),
    ("javascript", "sample.js"),
    ("rust", "sample.rs"),
    ("go", "sample.go"),
    ("shell", "sample.sh"),
    ("yaml", "sample.yaml"),
    ("toml", "sample.toml"),
    ("c", "sample.c"),
    ("cpp", "sample.cpp"),
    ("java", "Sample.java"),
    ("swift", "sample.swift"),
    ("lua", "sample.lua"),
];

fn code_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code")
}

#[test]
fn every_code_fixture_matches_its_committed_golden_snapshot() {
    let update = std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1");
    let snapshots = code_dir().join("__snapshots__");
    if update {
        std::fs::create_dir_all(&snapshots).expect("create snapshots dir");
    }

    let mut failures = Vec::new();
    for (dir, file) in FIXTURES {
        let content = std::fs::read_to_string(code_dir().join(dir).join(file))
            .unwrap_or_else(|e| panic!("read fixture {dir}/{file}: {e}"));
        // A stable, fixture-relative path keeps the snapshot portable.
        let relative = Path::new("tests/fixtures/code").join(dir).join(file);
        let lang = Language::from_path(&relative, content.lines().next())
            .unwrap_or_else(|| panic!("{dir}/{file} has no language"));
        let actual = serde_json::to_string_pretty(&parse_code_unit(&relative, lang, &content))
            .expect("serialize ParsedUnit");

        let snapshot_path = snapshots.join(format!("{dir}.json"));
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
                "snapshot mismatch for {dir}/{file}:\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn reparsing_a_code_fixture_twice_is_byte_identical() {
    let path = code_dir().join("rust/sample.rs");
    let content = std::fs::read_to_string(&path).expect("read sample.rs");
    let a = parse_code_unit(&path, Language::Rust, &content);
    let b = parse_code_unit(&path, Language::Rust, &content);
    assert_eq!(a, b);
}
