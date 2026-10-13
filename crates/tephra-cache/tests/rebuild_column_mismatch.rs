//! Reproduces the R1 review finding: a schema-version mismatch whose
//! *old* core tables have a different column shape than the current
//! schema expects (e.g. a `units` table from before `frontmatter_json`
//! existed) must never brick `Cache::open` — the rebuild must always
//! complete, with or without an offline source present.

mod common;

use rusqlite::Connection;

use tephra_cache::{Cache, CURRENT_SCHEMA_VERSION};

/// Hand-builds a cache file whose `schema_version` is one behind
/// [`CURRENT_SCHEMA_VERSION`] and whose `units` table is missing the
/// `frontmatter_json` column the current schema's offline-row
/// snapshot query selects — the exact shape mismatch the reviewer's
/// scratch probe reproduced. `sources` matches the current (bootstrap)
/// shape, since bootstrap tables are never rebuilt. If `offline`,
/// the one seeded source row is marked `offline` and given a
/// (old-shape) cached unit row to prove that row doesn't need to
/// survive for the rebuild to succeed.
fn seed_old_shape_db(db_path: &std::path::Path, offline: bool) {
    let conn = Connection::open(db_path).expect("open raw connection");

    conn.execute(
        "CREATE TABLE sources (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            name TEXT NOT NULL UNIQUE, \
            path TEXT NOT NULL, \
            status TEXT NOT NULL CHECK (status IN ('active', 'stale', 'offline')), \
            last_scanned_at INTEGER \
        )",
        [],
    )
    .unwrap();
    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [CURRENT_SCHEMA_VERSION - 1],
    )
    .unwrap();

    // Old-shape `units`: no `frontmatter_json` column at all.
    conn.execute(
        "CREATE TABLE units (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            source_id INTEGER NOT NULL, \
            rel_path TEXT NOT NULL, \
            mtime INTEGER \
        )",
        [],
    )
    .unwrap();
    // links/tags/blocks: old-shape too (schema.rs's current shape has
    // no "created_at" column; this stands in for *some* prior release
    // having added an extra column since, which is just as fatal to a
    // naive SELECT of the current column list).
    conn.execute(
        "CREATE TABLE links (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            from_unit_id INTEGER NOT NULL, \
            to_raw TEXT NOT NULL, \
            to_unit_id INTEGER, \
            kind TEXT NOT NULL, \
            created_at INTEGER \
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE tags (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            unit_id INTEGER NOT NULL, \
            tag TEXT NOT NULL \
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE blocks (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            unit_id INTEGER NOT NULL, \
            block_id TEXT NOT NULL, \
            line INTEGER NOT NULL \
        )",
        [],
    )
    .unwrap();

    let status = if offline { "offline" } else { "active" };
    conn.execute(
        "INSERT INTO sources (name, path, status, last_scanned_at) VALUES ('notes', '/vault/notes', ?1, NULL)",
        [status],
    )
    .unwrap();

    if offline {
        conn.execute(
            "INSERT INTO units (id, source_id, rel_path, mtime) VALUES (1, 1, 'one.md', 1700000000)",
            [],
        )
        .unwrap();
    }
}

#[test]
fn open_completes_against_old_shape_schema_with_no_offline_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    seed_old_shape_db(&db_path, false);

    // Before the R1 fix, this failed to prepare the offline-row
    // snapshot SELECT (missing `frontmatter_json`) even though nothing
    // is offline, and `Cache::open` returned Err.
    let cache = Cache::open(&db_path).expect("open must complete despite the old table shape");

    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.source_row_count().unwrap(), 1);
    let source = cache.get_source("notes").unwrap().expect("source row");
    assert_eq!(source.path, "/vault/notes");
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
}

#[test]
fn open_completes_against_old_shape_schema_with_an_offline_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    seed_old_shape_db(&db_path, true);

    // The harder case: a currently-offline source with a cached
    // (old-shape) unit row. The rebuild must still complete; this
    // degraded path preserves the source's row and `offline` status
    // (sources is never touched by a rebuild) but — since the old
    // `units` table can't be safely snapshotted — does not carry the
    // old-shape cached unit row across.
    let cache = Cache::open(&db_path)
        .expect("open must complete even for an offline source on an old-shape schema");

    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.source_row_count().unwrap(), 1);
    let source = cache.get_source("notes").unwrap().expect("source row");
    assert_eq!(
        source.status,
        tephra_cache::SourceStatus::Offline,
        "the source's offline status must survive the rebuild even in the degraded path"
    );
    // Reopening a second time must also succeed (and be a no-op): the
    // bug this test guards against failed identically on every retry
    // since schema_version was never written on the failing path.
    drop(cache);
    let cache = Cache::open(&db_path).expect("second open must also complete");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
}

/// Hand-builds a cache file whose core tables' *column lists* already
/// match the current schema exactly (so
/// `offline_rows_are_carryable` returns `true` and a snapshot is
/// attempted) but whose *data* violates a constraint only the current
/// schema enforces — the R4 review finding: a column-name check alone
/// doesn't catch a NULL in a column that is now `NOT NULL`, or
/// duplicate rows that would violate a new `UNIQUE`. `schema_version`
/// is seeded one behind `CURRENT_SCHEMA_VERSION` so `Cache::open`
/// takes the rebuild path. The single seeded source is `offline` and
/// owns the incompatible row(s), exactly like the reviewer's repro.
enum DataIncompatibility {
    /// Old `blocks` has every current column name, including `line`,
    /// but `line` is nullable there (it is `NOT NULL` in the current
    /// schema) and the seeded row's `line` is `NULL`.
    NullBlockLine,
    /// Old `units` has every current column name, including the
    /// paired `(source_id, rel_path)`, but has no `UNIQUE` constraint
    /// on that pair (the current schema does), and two seeded rows
    /// share the same `(source_id, rel_path)`.
    DuplicateUnitPath,
}

fn seed_data_incompatible_db(db_path: &std::path::Path, variant: DataIncompatibility) {
    let conn = Connection::open(db_path).expect("open raw connection");

    conn.execute(
        "CREATE TABLE sources (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            name TEXT NOT NULL UNIQUE, \
            path TEXT NOT NULL, \
            status TEXT NOT NULL CHECK (status IN ('active', 'stale', 'offline')), \
            last_scanned_at INTEGER \
        )",
        [],
    )
    .unwrap();
    conn.execute("CREATE TABLE schema_version (version INTEGER NOT NULL)", [])
        .unwrap();
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [CURRENT_SCHEMA_VERSION - 1],
    )
    .unwrap();

    let units_ddl = match variant {
        // Same column list as the current schema, but no UNIQUE.
        DataIncompatibility::DuplicateUnitPath => {
            "CREATE TABLE units (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                source_id INTEGER NOT NULL, \
                rel_path TEXT NOT NULL, \
                mtime INTEGER, \
                frontmatter_json TEXT \
            )"
        }
        DataIncompatibility::NullBlockLine => {
            "CREATE TABLE units (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                source_id INTEGER NOT NULL, \
                rel_path TEXT NOT NULL, \
                mtime INTEGER, \
                frontmatter_json TEXT, \
                UNIQUE(source_id, rel_path) \
            )"
        }
    };
    conn.execute(units_ddl, []).unwrap();
    conn.execute(
        "CREATE TABLE links (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            from_unit_id INTEGER NOT NULL, \
            to_raw TEXT NOT NULL, \
            to_unit_id INTEGER, \
            kind TEXT NOT NULL \
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE tags (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            unit_id INTEGER NOT NULL, \
            tag TEXT NOT NULL \
        )",
        [],
    )
    .unwrap();
    let blocks_ddl = match variant {
        // Same column list as the current schema, but `line` is
        // nullable instead of `NOT NULL`.
        DataIncompatibility::NullBlockLine => {
            "CREATE TABLE blocks (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                unit_id INTEGER NOT NULL, \
                block_id TEXT NOT NULL, \
                line INTEGER \
            )"
        }
        DataIncompatibility::DuplicateUnitPath => {
            "CREATE TABLE blocks (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                unit_id INTEGER NOT NULL, \
                block_id TEXT NOT NULL, \
                line INTEGER NOT NULL \
            )"
        }
    };
    conn.execute(blocks_ddl, []).unwrap();

    conn.execute(
        "INSERT INTO sources (name, path, status, last_scanned_at) VALUES ('notes', '/vault/notes', 'offline', NULL)",
        [],
    )
    .unwrap();

    match variant {
        DataIncompatibility::NullBlockLine => {
            conn.execute(
                "INSERT INTO units (id, source_id, rel_path, mtime, frontmatter_json) \
                 VALUES (1, 1, 'one.md', 1700000000, NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO blocks (id, unit_id, block_id, line) VALUES (1, 1, 'abcd1234', NULL)",
                [],
            )
            .unwrap();
        }
        DataIncompatibility::DuplicateUnitPath => {
            conn.execute(
                "INSERT INTO units (id, source_id, rel_path, mtime, frontmatter_json) \
                 VALUES (1, 1, 'one.md', 1700000000, NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO units (id, source_id, rel_path, mtime, frontmatter_json) \
                 VALUES (2, 1, 'one.md', 1700000001, NULL)",
                [],
            )
            .unwrap();
        }
    }
}

#[test]
fn open_completes_when_offline_block_row_has_a_null_now_not_null_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    seed_data_incompatible_db(&db_path, DataIncompatibility::NullBlockLine);

    // Before the R4 fix, `snapshot_offline_rows` failed on
    // `row.get::<i64>` for the NULL `line` value and `Cache::open`
    // returned `Err`, leaving `schema_version` at the old value so
    // every subsequent open failed identically.
    let cache = Cache::open(&db_path)
        .expect("open must complete even when offline data violates a new NOT NULL column");

    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.source_row_count().unwrap(), 1);
    let source = cache.get_source("notes").unwrap().expect("source row");
    assert_eq!(
        source.status,
        tephra_cache::SourceStatus::Offline,
        "the source's offline status must survive even in this degraded path"
    );
    // The snapshot itself failed, so the carry-over is skipped
    // entirely: no units/blocks rows survive for this source.
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 0);

    // Reopening must also succeed and be a no-op.
    drop(cache);
    let cache = Cache::open(&db_path).expect("second open must also complete");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
}

#[test]
fn open_completes_when_offline_units_violate_a_new_unique_constraint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    seed_data_incompatible_db(&db_path, DataIncompatibility::DuplicateUnitPath);

    // Before the R4 fix, the snapshot succeeded (both old rows read
    // back fine), but `restore_offline_rows` failed inserting the
    // second row into the new, now-UNIQUE-constrained `units` table,
    // and `Cache::open` returned `Err` with `schema_version` left at
    // the old value.
    let cache = Cache::open(&db_path).expect(
        "open must complete even when offline data violates a new UNIQUE constraint on restore",
    );

    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(cache.source_row_count().unwrap(), 1);
    let source = cache.get_source("notes").unwrap().expect("source row");
    assert_eq!(
        source.status,
        tephra_cache::SourceStatus::Offline,
        "the source's offline status must survive even in this degraded path"
    );
    // The restore failed partway through (on the second row) and was
    // rolled back via SAVEPOINT, so neither row survives — not just
    // the second one that collided.
    assert_eq!(
        cache.table_row_count("units").unwrap(),
        0,
        "a failed restore must be rolled back in full, not left partially applied"
    );

    // Reopening must also succeed and be a no-op.
    drop(cache);
    let cache = Cache::open(&db_path).expect("second open must also complete");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
}

#[test]
fn reopen_after_a_clean_mismatch_rebuild_still_carries_offline_rows() {
    // Control case: when the old and current table shapes *do* match,
    // offline-row carry-over must still work exactly as before this
    // fix (no regression from the new compatibility gate).
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    {
        let cache = Cache::open(&db_path).expect("first open");
        let seeded = common::seed_source_with_units(
            &cache,
            "notes",
            "/vault/notes",
            tephra_cache::SourceStatus::Active,
        );
        cache
            .transition_source_status("notes", tephra_cache::SourceStatus::Offline)
            .expect("active -> offline");
        let _ = seeded;
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
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    // Column shapes matched, so the offline source's cached rows
    // should have been carried across exactly as offline_preservation
    // tests already verify.
    assert_eq!(cache.table_row_count("units").unwrap(), 2);
    assert_eq!(cache.table_row_count("links").unwrap(), 2);
    assert_eq!(cache.table_row_count("tags").unwrap(), 2);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 2);
}
