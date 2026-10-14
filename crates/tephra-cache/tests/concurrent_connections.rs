//! Two connections opened against the same `cache.db` under WAL mode
//! can read while one writes, without `SQLITE_BUSY` on a short-timeout
//! write (G5).

mod common;

use std::time::Duration;

use tephra_cache::{Cache, NewUnit, SourceStatus};

#[test]
fn a_reader_can_read_while_a_writer_holds_a_transaction_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    // Writer connection: opens, registers a source, then holds a write
    // transaction open across the reader's query.
    let writer = Cache::open(&db_path).expect("writer open");
    let source_id = writer
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .expect("register_source");
    writer
        .insert_unit(&NewUnit {
            source_id,
            rel_path: "committed.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert committed unit");

    // Reader: a second, independent connection to the same file,
    // opened *before* the writer starts the in-flight transaction
    // below. `Cache::open` runs its own schema-check statements, which
    // must never race against another connection's open write
    // transaction — opening it here, while every connection is still
    // in autocommit mode, avoids that entirely.
    let reader = Cache::open(&db_path).expect("reader open");

    let writer_conn = writer.connection();
    writer_conn
        .execute("BEGIN IMMEDIATE", [])
        .expect("begin writer tx");
    writer_conn
        .execute(
            "INSERT INTO units (source_id, rel_path, mtime, frontmatter_json) VALUES (?1, ?2, NULL, NULL)",
            rusqlite::params![source_id, "in-flight.md"],
        )
        .expect("insert inside open writer tx");

    // Under WAL, the reader must still see the last-committed snapshot
    // (one row, the one committed before BEGIN IMMEDIATE above)
    // without blocking or erroring, even while the writer's
    // transaction is still open.
    let units = reader
        .units_with_status_for_source("notes")
        .expect("reader query");
    assert_eq!(
        units.len(),
        1,
        "reader should see only the committed row, not the writer's uncommitted insert"
    );
    assert_eq!(units[0].unit.rel_path, "committed.md");

    // Commit the writer's transaction, then confirm the reader (a
    // fresh query on the same connection) now sees both rows.
    writer_conn.execute("COMMIT", []).expect("commit writer tx");
    let units_after = reader
        .units_with_status_for_source("notes")
        .expect("reader query after commit");
    assert_eq!(units_after.len(), 2);
}

#[test]
fn a_writer_with_a_short_busy_timeout_does_not_get_sqlite_busy_against_a_concurrent_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");

    let cache = Cache::open(&db_path).expect("open");
    let source_id = cache
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .expect("register_source");

    // Open the second connection (also while every connection is still
    // in autocommit mode) before holding any transaction open.
    let reader = Cache::open(&db_path).expect("reader open");
    let reader_conn = reader.connection();
    reader_conn.execute("BEGIN", []).expect("begin reader tx");
    let _: i64 = reader_conn
        .query_row("SELECT COUNT(*) FROM units", [], |r| r.get(0))
        .expect("read inside reader tx");

    // A write against the first connection, with a short busy_timeout
    // (WAL mode's point: a reader never blocks a writer). This must
    // succeed promptly rather than returning SQLITE_BUSY.
    cache
        .connection()
        .busy_timeout(Duration::from_millis(200))
        .expect("set short busy_timeout");
    let result = cache.insert_unit(&NewUnit {
        source_id,
        rel_path: "written-while-reader-open.md",
        mtime: None,
        frontmatter_json: None,
    });
    assert!(
        result.is_ok(),
        "WAL mode should let a write proceed despite a concurrent open reader: {result:?}"
    );

    reader_conn.execute("COMMIT", []).expect("commit reader tx");
}
