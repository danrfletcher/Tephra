//! Integration tests for the G2/G7/E11 scan behaviour that needs a
//! real file tree and a real cache: the 20 MiB cap, kind changes
//! through the change-batch path, UTF-16 and mis-encoded Markdown, and
//! an interrupted scan being redone from the top.

mod fixtures;

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::PathBuf;

use tephra_cache::{Cache, SourceStatus};
use tephrad::file_kind::MAX_INDEXED_BYTES;
use tephrad::ignore::IgnoreRules;
use tephrad::registry::SourceRegistry;
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
    // Same canonicalisation `register` does, so paths line up on macOS.
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

fn unit(env: &Env, rel: &str) -> Option<tephra_cache::UnitRow> {
    env.cache
        .get_unit_by_rel_path(env.source_id, rel)
        .expect("get_unit_by_rel_path")
}

fn count(env: &Env, table: &str, col: &str, unit_id: i64) -> i64 {
    env.cache
        .connection()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = ?1"),
            [unit_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// A sparse file of exactly `len` bytes (all NUL): no real 20 MiB
/// write, and a read of it would be easy to spot.
fn make_sparse(path: &std::path::Path, len: u64) {
    make_sparse_with_head(path, b"", len);
}

/// A sparse file of exactly `len` bytes whose first bytes are `head`
/// and the rest NUL.
fn make_sparse_with_head(path: &std::path::Path, head: &[u8], len: u64) {
    use std::io::Write;
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    f.write_all(head).unwrap();
    f.set_len(len).unwrap();
}

#[test]
fn the_size_cap_is_inclusive_and_a_too_large_file_is_indexed_by_name_only() {
    let fixture = fixtures::empty_source();
    // At the cap: parsed normally. A 20 MiB Markdown file of text.
    let mut at_cap = b"# Big\n\n[[Target]] #tag\n".to_vec();
    at_cap.resize(MAX_INDEXED_BYTES as usize, b'x');
    fixture.write_bytes("at_cap.md", &at_cap);
    // One byte over: name only, never parsed. The Markdown file is text
    // so it keeps `prose`; the sparse files below are all NUL in their
    // first 8 KB, so they only come out right if the head is classified.
    let mut over = b"# Big\n\n[[Target]] #tag\n".to_vec();
    over.resize(MAX_INDEXED_BYTES as usize + 1, b'x');
    fixture.write_bytes("over.md", &over);
    make_sparse(&fixture.root().join("over.dat"), MAX_INDEXED_BYTES + 1);
    make_sparse(&fixture.root().join("over.rs"), MAX_INDEXED_BYTES + 1);
    make_sparse(&fixture.root().join("over.txt"), MAX_INDEXED_BYTES + 1);
    make_sparse(&fixture.root().join("over.pdf"), MAX_INDEXED_BYTES + 1);
    make_sparse(&fixture.root().join("sparse_at_cap.dat"), MAX_INDEXED_BYTES);
    // A PNG signature: unknown to the extension table, NUL in the header.
    make_sparse_with_head(
        &fixture.root().join("over.png"),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        MAX_INDEXED_BYTES + 1,
    );
    // 9000 bytes of text, then NULs: the NULs are past the 8 KB window,
    // so these are text. Nothing beyond the window is read or judged.
    let text_head = vec![b'a'; 9000];
    make_sparse_with_head(
        &fixture.root().join("over_text.log"),
        &text_head,
        MAX_INDEXED_BYTES + 1,
    );
    make_sparse_with_head(
        &fixture.root().join("over_text.py"),
        &text_head,
        MAX_INDEXED_BYTES + 1,
    );

    let env = env_for(&fixture);
    let stats = scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("scan");
    assert_eq!(stats.units_written, 10);

    let at = unit(&env, "at_cap.md").unwrap();
    assert!(!at.too_large);
    assert_eq!(at.size, Some(MAX_INDEXED_BYTES as i64));
    assert_eq!(at.kind, "prose");
    assert_eq!(count(&env, "links", "from_unit_id", at.id), 1, "parsed");
    assert_eq!(count(&env, "tags", "unit_id", at.id), 1, "parsed");

    for (rel, kind) in [
        ("over.md", "prose"),
        // NUL in the first 8 KB: binary, whatever the extension says
        // about prose or code or an unknown type.
        ("over.dat", "binary"),
        ("over.png", "binary"),
        ("over.txt", "binary"),
        ("over.rs", "binary"),
        // Text head: the real kind, by extension.
        ("over_text.log", "text-other"),
        ("over_text.py", "code"),
        // Documents are by extension, as under the cap.
        ("over.pdf", "document"),
    ] {
        let row = unit(&env, rel).unwrap_or_else(|| panic!("{rel} must still be a unit"));
        assert!(row.too_large, "{rel}");
        assert_eq!(row.size, Some(MAX_INDEXED_BYTES as i64 + 1), "{rel}");
        assert_eq!(row.kind, kind, "{rel}: real kind from name and first 8 KB");
        assert_eq!(count(&env, "links", "from_unit_id", row.id), 0, "{rel}");
        assert_eq!(count(&env, "tags", "unit_id", row.id), 0, "{rel}");
        assert_eq!(count(&env, "blocks", "unit_id", row.id), 0, "{rel}");
    }

    // Exactly the cap, not Markdown, all NULs: read and classified, so
    // binary — and not flagged too_large.
    let sparse = unit(&env, "sparse_at_cap.dat").unwrap();
    assert!(!sparse.too_large);
    assert_eq!(sparse.kind, "binary");
}

#[test]
fn a_utf16_markdown_file_with_a_bom_is_parsed_and_invalid_markdown_is_binary() {
    let fixture = fixtures::empty_source();
    let text = "# Título\n\nSee [[café]] and #sixteen ^abc123\n";
    let mut utf16 = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        utf16.extend(u.to_le_bytes());
    }
    fixture.write_bytes("wide.md", &utf16);
    // Plain ASCII for the first 8 KB, invalid UTF-8 after it: the head
    // check passes, so the full parse is what finds the problem.
    let mut late_bad = vec![b'a'; 9000];
    late_bad.extend([0xFF, 0xFE, b'\n']);
    fixture.write_bytes("late_bad.md", &late_bad);
    // NUL early: binary straight from the classifier.
    fixture.write_bytes("nul.md", b"# hi\0\n[[x]]\n");

    let env = env_for(&fixture);
    scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("scan: one bad file must not abort it");

    let wide = unit(&env, "wide.md").unwrap();
    assert_eq!(wide.kind, "prose");
    assert_eq!(wide.size, Some(utf16.len() as i64));
    assert_eq!(count(&env, "links", "from_unit_id", wide.id), 1);
    assert_eq!(count(&env, "tags", "unit_id", wide.id), 1);
    assert_eq!(count(&env, "blocks", "unit_id", wide.id), 1);

    for rel in ["late_bad.md", "nul.md"] {
        let row = unit(&env, rel).unwrap_or_else(|| panic!("{rel} must still be a unit"));
        assert_eq!(row.kind, "binary", "{rel}");
        assert_eq!(count(&env, "links", "from_unit_id", row.id), 0, "{rel}");
    }
}

#[test]
fn a_changed_file_is_reclassified_by_the_change_batch_and_leaves_no_stale_rows() {
    let fixture = fixtures::empty_source();
    fixture.write("note.md", "# N\n\n[[Other]] #t ^blk99\n");
    let env = env_for(&fixture);
    let rules = IgnoreRules::default();
    scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();
    let before = unit(&env, "note.md").unwrap();
    assert_eq!(before.kind, "prose");
    assert_eq!(count(&env, "links", "from_unit_id", before.id), 1);

    let changed = |p: &str| -> HashSet<PathBuf> { HashSet::from([env.root.join(p)]) };

    // Overwritten with binary bytes: kind flips, the old rows go.
    fixture.write_bytes("note.md", &[0, 1, 2, 3, 0, 0]);
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed("note.md"),
        &rules,
    )
    .unwrap();
    let after = unit(&env, "note.md").unwrap();
    assert_eq!(after.kind, "binary");
    assert_eq!(after.size, Some(6));
    for (table, col) in [
        ("links", "from_unit_id"),
        ("tags", "unit_id"),
        ("blocks", "unit_id"),
    ] {
        assert_eq!(count(&env, table, col, before.id), 0, "{table} of old id");
        assert_eq!(count(&env, table, col, after.id), 0, "{table} of new id");
    }
    assert_eq!(env.cache.table_row_count("units").unwrap(), 1);

    // A brand-new code file arrives through the same path.
    fixture.write("tool.py", "print('[[x]]')\n");
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed("tool.py"),
        &rules,
    )
    .unwrap();
    assert_eq!(unit(&env, "tool.py").unwrap().kind, "code");

    // An ignored name through the batch path is never written.
    fixture.write(".DS_Store", "x");
    fixture.write(".venv/lib/a.py", "x");
    let mut both = changed(".DS_Store");
    both.extend(changed(".venv/lib/a.py"));
    scan::apply_change_batch(&env.cache, env.source_id, &env.root, &both, &rules).unwrap();
    assert!(unit(&env, ".DS_Store").is_none());
    assert!(unit(&env, ".venv/lib/a.py").is_none());

    // And a deleted file's row goes.
    fixture.remove("tool.py");
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &changed("tool.py"),
        &rules,
    )
    .unwrap();
    assert!(unit(&env, "tool.py").is_none());
}

#[test]
fn a_scan_records_started_and_finished_and_a_killed_scan_stays_unfinished() {
    let fixture = fixtures::kinds_fixture();
    let env = env_for(&fixture);
    let rules = IgnoreRules::default();
    assert!(env.cache.sources_with_unfinished_scan().unwrap().is_empty());

    scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();
    assert!(
        env.cache.sources_with_unfinished_scan().unwrap().is_empty(),
        "a completed scan is finished"
    );

    // A scan that dies mid-way (the process was killed after "started"
    // and after some rows were written) leaves exactly this state.
    env.cache.mark_scan_started(env.source_id).unwrap();
    let names: Vec<_> = env
        .cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["s".to_string()]);

    // A completed rescan clears it.
    scan::rescan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();
    assert!(env.cache.sources_with_unfinished_scan().unwrap().is_empty());
}

#[test]
fn restart_after_a_kill_mid_scan_redoes_the_scan_from_the_top() {
    // E11 through the real registry: register (full scan), simulate the
    // kill by re-marking the scan started and leaving a partial row
    // set, then "restart" = a fresh registry over the same cache file
    // running the startup recovery that `main.rs` runs.
    let fixture = fixtures::kinds_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let expected_units = fixtures::KINDS_EXPECTED.len() as i64;

    let source_id = {
        let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
        registry.register("kinds", fixture.root()).unwrap();
        let id = registry.list().unwrap()[0].id;
        // Nothing unfinished after a clean register: a restart now is a no-op.
        assert!(registry.rescan_unfinished(|| false).is_empty());
        id
    };
    {
        // The "kill": started, never finished, only part of the rows.
        let cache = Cache::open(&cache_path).unwrap();
        cache.mark_scan_started(source_id).unwrap();
        let doomed: Vec<i64> = cache
            .connection()
            .prepare("SELECT id FROM units WHERE rel_path NOT IN ('Cargo.toml', 'notes/one.md')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for id in doomed {
            cache.delete_unit(id).unwrap();
        }
        assert_eq!(cache.table_row_count("units").unwrap(), 2);
    }

    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();

    // A shutdown requested before the first source starts leaves it alone.
    assert!(registry.rescan_unfinished(|| true).is_empty());
    assert_eq!(
        Cache::open(&cache_path)
            .unwrap()
            .table_row_count("units")
            .unwrap(),
        2
    );

    let outcomes = registry.rescan_unfinished(|| false);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, "kinds");
    let stats = outcomes[0].1.as_ref().expect("rescan ok");
    assert_eq!(stats.units_written as i64, expected_units);

    let cache = Cache::open(&cache_path).unwrap();
    assert_eq!(
        cache.table_row_count("units").unwrap(),
        expected_units,
        "same unit count as a clean scan, no duplicates"
    );
    let distinct: i64 = cache
        .connection()
        .query_row(
            "SELECT COUNT(DISTINCT rel_path) FROM units WHERE source_id = ?1",
            [source_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, expected_units);
    assert!(cache.sources_with_unfinished_scan().unwrap().is_empty());
    // The redo is itself one-shot.
    assert!(registry.rescan_unfinished(|| false).is_empty());
}

#[test]
fn an_unfinished_source_whose_root_vanished_goes_offline_and_stays_unfinished() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let root = fixture.root().canonicalize().unwrap();
    {
        let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
        registry.register("v", &root).unwrap();
    }
    Cache::open(&cache_path)
        .unwrap()
        .mark_scan_started(1)
        .unwrap();
    drop(fixture); // deletes the directory

    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    let outcomes = registry.rescan_unfinished(|| false);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].1.is_ok(), "a missing root is not an error");
    let cache = Cache::open(&cache_path).unwrap();
    assert_eq!(
        cache.get_source("v").unwrap().unwrap().status,
        SourceStatus::Offline
    );
    assert_eq!(cache.sources_with_unfinished_scan().unwrap().len(), 1);
}

#[test]
fn a_rescan_of_a_carried_over_offline_source_clears_needs_rescan_and_fills_kind_and_size() {
    // E10: a v1 cache whose source is offline; the new build carries its
    // rows over with an extension-inferred kind, NULL size and
    // needs_rescan = 1. Once the root is reachable again, a rescan
    // replaces them with fully classified rows.
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "# A\n\n[[B]]\n");
    fixture.write("src/lib.rs", "fn x() {}\n");
    fixture.write("Cargo.lock", "# lock\n");
    let root = fixture.root().canonicalize().unwrap();

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    {
        let conn = rusqlite::Connection::open(&cache_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE sources (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
                path TEXT NOT NULL, status TEXT NOT NULL CHECK (status IN ('active','stale','offline')),
                last_scanned_at INTEGER);
             CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (1);
             CREATE TABLE units (id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id INTEGER NOT NULL REFERENCES sources(id), rel_path TEXT NOT NULL,
                mtime INTEGER, frontmatter_json TEXT, UNIQUE(source_id, rel_path));
             CREATE TABLE links (id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_unit_id INTEGER NOT NULL REFERENCES units(id), to_raw TEXT NOT NULL,
                to_unit_id INTEGER REFERENCES units(id), kind TEXT NOT NULL);
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT,
                unit_id INTEGER NOT NULL REFERENCES units(id), tag TEXT NOT NULL);
             CREATE TABLE blocks (id INTEGER PRIMARY KEY AUTOINCREMENT,
                unit_id INTEGER NOT NULL REFERENCES units(id), block_id TEXT NOT NULL,
                line INTEGER NOT NULL);
             INSERT INTO sources VALUES (1, 'v', '{}', 'offline', 1);
             INSERT INTO units (id, source_id, rel_path, mtime) VALUES
                (1, 1, 'a.md', 5), (2, 1, 'src/lib.rs', 5), (3, 1, 'Cargo.lock', 5);",
            root.display()
        ))
        .unwrap();
    }

    // The daemon's own open: extension table from `file_kind`.
    let cache =
        Cache::open_with_kind_inferrer(&cache_path, tephrad::file_kind::infer_kind_from_path)
            .unwrap();
    let carried = |rel: &str| {
        cache
            .get_unit_by_rel_path(1, rel)
            .unwrap()
            .unwrap_or_else(|| panic!("{rel} carried over"))
    };
    for (rel, kind) in [
        ("a.md", "prose"),
        ("src/lib.rs", "code"),
        ("Cargo.lock", "text-other"),
    ] {
        let row = carried(rel);
        assert_eq!(row.kind, kind, "{rel}");
        assert_eq!(row.size, None, "{rel}");
        assert!(row.needs_rescan, "{rel}");
    }
    // An offline source isn't flagged for the startup rescan.
    assert!(cache.sources_with_unfinished_scan().unwrap().is_empty());
    drop(cache);

    let registry = SourceRegistry::new(
        Cache::open_with_kind_inferrer(&cache_path, tephrad::file_kind::infer_kind_from_path)
            .unwrap(),
    )
    .unwrap();
    registry.rescan("v").expect("rescan once the root is back");

    let cache = Cache::open(&cache_path).unwrap();
    assert_eq!(
        cache.get_source("v").unwrap().unwrap().status,
        SourceStatus::Active
    );
    for (rel, kind) in [
        ("a.md", "prose"),
        ("src/lib.rs", "code"),
        ("Cargo.lock", "text-other"),
    ] {
        let row = cache.get_unit_by_rel_path(1, rel).unwrap().unwrap();
        let on_disk = std::fs::metadata(root.join(rel)).unwrap().len() as i64;
        assert_eq!(row.kind, kind, "{rel}");
        assert_eq!(row.size, Some(on_disk), "{rel}: size filled");
        assert!(!row.needs_rescan, "{rel}: needs_rescan cleared");
    }
    assert_eq!(cache.table_row_count("units").unwrap(), 3);
}

#[test]
#[cfg(unix)]
fn an_unreadable_file_is_skipped_with_a_warning_and_the_scan_carries_on() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixtures::empty_source();
    fixture.write("good.md", "# Good\n");
    fixture.write("secret.txt", "hidden\n");
    fixture.write("secret.rs", "fn x() {}\n");
    fixture.write("later.py", "x = 1\n");
    let secret_paths = ["secret.txt", "secret.rs"].map(|p| fixture.root().join(p));
    for p in &secret_paths {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Root bypasses permission bits; the check can't mean anything there.
    let readable_anyway = std::fs::read(&secret_paths[0]).is_ok();

    let env = env_for(&fixture);
    let result = scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    );
    for p in &secret_paths {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let stats = result.expect("one unreadable file must not abort the scan");

    assert!(unit(&env, "good.md").is_some());
    assert!(unit(&env, "later.py").is_some());
    if readable_anyway {
        return;
    }
    // Skipped: not a unit at all (its kind can't be known), and the
    // other files are.
    assert!(unit(&env, "secret.txt").is_none());
    assert!(unit(&env, "secret.rs").is_none());
    assert_eq!(stats.units_written, 2);
}

#[derive(Clone, Default)]
struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

#[test]
fn a_too_large_file_logs_one_info_line_and_a_nul_file_logs_nothing() {
    // E3: exactly one `info` line naming the path and the size; E2: a
    // NUL-byte `.txt`/`.py` is stored as binary without any warning.
    let fixture = fixtures::empty_source();
    // A text head past the 8 KB window, then NULs: still `prose`.
    make_sparse_with_head(
        &fixture.root().join("big.md"),
        &vec![b'a'; 9000],
        MAX_INDEXED_BYTES + 1,
    );
    fixture.write_bytes("nul.txt", b"abc\0def");
    fixture.write_bytes("nul.py", b"print(1)\0");
    fixture.write("fine.md", "# ok\n");
    let env = env_for(&fixture);

    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let stats = tracing::subscriber::with_default(subscriber, || {
        scan::scan_source(
            &env.cache,
            env.source_id,
            &env.root,
            &IgnoreRules::default(),
        )
    })
    .expect("scan carries on");
    assert_eq!(stats.units_written, 4);

    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let big: Vec<&&str> = lines.iter().filter(|l| l.contains("big.md")).collect();
    assert_eq!(big.len(), 1, "{lines:?}");
    assert!(big[0].contains("INFO"), "{}", big[0]);
    assert!(
        big[0].contains(&(MAX_INDEXED_BYTES + 1).to_string()),
        "{}",
        big[0]
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("WARN") || l.contains("nul.txt") || l.contains("nul.py")),
        "no warning and no mention of the NUL files: {lines:?}"
    );
    assert_eq!(unit(&env, "nul.txt").unwrap().kind, "binary");
    assert_eq!(unit(&env, "nul.py").unwrap().kind, "binary");
    let big = unit(&env, "big.md").unwrap();
    assert_eq!((big.kind.as_str(), big.too_large), ("prose", true));
}

fn headings_of(env: &Env, rel: &str) -> Vec<(i64, String, i64)> {
    let id = unit(env, rel).expect("unit").id;
    let conn = env.cache.connection();
    let mut stmt = conn
        .prepare("SELECT level, text, line FROM headings WHERE unit_id = ?1 ORDER BY line, id")
        .unwrap();
    stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn listed_title(env: &Env, rel: &str) -> Option<String> {
    let listing = env.cache.list_folder("s", "", true).expect("list_folder");
    listing
        .entries
        .into_iter()
        .find(|e| e.rel_path == rel)
        .unwrap_or_else(|| panic!("{rel} is listed"))
        .title
}

#[test]
fn a_scan_and_a_change_batch_cache_headings_and_the_listing_title_falls_back_to_the_first() {
    let fixture = fixtures::empty_source();
    fixture.write("h.md", "intro\n\n# First\n\ntext\n\n## Second\n");
    fixture.write("front.md", "---\ntitle: From Frontmatter\n---\n# Ignored\n");
    fixture.write("none.md", "no headings here\n");
    fixture.write("code.rs", "// # not a heading\nfn main() {}\n");
    let env = env_for(&fixture);
    let rules = IgnoreRules::default();
    scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();

    assert_eq!(
        headings_of(&env, "h.md"),
        [(1, "First".to_string(), 3), (2, "Second".to_string(), 7)]
    );
    assert_eq!(listed_title(&env, "h.md").as_deref(), Some("First"));
    assert_eq!(
        listed_title(&env, "front.md").as_deref(),
        Some("From Frontmatter")
    );
    assert_eq!(listed_title(&env, "none.md"), None);
    assert_eq!(env.cache.table_row_count("headings").unwrap(), 3);

    // An edit through the change batch replaces the old headings
    // rather than adding to them.
    fixture.write("h.md", "# Renamed\n");
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &HashSet::from([env.root.join("h.md")]),
        &rules,
    )
    .unwrap();
    assert_eq!(headings_of(&env, "h.md"), [(1, "Renamed".to_string(), 1)]);
    assert_eq!(listed_title(&env, "h.md").as_deref(), Some("Renamed"));
    assert_eq!(env.cache.table_row_count("headings").unwrap(), 2);

    // A file that stops being prose keeps no headings or title.
    fixture.write_bytes("h.md", &[0, 1, 2, 0]);
    scan::apply_change_batch(
        &env.cache,
        env.source_id,
        &env.root,
        &HashSet::from([env.root.join("h.md")]),
        &rules,
    )
    .unwrap();
    assert_eq!(env.cache.table_row_count("headings").unwrap(), 1);

    // A forced rescan rebuilds the same rows, with no duplicates.
    scan::rescan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();
    assert_eq!(env.cache.table_row_count("headings").unwrap(), 1);
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    fn outside() -> fixtures::TempSource {
        let outside = fixtures::empty_source();
        outside.write("secret.md", "# Secret\n\n[[x]] #leak\n");
        outside.write("deeper/more.txt", "more\n");
        outside
    }

    fn rel_paths(env: &Env) -> Vec<String> {
        let mut paths: Vec<String> = env
            .cache
            .connection()
            .prepare("SELECT rel_path FROM units ORDER BY rel_path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn a_scan_never_reads_through_a_symlinked_file_or_folder() {
        let outside = outside();
        let fixture = fixtures::empty_source();
        fixture.write("real.md", "# Real\n");
        symlink(outside.root(), fixture.root().join("link_dir")).unwrap();
        symlink(
            outside.root().join("secret.md"),
            fixture.root().join("link_file.md"),
        )
        .unwrap();
        // A link to a file inside the root is not a unit either.
        symlink(
            fixture.root().join("real.md"),
            fixture.root().join("alias.md"),
        )
        .unwrap();
        let env = env_for(&fixture);

        scan::scan_source(
            &env.cache,
            env.source_id,
            &env.root,
            &IgnoreRules::default(),
        )
        .unwrap();

        assert_eq!(rel_paths(&env), ["real.md"]);
        assert_eq!(env.cache.table_row_count("headings").unwrap(), 1);
        assert_eq!(env.cache.table_row_count("tags").unwrap(), 0);
        assert_eq!(env.cache.table_row_count("links").unwrap(), 0);
    }

    /// The reported leak: a link to another tree made while the daemon
    /// is already watching arrives as a change batch naming the link
    /// and, on some platforms, paths below it.
    #[test]
    fn a_change_batch_never_indexes_a_late_symlink_or_anything_below_it() {
        let outside = outside();
        let fixture = fixtures::empty_source();
        fixture.write("real.md", "# Real\n");
        let env = env_for(&fixture);
        let rules = IgnoreRules::default();
        scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();

        symlink(outside.root(), fixture.root().join("late_link")).unwrap();
        symlink(
            outside.root().join("secret.md"),
            fixture.root().join("late_file.md"),
        )
        .unwrap();
        let batch: HashSet<PathBuf> = [
            "late_link",
            "late_link/secret.md",
            "late_link/deeper",
            "late_link/deeper/more.txt",
            "late_file.md",
        ]
        .iter()
        .map(|p| env.root.join(p))
        .collect();
        let outcome =
            scan::apply_change_batch(&env.cache, env.source_id, &env.root, &batch, &rules).unwrap();

        assert_eq!(rel_paths(&env), ["real.md"]);
        assert!(
            outcome.changed_paths.is_empty(),
            "{:?}",
            outcome.changed_paths
        );
        assert_eq!(env.cache.table_row_count("tags").unwrap(), 0);
    }

    /// A directory that arrives whole (a create or a move-in) is walked;
    /// a link inside it is not followed, and the walk's own root being
    /// a link is refused too.
    #[test]
    fn a_walked_directory_skips_links_inside_it_and_a_linked_directory_root() {
        let outside = outside();
        let fixture = fixtures::empty_source();
        fixture.write("real.md", "# Real\n");
        let env = env_for(&fixture);
        let rules = IgnoreRules::default();
        scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();

        fixture.write("moved/in.md", "# In\n");
        symlink(outside.root(), fixture.root().join("moved/escape")).unwrap();
        symlink(outside.root(), fixture.root().join("as_dir")).unwrap();
        let batch: HashSet<PathBuf> = ["moved", "as_dir"]
            .iter()
            .map(|p| env.root.join(p))
            .collect();
        scan::apply_change_batch(&env.cache, env.source_id, &env.root, &batch, &rules).unwrap();

        assert_eq!(rel_paths(&env), ["moved/in.md", "real.md"]);
    }

    /// Rows indexed through a link before the check existed (or by a
    /// bug) are dropped the next time the link shows up in a batch.
    #[test]
    fn rows_cached_below_a_symlink_are_dropped_when_it_next_changes() {
        let outside = outside();
        let fixture = fixtures::empty_source();
        fixture.write("real.md", "# Real\n");
        let env = env_for(&fixture);
        let rules = IgnoreRules::default();
        scan::scan_source(&env.cache, env.source_id, &env.root, &rules).unwrap();

        for rel in [
            "late_link/secret.md",
            "late_link/deeper/more.txt",
            "late_link",
        ] {
            env.cache
                .insert_unit(&tephra_cache::NewUnit {
                    source_id: env.source_id,
                    rel_path: rel,
                    mtime: None,
                    frontmatter_json: None,
                    kind: "text-other",
                    size: Some(1),
                    too_large: false,
                })
                .unwrap();
        }
        // A sibling that merely shares the name prefix must survive.
        fixture.write("late_link_notes.md", "# Mine\n");
        scan::apply_change_batch(
            &env.cache,
            env.source_id,
            &env.root,
            &HashSet::from([env.root.join("late_link_notes.md")]),
            &rules,
        )
        .unwrap();
        symlink(outside.root(), fixture.root().join("late_link")).unwrap();

        let outcome = scan::apply_change_batch(
            &env.cache,
            env.source_id,
            &env.root,
            &HashSet::from([env.root.join("late_link")]),
            &rules,
        )
        .unwrap();

        assert_eq!(rel_paths(&env), ["late_link_notes.md", "real.md"]);
        assert_eq!(outcome.changed_paths, [env.root.join("late_link")]);
    }
}
