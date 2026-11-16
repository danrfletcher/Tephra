//! `fix_log`, `pending_fixes` and `promoted_roots` are durable: created
//! like `sources`, never dropped by a schema rebuild, and free of any
//! foreign key into `sources` or `units` (which a rebuild drops and
//! recreates). Real SQLite in a tempdir.

mod common;

use rusqlite::Connection;
use tephra_cache::{Cache, CacheError, FaultInjector, SourceStatus, CURRENT_SCHEMA_VERSION};

fn dump_all(conn: &Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    common::DURABLE_TABLES
        .iter()
        .map(|t| common::dump_table(conn, t))
        .collect()
}

fn seeded(dir: &tempfile::TempDir) -> Cache {
    let cache = common::open_temp_cache(dir);
    common::seed_durable_rows(cache.connection());
    for table in common::DURABLE_TABLES {
        assert_eq!(cache.table_row_count(table).unwrap(), 2, "{table}");
    }
    cache
}

#[test]
fn rows_in_all_three_tables_survive_cache_rebuild_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = seeded(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let before = dump_all(cache.connection());

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    assert_eq!(
        cache.table_row_count("units").unwrap(),
        0,
        "core tables were rebuilt"
    );
    assert_eq!(dump_all(cache.connection()), before);
}

#[test]
fn rows_survive_a_version_mismatch_open() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let before;
    {
        let cache = Cache::open(&db_path).unwrap();
        common::seed_durable_rows(cache.connection());
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
        before = dump_all(cache.connection());
        cache
            .connection()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [CURRENT_SCHEMA_VERSION - 1],
            )
            .unwrap();
    }

    let cache = Cache::open(&db_path).unwrap();
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(CURRENT_SCHEMA_VERSION - 1)
    );
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(dump_all(cache.connection()), before);
}

#[test]
fn rows_survive_a_rebuild_forced_by_an_older_version_number_on_a_real_v2_file() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    common::write_v2_cache(&db_path);
    // Tables a v2 file does not have: created by the open, then filled.
    {
        let cache = Cache::open(&db_path).unwrap();
        common::seed_durable_rows(cache.connection());
    }
    let before = dump_all(&Connection::open(&db_path).unwrap());
    // Back to v2: the next open rebuilds again.
    Connection::open(&db_path)
        .unwrap()
        .execute("UPDATE schema_version SET version = 2", [])
        .unwrap();
    let cache = Cache::open(&db_path).unwrap();
    assert_eq!(cache.rebuilt_from_version(), Some(2));
    assert_eq!(dump_all(cache.connection()), before);
}

#[test]
fn no_foreign_key_points_into_or_out_of_the_durable_tables() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();

    for table in common::DURABLE_TABLES {
        let out: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_foreign_key_list(?1)",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(out, 0, "{table} references no table");
    }
    // And no other table references one of them: every foreign key in the
    // schema targets only the tables a rebuild leaves alone or recreates
    // together (`sources`, `units`, `symbols`).
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for table in tables {
        let mut stmt = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")
            .unwrap();
        let targets: Vec<String> = stmt
            .query_map([&table], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for target in targets {
            assert!(
                !common::DURABLE_TABLES.contains(&target.as_str()),
                "{table} has a foreign key into {target}"
            );
        }
    }
}

#[test]
fn a_rebuild_with_rows_in_the_tables_and_foreign_keys_on_does_not_fail() {
    // The connection runs with `foreign_keys = ON`; dropping `units` must
    // not trip over a durable row that mentions a unit's source or path.
    let dir = tempfile::tempdir().unwrap();
    let mut cache = seeded(&dir);
    assert!(cache.foreign_keys_enabled().unwrap());
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    // Also the pending-fixes row's `fix_id` names a fix_log row by number.
    cache
        .rebuild(&FaultInjector::none())
        .expect("rebuild with durable rows");
    let violations: i64 = cache
        .connection()
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn unregistering_a_source_leaves_its_history_behind() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_durable_rows(cache.connection());
    let before = dump_all(cache.connection());

    cache.delete_source("notes").unwrap();

    assert!(cache.get_source("notes").unwrap().is_none());
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(
        dump_all(cache.connection()),
        before,
        "rows naming `notes` are still there"
    );
    // A source of the same name registered again meets its history.
    cache
        .register_source("notes", "/elsewhere", SourceStatus::Active)
        .unwrap();
    assert_eq!(cache.table_row_count("fix_log").unwrap(), 2);
}

#[test]
fn an_injected_failure_mid_rebuild_rolls_back_with_the_new_tables_and_columns_intact() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let mut cache = Cache::open(&db_path).unwrap();
    let seeded = common::seed_source_with_units(&cache, "notes", "/n", SourceStatus::Active);
    common::seed_durable_rows(cache.connection());
    cache
        .connection()
        .execute(
            "UPDATE units SET hash = 'abc', inode = -1, device = 9 WHERE id = ?1",
            [seeded.unit_ids[0]],
        )
        .unwrap();
    let units_before = common::dump_table(cache.connection(), "units");
    let durable_before = dump_all(cache.connection());

    let err = cache
        .rebuild(&FaultInjector {
            fail_after_drop: true,
        })
        .unwrap_err();
    assert!(matches!(err, CacheError::FaultInjected));
    // Same connection: the rollback restored `units` with its v3 columns.
    assert_eq!(
        common::dump_table(cache.connection(), "units"),
        units_before
    );
    assert_eq!(dump_all(cache.connection()), durable_before);
    drop(cache);

    // And a fresh open of the file agrees.
    let cache = Cache::open(&db_path).unwrap();
    assert_eq!(cache.rebuilt_from_version(), None);
    assert_eq!(
        common::dump_table(cache.connection(), "units"),
        units_before
    );
    assert_eq!(dump_all(cache.connection()), durable_before);
    let row = cache.get_unit(seeded.unit_ids[0]).unwrap().unwrap();
    assert_eq!(row.hash.as_deref(), Some("abc"));
    assert_eq!((row.device, row.inode), (Some(9), Some(u64::MAX)));
}

#[test]
fn a_failed_rebuild_on_open_of_a_v2_file_leaves_it_v2_and_the_open_after_it_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    common::write_v2_cache(&db_path);

    let err = Cache::open_with_fault_injector(
        &db_path,
        &FaultInjector {
            fail_after_drop: true,
        },
    )
    .map(|_| ())
    .unwrap_err();
    assert!(matches!(err, CacheError::FaultInjected));

    let conn = Connection::open(&db_path).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2, "rolled back to v2");
    let units: i64 = conn
        .query_row("SELECT count(*) FROM units", [], |r| r.get(0))
        .unwrap();
    assert_eq!(units, 3, "the v2 rows are all still there");
    let has_hash: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('units') WHERE name = 'hash'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_hash, 0, "units is still the v2 table");
    drop(conn);

    let cache = Cache::open(&db_path).unwrap();
    assert_eq!(cache.schema_version().unwrap(), Some(3));
}
