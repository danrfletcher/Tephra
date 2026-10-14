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
