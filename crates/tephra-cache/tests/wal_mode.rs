//! Asserts `journal_mode` and `synchronous` pragmas on a fresh
//! connection; no mocks, a real temp SQLite file (G5).

mod common;

#[test]
fn fresh_connection_is_wal_mode_and_synchronous_normal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let journal_mode = cache.journal_mode().expect("journal_mode");
    assert_eq!(journal_mode.to_lowercase(), "wal");

    // SQLite's PRAGMA synchronous integer encoding: 1 == NORMAL.
    let synchronous = cache.synchronous_mode().expect("synchronous");
    assert_eq!(synchronous, 1, "expected synchronous=NORMAL (1)");
}

#[test]
fn foreign_keys_pragma_is_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    assert!(cache.foreign_keys_enabled().expect("foreign_keys"));
}

#[test]
fn journal_mode_survives_a_fresh_reopen_of_the_same_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    {
        let cache = tephra_cache::Cache::open(&db_path).expect("first open");
        assert_eq!(cache.journal_mode().unwrap().to_lowercase(), "wal");
    }

    let cache = tephra_cache::Cache::open(&db_path).expect("second open");
    assert_eq!(cache.journal_mode().unwrap().to_lowercase(), "wal");
}
