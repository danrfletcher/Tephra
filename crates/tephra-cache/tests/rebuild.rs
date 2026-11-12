//! Rebuild preserves `sources` row count and column values; rebuild is
//! atomic under a simulated panic mid-transaction (F2).

mod common;

use tephra_cache::{Cache, FaultInjector, SourceStatus, CURRENT_SCHEMA_VERSION};

#[test]
fn rebuild_preserves_sources_row_count_and_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Stale);

    let before = cache.list_sources().expect("list before");
    assert_eq!(before.len(), 2);

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    let after = cache.list_sources().expect("list after");
    assert_eq!(after, before, "rebuild must not alter sources rows at all");
    assert_eq!(cache.source_row_count().unwrap(), 2);

    // Core tables were dropped and recreated empty.
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(cache.table_row_count("links").unwrap(), 0);
    assert_eq!(cache.table_row_count("tags").unwrap(), 0);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 0);

    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
}

#[test]
fn rebuild_is_atomic_under_injected_failure_after_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    let mut cache = Cache::open(&db_path).expect("open");
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    let version_before = cache.schema_version().unwrap();
    let sources_before = cache.list_sources().unwrap();
    let units_before = cache.table_row_count("units").unwrap();

    // Force a failure between dropping the old core tables and
    // recreating them. The in-progress transaction must never be
    // committed, so SQLite rolls it back entirely.
    let result = cache.rebuild(&FaultInjector {
        fail_after_drop: true,
    });
    assert!(result.is_err(), "injected failure must surface as an error");

    // Re-open fresh from disk (not just re-check the live connection)
    // to prove the rollback was durable, not merely an in-memory
    // illusion on the same `Connection`.
    drop(cache);
    let cache = Cache::open(&db_path).expect("reopen after failed rebuild");

    assert_eq!(
        cache.schema_version().unwrap(),
        version_before,
        "schema_version must be untouched by a rolled-back rebuild"
    );
    assert_eq!(cache.list_sources().unwrap(), sources_before);
    assert_eq!(
        cache.table_row_count("units").unwrap(),
        units_before,
        "core tables (and their rows) must survive a rolled-back rebuild intact"
    );
}

#[test]
fn injected_failure_mid_rebuild_leaves_every_v2_table_and_row_intact() {
    // Like the atomicity test above, but with rows in every table v2
    // added, including the FTS5 virtual table (dropping and restoring a
    // virtual table inside a rolled-back transaction is its own risk).
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    let mut cache = Cache::open(&db_path).expect("open");
    let seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let unit = seeded.unit_ids[0];
    {
        let conn = cache.connection();
        conn.execute(
            "INSERT INTO headings (unit_id, level, text, line) VALUES (?1, 2, 'Title', 3)",
            [unit],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (unit_id, kind, name, parent, start_line, end_line) \
             VALUES (?1, 'fn', 'main', NULL, 1, 9)",
            [unit],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO block_fts (unit_id, line, text) VALUES (?1, 1, 'ConflictReason')",
            [unit],
        )
        .unwrap();
    }
    cache.mark_scan_started(seeded.source_id).unwrap();
    cache.mark_scan_finished(seeded.source_id).unwrap();

    let result = cache.rebuild(&FaultInjector {
        fail_after_drop: true,
    });
    assert!(result.is_err());
    drop(cache);

    let cache = Cache::open(&db_path).expect("reopen after failed rebuild");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    for (table, rows) in [
        ("units", 2),
        ("blocks", 2),
        ("headings", 1),
        ("symbols", 1),
        ("block_fts", 1),
        ("scan_state", 1),
    ] {
        assert_eq!(cache.table_row_count(table).unwrap(), rows, "{table}");
    }
    let hit: i64 = cache
        .connection()
        .query_row(
            "SELECT unit_id FROM block_fts WHERE block_fts MATCH 'onfl'",
            [],
            |r| r.get(0),
        )
        .expect("the FTS index still answers after the rolled-back rebuild");
    assert_eq!(hit, unit);
    assert!(
        cache.sources_with_unfinished_scan().unwrap().is_empty(),
        "a rolled-back rebuild must not flag any scan unfinished"
    );
}

#[test]
fn a_successful_rebuild_empties_the_v2_tables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = common::open_temp_cache(&dir);
    let seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    cache
        .connection()
        .execute(
            "INSERT INTO block_fts (unit_id, line, text) VALUES (?1, 1, 'x y z')",
            [seeded.unit_ids[0]],
        )
        .unwrap();
    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    for table in ["headings", "symbols", "block_fts"] {
        assert_eq!(cache.table_row_count(table).unwrap(), 0, "{table}");
    }
}

#[test]
fn injected_failure_during_the_v1_to_v2_rebuild_on_open_leaves_the_v1_file_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    common::write_v1_cache(&db_path);

    let err = Cache::open_with_fault_injector(
        &db_path,
        &FaultInjector {
            fail_after_drop: true,
        },
    )
    .map(|_| ())
    .expect_err("the injected fault must surface");
    assert!(
        matches!(err, tephra_cache::CacheError::FaultInjected),
        "{err:?}"
    );

    // Raw look at the file: still v1, every v1 table and row present,
    // no v2 table created.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    for (table, rows) in [
        ("units", 3),
        ("links", 1),
        ("tags", 1),
        ("blocks", 1),
        ("sources", 2),
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, rows, "{table}");
    }
    for v2_only in ["headings", "symbols", "block_fts"] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [v2_only],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{v2_only} must not exist after a rolled-back rebuild");
    }
    drop(conn);

    // And a normal open afterwards still upgrades it.
    let cache = Cache::open(&db_path).expect("open after the failed attempt");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
}
