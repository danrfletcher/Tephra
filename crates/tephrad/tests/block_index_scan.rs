//! The `^^` index (G10) as the scan writes it: the rows a full scan, a
//! rescan and the watcher's change batch leave in `block_fts` for each
//! kind of file, in the same transaction as the unit, and none for a
//! binary, a too-large or an unreadable file. Read back through the
//! cache's own search, the same one the daemon uses.

mod fixtures;

use std::collections::HashSet;
use std::path::PathBuf;

use tephra_cache::{Cache, SourceStatus};
use tephrad::file_kind::MAX_INDEXED_BYTES;
use tephrad::ignore::IgnoreRules;
use tephrad::scan;

struct Env {
    _cache_dir: tempfile::TempDir,
    cache: Cache,
    source_id: i64,
    root: PathBuf,
}

fn env_for(fixture: &fixtures::TempSource) -> Env {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");
    let root = fixture.root().canonicalize().expect("canonicalize");
    let source_id = cache
        .register_source("s", &root.to_string_lossy(), SourceStatus::Active)
        .expect("register_source");
    Env {
        _cache_dir: cache_dir,
        cache,
        source_id,
        root,
    }
}

fn scan_all(env: &Env) {
    scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
}

fn batch(env: &Env, paths: &[&str]) {
    let changed: HashSet<PathBuf> = paths.iter().map(|p| env.root.join(p)).collect();
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed,
        &IgnoreRules::default(),
    )
    .unwrap();
}

/// `(rel_path, kind, line)` of each hit for `query`.
fn found(env: &Env, query: &str) -> Vec<(String, String, Option<i64>)> {
    env.cache
        .search_all_blocks(query, 100)
        .unwrap()
        .into_iter()
        .map(|h| (h.rel_path, h.kind, h.line))
        .collect()
}

fn rows_of(env: &Env, rel: &str) -> i64 {
    let unit = env
        .cache
        .get_unit_by_rel_path(env.source_id, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("no unit {rel}"));
    env.cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM block_fts WHERE unit_id = ?1",
            [unit.id],
            |r| r.get(0),
        )
        .unwrap()
}

fn s(path: &str, kind: &str, line: i64) -> (String, String, Option<i64>) {
    (path.to_string(), kind.to_string(), Some(line))
}

#[test]
fn a_scan_writes_the_rows_each_kind_of_file_contributes() {
    let fixture = fixtures::empty_source();
    fixture.write(
        "n.md",
        "# Title needle\n\nA paragraph with a needle in it. ^abc\n\nNo match here.\n",
    );
    fixture.write(
        "c.rs",
        "// A needle in a comment.\nfn needle_finder() {}\nconst S: &str = \"needle // x\";\n",
    );
    fixture.write("t.txt", "first\n\nsecond needle\nline\n");
    fixture.write_bytes("b.bin", b"needle\0needle");
    fixture.write("empty.md", "");
    let env = env_for(&fixture);
    scan_all(&env);

    let mut got = found(&env, "needle");
    got.sort();
    assert_eq!(
        got,
        [
            s("c.rs", "comment", 1),
            s("c.rs", "symbol", 2),
            s("n.md", "heading", 1),
            s("n.md", "paragraph", 3),
            s("t.txt", "paragraph", 3),
        ]
    );
    // The paragraph's `^id` is its block ID and not part of its text.
    let hit = env
        .cache
        .search_all_blocks("paragraph with", 5)
        .unwrap()
        .remove(0);
    assert_eq!(hit.block_id.as_deref(), Some("abc"));
    assert!(!hit.snippet.contains("^abc"), "{}", hit.snippet);
    // Nothing for the binary, the empty file or in the string literal.
    assert_eq!(rows_of(&env, "b.bin"), 0);
    assert_eq!(rows_of(&env, "empty.md"), 0);
}

#[test]
fn documents_are_indexed_by_place() {
    let fixture = fixtures::empty_source();
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-parser/tests/fixtures/documents");
    fixture.write_bytes("deck.pptx", &std::fs::read(docs.join("deck.pptx")).unwrap());
    fixture.write_bytes(
        "scan.pdf",
        &std::fs::read(docs.join("no-text.pdf")).unwrap(),
    );
    fixture.write_bytes(
        "bad.docx",
        &std::fs::read(docs.join("truncated.docx")).unwrap(),
    );
    let env = env_for(&fixture);
    scan_all(&env);

    let hits = env.cache.search_all_blocks("Agenda", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].rel_path, "deck.pptx");
    assert_eq!(
        (hits[0].place.as_deref(), hits[0].place_n, hits[0].line),
        (Some("slide"), Some(2), None)
    );
    assert!(hits[0].block_id.is_none());
    // A scanned PDF and a corrupt docx have a unit and no rows.
    assert_eq!(rows_of(&env, "scan.pdf"), 0);
    assert_eq!(rows_of(&env, "bad.docx"), 0);
}

#[test]
fn the_change_batch_replaces_a_files_rows_and_removes_a_deleted_files() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "# One\n\nalpha text\n");
    fixture.write("t.txt", "alpha text\n");
    let env = env_for(&fixture);
    scan_all(&env);
    assert_eq!(
        found(&env, "alpha"),
        [s("n.md", "paragraph", 3), s("t.txt", "paragraph", 1)]
    );

    // Edited: the old rows go and the new ones come, never both.
    fixture.write("n.md", "# One\n\nbeta text\n\nmore\n\nbeta again\n");
    batch(&env, &["n.md"]);
    assert_eq!(found(&env, "alpha"), [s("t.txt", "paragraph", 1)]);
    assert_eq!(
        found(&env, "beta"),
        [s("n.md", "paragraph", 3), s("n.md", "paragraph", 7)]
    );
    // The same batch again changes nothing (no duplicates).
    batch(&env, &["n.md"]);
    assert_eq!(found(&env, "beta").len(), 2);
    assert_eq!(rows_of(&env, "n.md"), 4, "heading and three paragraphs");

    // A text file turned binary: its rows go.
    fixture.write_bytes("t.txt", &[0, 1, 2, 0]);
    batch(&env, &["t.txt"]);
    assert_eq!(rows_of(&env, "t.txt"), 0);
    assert!(found(&env, "alpha").is_empty());

    // A binary file turned into text: rows appear.
    fixture.write("t.txt", "gamma here\n");
    batch(&env, &["t.txt"]);
    assert_eq!(found(&env, "gamma"), [s("t.txt", "paragraph", 1)]);

    // Deleted: gone, with no row left for any unit id.
    fixture.remove("n.md");
    batch(&env, &["n.md"]);
    assert!(found(&env, "beta").is_empty());
    assert_eq!(env.cache.table_row_count("block_fts").unwrap(), 1);
}

/// A rename reaches the scan as the old path and the new one in one change
/// batch (the old one is gone from disk, the new one is new): the old
/// path's rows go, and the new path has exactly one set, the same as the
/// old one had. A renamed folder is the same with the folder's path.
#[test]
fn a_renamed_file_or_folder_moves_its_rows_and_leaves_none_behind() {
    let fixture = fixtures::empty_source();
    fixture.write("old.md", "# H\n\nneedle one\n\nneedle two ^id1\n");
    fixture.write("code.rs", "// needle comment\nfn needle_fn() {}\n");
    fixture.write("dir/x.txt", "needle in a folder\n\nsecond\n");
    fixture.write("other.md", "needle elsewhere\n");
    let env = env_for(&fixture);
    scan_all(&env);
    let before = env.cache.table_row_count("block_fts").unwrap();
    let (old_rows, code_rows, x_rows) = (
        rows_of(&env, "old.md"),
        rows_of(&env, "code.rs"),
        rows_of(&env, "dir/x.txt"),
    );
    assert_eq!((old_rows, code_rows, x_rows), (3, 2, 2));

    // A markdown file, a code file, and a folder, renamed in one batch.
    let root = &env.root;
    std::fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
    std::fs::rename(root.join("code.rs"), root.join("moved.rs")).unwrap();
    std::fs::rename(root.join("dir"), root.join("dir2")).unwrap();
    batch(
        &env,
        &["old.md", "new.md", "code.rs", "moved.rs", "dir", "dir2"],
    );

    for gone in ["old.md", "code.rs", "dir/x.txt"] {
        assert!(
            env.cache
                .get_unit_by_rel_path(env.source_id, gone)
                .unwrap()
                .is_none(),
            "{gone} still has a unit"
        );
    }
    assert_eq!(rows_of(&env, "new.md"), old_rows);
    assert_eq!(rows_of(&env, "moved.rs"), code_rows);
    assert_eq!(rows_of(&env, "dir2/x.txt"), x_rows);
    assert_eq!(
        env.cache.table_row_count("block_fts").unwrap(),
        before,
        "no row is left behind and none is doubled"
    );
    // Every hit is under a new path.
    let mut got = found(&env, "needle");
    got.sort();
    let paths: Vec<&str> = got.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "dir2/x.txt",
            "moved.rs",
            "moved.rs",
            "new.md",
            "new.md",
            "other.md"
        ]
    );
    // The same batch again changes nothing.
    batch(&env, &["old.md", "new.md"]);
    assert_eq!(env.cache.table_row_count("block_fts").unwrap(), before);
}

#[test]
fn a_rescan_leaves_exactly_one_set_of_rows() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "# H\n\ntext one\n\ntext two\n");
    fixture.write("c.py", "# note\ndef f():\n    pass\n");
    let env = env_for(&fixture);
    scan_all(&env);
    let count = env.cache.table_row_count("block_fts").unwrap();
    assert!(count >= 5, "{count}");
    for _ in 0..3 {
        env.cache.delete_units_for_source(env.source_id).unwrap();
        scan_all(&env);
        assert_eq!(env.cache.table_row_count("block_fts").unwrap(), count);
    }
}

#[test]
fn a_file_over_the_cap_is_a_unit_with_no_rows_and_is_not_read() {
    let fixture = fixtures::empty_source();
    fixture.write("small.md", "needle\n");
    let big = fixture.root().join("big.txt");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_INDEXED_BYTES + 1)
        .unwrap();
    let env = env_for(&fixture);
    scan_all(&env);
    let unit = env
        .cache
        .get_unit_by_rel_path(env.source_id, "big.txt")
        .unwrap()
        .unwrap();
    assert!(unit.too_large);
    assert_eq!(rows_of(&env, "big.txt"), 0);
    assert_eq!(found(&env, "needle"), [s("small.md", "paragraph", 1)]);
}

/// The rows of `rel` whose text holds `needle`: `(line, text)`.
fn rows_holding(env: &Env, rel: &str, needle: &str) -> Vec<(i64, String)> {
    let mut stmt = env
        .cache
        .connection()
        .prepare(
            "SELECT f.line, f.text FROM block_fts f JOIN units u ON u.id = f.unit_id \
             WHERE u.rel_path = ?1 AND instr(f.text, ?2) > 0 ORDER BY f.line",
        )
        .unwrap();
    stmt.query_map([rel, needle], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The line `needle` is on, worked out from the one row that holds it: the
/// row's start line plus the lines of its text before the needle.
fn needle_line(env: &Env, rel: &str, needle: &str) -> i64 {
    let rows = rows_holding(env, rel, needle);
    assert_eq!(rows.len(), 1, "{rel}: {rows:?}");
    let (line, text) = &rows[0];
    let before = &text[..text.find(needle).unwrap()];
    line + before.matches('\n').count() as i64
}

fn max_row_len(env: &Env, rel: &str) -> i64 {
    env.cache
        .connection()
        .query_row(
            "SELECT MAX(length(text)) FROM block_fts f JOIN units u ON u.id = f.unit_id \
             WHERE u.rel_path = ?1",
            [rel],
            |r| r.get(0),
        )
        .unwrap()
}

/// R2: a block over the row limit is split into rows, never cut off. A
/// text-other file with no blank lines (a CSV, a log, a lock file) is one
/// block, and a needle past its first 16K characters is still found, at
/// its own line.
#[test]
fn a_long_text_other_block_is_searchable_to_its_last_line() {
    let fixture = fixtures::empty_source();
    let mut csv = String::from("id,name,value\n");
    for i in 1..=3000 {
        csv.push_str(&format!("{i},row number {i},{}\n", i * 7));
    }
    csv.push_str("9999,the needle row,1\n");
    assert!(csv.len() > 4 * tephra_parser::MAX_ROW_CHARS);
    fixture.write("data.csv", &csv);
    let env = env_for(&fixture);
    scan_all(&env);

    let want_line = csv.lines().position(|l| l.contains("needle")).unwrap() as i64 + 1;
    assert_eq!(want_line, 3002);
    assert_eq!(needle_line(&env, "data.csv", "needle"), want_line);
    // Through the query too: one hit, a paragraph row of that file, its
    // line the start of the row that holds the needle.
    let hits = found(&env, "needle");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].0, "data.csv");
    assert!(hits[0].2.unwrap() <= want_line && want_line - hits[0].2.unwrap() < 100);
    assert!(max_row_len(&env, "data.csv") <= tephra_parser::MAX_ROW_CHARS as i64);
    // The rows near the start and the middle are still there.
    assert_eq!(found(&env, "row number 3,").len(), 1);
    assert_eq!(found(&env, "row number 1500,").len(), 1);
}

/// R2: the same for a prose paragraph and a comment block, each longer
/// than the limit: the needle on the last line is found at that line.
#[test]
fn a_long_paragraph_and_a_long_comment_block_are_searchable_to_their_last_line() {
    let fixture = fixtures::empty_source();
    let body: String = (1..=2500).map(|i| format!("filler words {i}\n")).collect();
    // One paragraph (no blank line) starting on line 3.
    fixture.write("p.md", &format!("# Top\n\n{body}the needle line\n"));
    let comments: String = (1..=2500)
        .map(|i| format!("// filler words {i}\n"))
        .collect();
    fixture.write(
        "c.rs",
        &format!("{comments}// the needle line\nfn f() {{}}\n"),
    );
    let env = env_for(&fixture);
    scan_all(&env);

    // Lines are exact for a paragraph (none dropped); a comment block's
    // text has its empty lines dropped, and there are none here.
    assert_eq!(needle_line(&env, "p.md", "needle"), 3 + 2500);
    assert_eq!(needle_line(&env, "c.rs", "needle"), 2501);
    for rel in ["p.md", "c.rs"] {
        assert!(max_row_len(&env, rel) <= tephra_parser::MAX_ROW_CHARS as i64);
    }
    let kinds: Vec<(String, String)> = found(&env, "needle")
        .into_iter()
        .map(|(rel, kind, _)| (rel, kind))
        .collect();
    assert!(kinds.contains(&(s("p.md", "paragraph", 0).0, "paragraph".to_string())));
    assert!(kinds.contains(&(s("c.rs", "comment", 0).0, "comment".to_string())));
}

/// R2: a minified file's one huge line is split between words, so a needle
/// at its end is found, on line 1; a run with no whitespace at all is cut
/// at the limit and a needle inside a cut piece is found too.
#[test]
fn a_huge_single_line_is_split_into_bounded_rows_and_still_searchable() {
    let fixture = fixtures::empty_source();
    let words: String = (0..40_000).map(|i| format!("w{i} ")).collect();
    fixture.write("min.txt", &format!("{words}needle\n"));
    fixture.write("min.rs", &format!("// {}needle\n", "y".repeat(200_000)));
    let env = env_for(&fixture);
    scan_all(&env);
    for rel in ["min.txt", "min.rs"] {
        assert!(
            max_row_len(&env, rel) <= tephra_parser::MAX_ROW_CHARS as i64,
            "{rel}"
        );
        assert!(rows_of(&env, rel) > 1, "{rel} was kept as one row");
    }
    assert_eq!(needle_line(&env, "min.txt", "needle"), 1);
    assert_eq!(needle_line(&env, "min.rs", "needle"), 1);
    // The middle of the long line is there too, whole.
    assert_eq!(found(&env, "w20000 ").len(), 1);
}
