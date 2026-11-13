//! G9: what `Cache::in_file_candidates` offers an in-file search for
//! each kind of unit and each scope, through the real `Cache` API on an
//! on-disk cache in a temp dir.
//!
//! The cache is seeded by the real scan (`tephrad::scan::scan_source`)
//! over a temp dir holding a file of every kind. `tephrad` depends on
//! this crate, so it is only a dev-dependency here: integration tests
//! link the one `tephra_cache` build, so the types match.

mod common;

use std::path::Path;

use common::open_temp_cache;
use tephra_cache::{
    Cache, InFileCandidate, InFileScope, NewBlock, NewHeading, SourceStatus, UnitRow,
};

const PROSE_MD: &str = "\
# First

Intro paragraph.
It runs over two lines.

## Second café

- a list item ^dup

Another paragraph. ^dup
";

const CODE_RS: &str = "\
fn run() {}

pub enum ConflictReason { // ^abc
    Name,
}
";

fn doc_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-parser/tests/fixtures/documents")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn write(root: &Path, rel_path: &str, bytes: &[u8]) {
    let full = root.join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, bytes).unwrap();
}

/// One unit of every kind, scanned from real files: the rules, not
/// the data, decide what comes back.
struct Fixture {
    cache: Cache,
    _dir: tempfile::TempDir,
    _source_dir: tempfile::TempDir,
    prose: UnitRow,
    code: UnitRow,
    docx: UnitRow,
    pdf: UnitRow,
    text_other: UnitRow,
    binary: UnitRow,
    too_large: UnitRow,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);

    let source_dir = tempfile::tempdir().unwrap();
    let root = source_dir.path().canonicalize().unwrap();
    write(&root, "notes/a.md", PROSE_MD.as_bytes());
    write(&root, "code/lib.rs", CODE_RS.as_bytes());
    write(&root, "docs/a.docx", &doc_fixture("headings.docx"));
    write(&root, "docs/a.pdf", &doc_fixture("text.pdf"));
    write(&root, "data/x.csv", b"a,b\n1,2\n");
    write(&root, "img/x.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0]);
    // Sparse, one byte over the indexing cap: indexed by name only (G7).
    let big = root.join("big.md");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(tephrad::file_kind::MAX_INDEXED_BYTES + 1)
        .unwrap();

    let source = cache
        .register_source("src", &root.to_string_lossy(), SourceStatus::Active)
        .unwrap();
    tephrad::scan::scan_source(
        &cache,
        source,
        &root,
        &tephrad::ignore::IgnoreRules::load(&root).unwrap(),
    )
    .expect("scan");

    let unit = |rel_path: &str| {
        cache
            .get_unit_by_rel_path(source, rel_path)
            .unwrap()
            .unwrap_or_else(|| panic!("{rel_path} not scanned"))
    };
    let prose = unit("notes/a.md");
    let code = unit("code/lib.rs");
    let docx = unit("docs/a.docx");
    let pdf = unit("docs/a.pdf");
    let text_other = unit("data/x.csv");
    let binary = unit("img/x.png");
    let too_large = unit("big.md");

    Fixture {
        cache,
        _dir: dir,
        _source_dir: source_dir,
        prose,
        code,
        docx,
        pdf,
        text_other,
        binary,
        too_large,
    }
}

fn heading_c(level: i64, text: &str, line: i64) -> InFileCandidate {
    InFileCandidate::Heading {
        level,
        text: text.to_string(),
        line,
    }
}

fn para(line: i64, end_line: i64, id: Option<&str>) -> InFileCandidate {
    InFileCandidate::Paragraph {
        line,
        end_line,
        block_id: id.map(str::to_string),
    }
}

fn doc_heading(level: i64, text: &str, number: i64) -> InFileCandidate {
    InFileCandidate::DocumentHeading {
        level,
        text: text.to_string(),
        number,
    }
}

fn doc_para(number: i64, index: i64) -> InFileCandidate {
    InFileCandidate::DocumentParagraph { number, index }
}

fn symbols_expected() -> Vec<InFileCandidate> {
    vec![
        InFileCandidate::Symbol {
            symbol_kind: "function".to_string(),
            name: "run".to_string(),
            parent: None,
            line: 1,
            block_id: None,
        },
        InFileCandidate::Symbol {
            symbol_kind: "enum".to_string(),
            name: "ConflictReason".to_string(),
            parent: None,
            line: 3,
            block_id: Some("abc".to_string()),
        },
    ]
}

/// The kind rule of G9 for every kind and scope, with the order.
#[test]
fn each_kind_and_scope_offers_exactly_what_g9_says() {
    use InFileScope::{Blocks, Headings};
    let f = fixture();
    let cases: Vec<(&str, &UnitRow, InFileScope, Vec<InFileCandidate>)> = vec![
        (
            "prose headings",
            &f.prose,
            Headings,
            vec![heading_c(1, "First", 1), heading_c(2, "Second café", 6)],
        ),
        // Both occurrences of a duplicate ID are listed, each at its own
        // line (E7).
        (
            "prose blocks",
            &f.prose,
            Blocks,
            vec![
                para(3, 4, None),
                para(8, 8, Some("dup")),
                para(10, 10, Some("dup")),
            ],
        ),
        // Settled detail: `headings` on code is its symbols.
        ("code headings", &f.code, Headings, symbols_expected()),
        ("code blocks", &f.code, Blocks, symbols_expected()),
        // Heading numbers are paragraph numbers: the empty paragraph
        // (5) is skipped.
        (
            "docx headings",
            &f.docx,
            Headings,
            vec![
                doc_heading(1, "Quarterly report", 1),
                doc_heading(1, "Overview", 2),
                doc_heading(2, "Details", 4),
                doc_heading(3, "A style known only by name", 6),
            ],
        ),
        (
            "docx blocks",
            &f.docx,
            Blocks,
            [1, 2, 3, 4, 6, 7, 8, 9]
                .into_iter()
                .map(|n| doc_para(n, n))
                .collect(),
        ),
        // A heading's number is its page.
        (
            "pdf headings",
            &f.pdf,
            Headings,
            vec![
                doc_heading(1, "Chapter one", 1),
                doc_heading(2, "Section 1.1", 1),
                doc_heading(1, "Chapter two", 2),
            ],
        ),
        // By page, then by index within the page.
        (
            "pdf blocks",
            &f.pdf,
            Blocks,
            vec![
                doc_para(1, 1),
                doc_para(1, 2),
                doc_para(1, 3),
                doc_para(1, 4),
                doc_para(2, 1),
                doc_para(2, 2),
            ],
        ),
        ("text-other headings", &f.text_other, Headings, vec![]),
        ("text-other blocks", &f.text_other, Blocks, vec![]),
        ("binary headings", &f.binary, Headings, vec![]),
        ("binary blocks", &f.binary, Blocks, vec![]),
        // G7: over the size cap, nothing is offered whatever the rows say.
        ("too_large headings", &f.too_large, Headings, vec![]),
        ("too_large blocks", &f.too_large, Blocks, vec![]),
    ];
    for (name, unit, scope, expected) in cases {
        let got = f
            .cache
            .in_file_candidates(unit, scope)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got, expected, "{name}");
    }
}

/// The kind decides, not the rows: a stray heading and block on a
/// kind with no parser, or on a file over the size cap, is not offered.
#[test]
fn stray_rows_on_a_kind_with_nothing_to_search_are_not_offered() {
    let f = fixture();
    for unit in [&f.text_other, &f.binary, &f.too_large] {
        f.cache
            .insert_heading(&NewHeading {
                unit_id: unit.id,
                level: 1,
                text: "stray",
                line: 1,
            })
            .unwrap();
        f.cache
            .insert_block(&NewBlock {
                unit_id: unit.id,
                block_id: None,
                line: 1,
                end_line: Some(1),
                symbol_id: None,
            })
            .unwrap();
        for scope in [InFileScope::Headings, InFileScope::Blocks] {
            assert_eq!(
                f.cache.in_file_candidates(unit, scope).unwrap(),
                vec![],
                "{} {scope:?}",
                unit.rel_path
            );
        }
    }
}

#[test]
fn scope_names_are_the_wire_names() {
    assert_eq!(InFileScope::Headings.as_str(), "headings");
    assert_eq!(InFileScope::Blocks.as_str(), "blocks");
}

/// The read works on a read-only connection and writes nothing (F4).
#[test]
fn a_read_only_connection_gets_the_same_candidates_and_changes_no_row() {
    let f = fixture();
    let path = f._dir.path().join("cache.db");
    let rows_before: i64 = f
        .cache
        .connection()
        .query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get(0))
        .unwrap();
    let ro = Cache::open_read_only(&path).expect("read-only open");
    let via_ro = ro
        .in_file_candidates(&f.prose, InFileScope::Blocks)
        .expect("read on a read-only connection");
    assert_eq!(
        via_ro,
        f.cache
            .in_file_candidates(&f.prose, InFileScope::Blocks)
            .unwrap()
    );
    let rows_after: i64 = ro
        .connection()
        .query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows_before, rows_after);
}
