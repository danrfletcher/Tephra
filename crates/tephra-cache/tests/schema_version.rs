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
}
