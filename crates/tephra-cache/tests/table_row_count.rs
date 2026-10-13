//! `Cache::table_row_count` rejects any table name outside its closed
//! allow-list, including an attempted SQL-injection payload, rather
//! than running it (R3 review finding).

mod common;

use tephra_cache::CacheError;

#[test]
fn table_row_count_accepts_every_documented_table_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    for table in [
        "sources",
        "units",
        "links",
        "tags",
        "blocks",
        "schema_version",
    ] {
        assert!(
            cache.table_row_count(table).is_ok(),
            "expected {table} to be a recognized table name"
        );
    }
}

#[test]
fn table_row_count_rejects_unknown_table_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.table_row_count("not_a_real_table");
    match result {
        Err(CacheError::InvalidTableName(name)) => assert_eq!(name, "not_a_real_table"),
        other => panic!("expected CacheError::InvalidTableName, got {other:?}"),
    }
}

#[test]
fn table_row_count_rejects_sql_injection_payload_in_release_mode() {
    // The exact payload the reviewer's --release probe demonstrated
    // actually running against the live connection before this fix:
    // it must now be rejected as an unrecognized table name, in both
    // debug and release builds (hence no reliance on debug_assert!).
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let payload = "sources WHERE 1=0 UNION SELECT count(*) FROM schema_version";
    let result = cache.table_row_count(payload);
    match result {
        Err(CacheError::InvalidTableName(name)) => assert_eq!(name, payload),
        other => panic!("expected the injection payload to be rejected, got {other:?}"),
    }
}
