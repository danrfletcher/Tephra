//! `tests/integration/documents_fixtures.rs`: golden snapshots of
//! `parse_document_bytes` over the committed documents under
//! `tests/fixtures/documents/`. No mocks: the real parse of the real
//! file. A fixture that cannot be read snapshots its error.
//!
//! Regenerate every snapshot with:
//!   UPDATE_SNAPSHOTS=1 cargo test -p tephra-parser --test integration
//! (the fixtures themselves come from `generate.py` in that directory.)

use std::path::{Path, PathBuf};

use tephra_parser::parse_document_bytes;

const FIXTURES: &[&str] = &[
    "headings.docx",
    "split-runs.docx",
    "truncated.docx",
    "empty.docx",
    "deck.pptx",
    "empty.pptx",
    "text.pdf",
    "no-text.pdf",
    "empty.pdf",
    "encrypted.pdf",
    "garbage.pdf",
];

fn documents_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/documents")
}

fn snapshot_of(name: &str) -> String {
    let bytes = std::fs::read(documents_dir().join(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
    // A stable, fixture-relative path keeps the snapshot portable.
    let relative = Path::new("tests/fixtures/documents").join(name);
    let value = match parse_document_bytes(&relative, &bytes) {
        Ok(doc) => serde_json::to_value(&doc).expect("serialize ParsedDocument"),
        Err(e) => serde_json::json!({ "error": e }),
    };
    serde_json::to_string_pretty(&value).expect("render snapshot")
}

#[test]
fn every_document_fixture_matches_its_committed_golden_snapshot() {
    let update = std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1");
    let snapshots = documents_dir().join("__snapshots__");
    if update {
        std::fs::create_dir_all(&snapshots).expect("create snapshots dir");
    }

    let mut failures = Vec::new();
    for name in FIXTURES {
        let actual = snapshot_of(name);
        let snapshot_path = snapshots.join(format!("{name}.json"));
        if update {
            std::fs::write(&snapshot_path, format!("{actual}\n")).expect("write snapshot");
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
fn reparsing_a_document_fixture_is_identical() {
    for name in FIXTURES {
        assert_eq!(snapshot_of(name), snapshot_of(name), "{name}");
    }
}
