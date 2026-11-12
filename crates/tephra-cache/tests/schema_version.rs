//! `schema_version` table write-on-init, and match-no-op vs.
//! mismatch-rebuild branching (G5, F2).

mod common;

use rusqlite::Connection;

use tephra_cache::{Cache, SourceStatus, CURRENT_SCHEMA_VERSION};

#[test]
fn first_open_writes_schema_version_row_equal_to_current() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let version = cache
        .schema_version()
        .expect("schema_version")
        .expect("schema_version row must exist after the very first open");
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn reopen_with_matching_version_is_a_no_op_and_preserves_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    {
        let cache = Cache::open(&db_path).expect("first open");
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    }

    // Second open against the same file: version already matches
    // CURRENT_SCHEMA_VERSION, so this must be a no-op — the seeded
    // rows from the first open survive untouched.
    let cache = Cache::open(&db_path).expect("second open");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.source_row_count().unwrap(), 1);
    assert_eq!(cache.table_row_count("units").unwrap(), 2);
    assert_eq!(cache.table_row_count("links").unwrap(), 2);
    assert_eq!(cache.table_row_count("tags").unwrap(), 2);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 2);
}

#[test]
fn stale_version_row_triggers_rebuild_of_core_tables_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    // Bootstrap a real cache, seed it, then manually downgrade the
    // on-disk schema_version row to simulate an older build's cache
    // file being opened by this (newer) build.
    {
        let cache = Cache::open(&db_path).expect("first open");
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    }
    {
        let conn = Connection::open(&db_path).expect("reopen raw");
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION - 1],
        )
        .expect("seed stale version row");
    }

    let cache = Cache::open(&db_path).expect("open after stale version seeded");

    // Rebuild happened: schema_version is back to current, core tables
    // are empty (they were dropped and recreated), but sources (and
    // its one row) is untouched (F2/E2 — asserted more thoroughly in
    // rebuild.rs and offline_preservation.rs).
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(cache.table_row_count("links").unwrap(), 0);
    assert_eq!(cache.table_row_count("tags").unwrap(), 0);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 0);
    assert_eq!(cache.source_row_count().unwrap(), 1);
    // The open reports the version it rebuilt from, so the daemon can log it.
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(CURRENT_SCHEMA_VERSION - 1)
    );
}

#[test]
fn fresh_and_current_opens_report_no_rebuild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    let cache = Cache::open(&db_path).expect("fresh open");
    assert_eq!(cache.rebuilt_from_version(), None);
    drop(cache);

    let cache = Cache::open(&db_path).expect("reopen at current version");
    assert_eq!(cache.rebuilt_from_version(), None);
}

/// `(column name, declared type, notnull, default)` for every column of
/// `table`, via `PRAGMA table_info`.
fn columns(conn: &Connection, table: &str) -> Vec<(String, String, bool, Option<String>)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn names(cols: &[(String, String, bool, Option<String>)]) -> Vec<&str> {
    cols.iter().map(|c| c.0.as_str()).collect()
}

#[test]
fn v2_ddl_has_every_table_and_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();

    assert_eq!(CURRENT_SCHEMA_VERSION, 2);

    let units = columns(conn, "units");
    assert_eq!(
        names(&units),
        [
            "id",
            "source_id",
            "rel_path",
            "mtime",
            "frontmatter_json",
            "kind",
            "size",
            "too_large",
            "needs_rescan"
        ]
    );
    let col = |name: &str| units.iter().find(|c| c.0 == name).unwrap().clone();
    assert!(col("kind").2, "kind is NOT NULL");
    assert!(!col("size").2, "size is nullable");
    assert_eq!(
        (col("too_large").2, col("too_large").3.as_deref()),
        (true, Some("0"))
    );
    assert_eq!(
        (col("needs_rescan").2, col("needs_rescan").3.as_deref()),
        (true, Some("0"))
    );

    assert_eq!(
        names(&columns(conn, "headings")),
        ["id", "unit_id", "level", "text", "line"]
    );
    assert_eq!(
        names(&columns(conn, "symbols")),
        [
            "id",
            "unit_id",
            "kind",
            "name",
            "parent",
            "start_line",
            "end_line"
        ]
    );
    assert_eq!(
        names(&columns(conn, "blocks")),
        ["id", "unit_id", "block_id", "line", "end_line"]
    );
    assert_eq!(
        names(&columns(conn, "block_fts")),
        ["unit_id", "line", "text"]
    );
    assert_eq!(
        names(&columns(conn, "scan_state")),
        ["source_id", "started_at", "finished_at"]
    );

    // block_fts is an FTS5 table with the trigram tokenizer.
    let fts_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'block_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        fts_sql.contains("fts5") && fts_sql.contains("trigram"),
        "{fts_sql}"
    );

    // An index on every new unit_id column.
    for idx in [
        "idx_headings_unit_id",
        "idx_symbols_unit_id",
        "idx_blocks_unit_id",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [idx],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing index {idx}");
    }
}

#[test]
fn units_kind_check_rejects_an_unknown_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let source_id = cache
        .register_source("s", "/s", SourceStatus::Active)
        .unwrap();
    let err = cache.insert_unit(&tephra_cache::NewUnit {
        source_id,
        rel_path: "a.md",
        mtime: None,
        frontmatter_json: None,
        kind: "markdown",
        size: None,
        too_large: false,
    });
    assert!(err.is_err(), "the CHECK must reject an unknown kind");
}

#[test]
fn trigram_match_finds_a_substring_inside_an_identifier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    conn.execute(
        "INSERT INTO block_fts (unit_id, line, text) VALUES (7, 12, 'enum ConflictReason { A, B }')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO block_fts (unit_id, line, text) VALUES (8, 1, 'something unrelated')",
        [],
    )
    .unwrap();

    let hits: Vec<(i64, i64)> = conn
        .prepare("SELECT unit_id, line FROM block_fts WHERE block_fts MATCH 'onfl'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(hits, vec![(7, 12)]);
}

#[test]
fn two_blocks_with_the_same_unit_and_block_id_both_persist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let seeded = common::seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let unit_id = seeded.unit_ids[0];

    // The seed already stored `abcd1234` once on this unit; a second
    // occurrence (a different line range) must not be rejected or
    // merged.
    cache
        .insert_block(&tephra_cache::NewBlock {
            unit_id,
            block_id: "abcd1234",
            line: 40,
            end_line: Some(44),
        })
        .expect("a duplicate (unit_id, block_id) must be storable");

    let blocks = cache.blocks_for_unit(unit_id).unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|b| b.block_id == "abcd1234"));
    assert_eq!(
        blocks
            .iter()
            .map(|b| (b.line, b.end_line))
            .collect::<Vec<_>>(),
        vec![(1, None), (40, Some(44))]
    );
}

#[test]
fn deleting_a_unit_removes_its_headings_symbols_and_fts_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let seeded = common::seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let (gone, kept) = (seeded.unit_ids[0], seeded.unit_ids[1]);
    let conn = cache.connection();
    for unit in [gone, kept] {
        conn.execute(
            "INSERT INTO headings (unit_id, level, text, line) VALUES (?1, 1, 'H', 1)",
            [unit],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (unit_id, kind, name, parent, start_line, end_line) \
             VALUES (?1, 'fn', 'f', NULL, 1, 2)",
            [unit],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO block_fts (unit_id, line, text) VALUES (?1, 1, 'text here')",
            [unit],
        )
        .unwrap();
    }

    cache.delete_unit(gone).unwrap();
    for table in ["headings", "symbols", "block_fts"] {
        assert_eq!(cache.table_row_count(table).unwrap(), 1, "{table}");
        let left: i64 = conn
            .query_row(&format!("SELECT unit_id FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, kept, "{table}");
    }

    cache.delete_units_for_source(seeded.source_id).unwrap();
    for table in ["headings", "symbols", "block_fts"] {
        assert_eq!(cache.table_row_count(table).unwrap(), 0, "{table}");
    }
}

#[test]
fn a_v1_fixture_database_rebuilds_to_v2_on_first_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    common::write_v1_cache(&db_path);

    let cache = Cache::open(&db_path).expect("open v1 cache with the v2 build");
    assert_eq!(cache.schema_version().unwrap(), Some(2));
    let conn = cache.connection();
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert!(names(&columns(conn, "units")).contains(&"needs_rescan"));
    assert!(!columns(conn, "block_fts").is_empty());
    // Both sources' rows in `sources` survive; the scan_state bootstrap
    // table was created.
    assert_eq!(cache.source_row_count().unwrap(), 2);
    assert!(!columns(conn, "scan_state").is_empty());

    // A second open is a no-op.
    drop(cache);
    let cache = Cache::open(&db_path).expect("reopen");
    assert_eq!(cache.table_row_count("units").unwrap(), 2);
}

#[test]
fn scan_state_records_started_and_finished_and_is_not_dropped_by_a_rebuild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = common::open_temp_cache(&dir);
    let a = cache
        .register_source("a", "/a", SourceStatus::Active)
        .unwrap();
    let b = cache
        .register_source("b", "/b", SourceStatus::Active)
        .unwrap();
    assert!(cache.sources_with_unfinished_scan().unwrap().is_empty());

    cache.mark_scan_started(a).unwrap();
    cache.mark_scan_started(b).unwrap();
    cache.mark_scan_finished(a).unwrap();
    let names: Vec<_> = cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["b".to_string()]);

    // Starting again re-opens a finished scan.
    cache.mark_scan_started(a).unwrap();
    assert_eq!(cache.sources_with_unfinished_scan().unwrap().len(), 2);

    // scan_state is bootstrap: a rebuild keeps it, and (every source
    // being online and its rows just wiped) flags all of them.
    cache.mark_scan_finished(a).unwrap();
    cache.mark_scan_finished(b).unwrap();
    cache.rebuild(&tephra_cache::FaultInjector::none()).unwrap();
    assert_eq!(cache.sources_with_unfinished_scan().unwrap().len(), 2);

    // Removing a source removes its scan_state row too.
    cache.delete_source("a").unwrap();
    let names: Vec<_> = cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["b".to_string()]);
}
