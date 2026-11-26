//! Removing a remote source (atlas-27 PR-13): `Cache::delete_remote_source` deletes
//! the source's rows from every remote table and no other source's, and
//! `Cache::remote_unsent_work` counts what a removal would throw away. Real
//! SQLite with the real schema in a tempdir.

mod common;

use std::collections::BTreeSet;

use tephra_cache::{Cache, CloneState, UnsentWork, REMOTE_SOURCE_TABLES, REMOTE_TABLES};

/// Tables that have a `source` column and are not the remote's: the link
/// fixes (`pending_fixes`, `promoted_roots`) are the vault's history and
/// stay when a source goes.
const NOT_REMOTE: [&str; 2] = ["pending_fixes", "promoted_roots"];

fn seed_remote(cache: &Cache, name: &str, kind: &str) {
    cache
        .connection()
        .execute_batch(&format!(
            "INSERT INTO sources (name, path, status) VALUES ('{name}', 'remote:{name}', 'active');
             INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('{name}', '{kind}', 'x/y', 1);
             INSERT INTO remote_items (source, path, kind, file_id, revision, size, state) VALUES
                ('{name}', 'a.md', 'prose', 'id-a', 'rev-1', 10, 'cached'),
                ('{name}', 'b.md', 'prose', 'id-b', 'rev-2', 20, 'pending_upload');
             INSERT INTO upload_queue (source, path, base_revision, created_at) VALUES ('{name}', 'b.md', 'rev-2', 5);
             INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('{name}', 'a.md', 0, 1);
             INSERT INTO remote_cursors (source, cursor, index_complete, updated_at) VALUES ('{name}', 'c1', 1, 2);
             INSERT INTO remote_copy_stamps (source, path, hash) VALUES ('{name}', 'a.md', 'h');
             INSERT INTO remote_copy_revisions (source, path, revision) VALUES ('{name}', 'a.md', 'rev-1');
             INSERT INTO remote_clone_state (source, tip, behind, not_pushed) VALUES ('{name}', 't', 1, 2);"
        ))
        .expect("seed a remote source");
}

fn count(cache: &Cache, table: &str, column: &str, name: &str) -> i64 {
    cache
        .connection()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

/// Every table that holds rows of a remote source, found in the schema
/// itself: `remote_sources`, and every table with a `source` column that
/// is not one of the vault's own.
fn tables_found_in_the_schema(cache: &Cache) -> BTreeSet<String> {
    let names: Vec<String> = cache
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names
        .into_iter()
        .filter(|table| {
            if NOT_REMOTE.contains(&table.as_str()) {
                return false;
            }
            table == "remote_sources"
                || cache
                    .connection()
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = 'source'",
                        [table],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
                    > 0
        })
        .collect()
}

#[test]
fn the_list_of_tables_is_every_remote_table_the_schema_has() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let listed: BTreeSet<String> = REMOTE_SOURCE_TABLES
        .iter()
        .map(|(t, _)| t.to_string())
        .collect();
    assert_eq!(
        tables_found_in_the_schema(&cache),
        listed,
        "a remote table with no entry in REMOTE_SOURCE_TABLES would keep a removed source's rows"
    );
    for table in REMOTE_TABLES {
        assert!(listed.contains(table), "{table} is one of the PR-1 tables");
    }
    for (table, column) in REMOTE_SOURCE_TABLES {
        let found: i64 = cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                [table, column],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "{table} has the column {column}");
    }
}

#[test]
fn delete_remote_source_removes_every_remote_table_row_of_that_source_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed_remote(&cache, "gone", "drive");
    seed_remote(&cache, "kept", "drive");
    // The link fixes are history and are not the source's to take.
    cache
        .connection()
        .execute_batch(
            "INSERT INTO fix_log (created_at, kind, status, old_source) VALUES (1, 'rename', 'done', 'gone');
             INSERT INTO pending_fixes (source, file_path, created_at, updated_at) VALUES ('gone', 'n.md', 1, 1);",
        )
        .unwrap();

    for table in tables_found_in_the_schema(&cache) {
        let column = if table == "remote_sources" {
            "name"
        } else {
            "source"
        };
        assert_eq!(
            count(&cache, &table, column, "gone"),
            if table == "remote_items" { 2 } else { 1 },
            "{table} was seeded (a table this test cannot seed fails here)"
        );
    }

    cache.delete_remote_source("gone").unwrap();

    for table in tables_found_in_the_schema(&cache) {
        let column = if table == "remote_sources" {
            "name"
        } else {
            "source"
        };
        assert_eq!(
            count(&cache, &table, column, "gone"),
            0,
            "{table}: rows of the removed source"
        );
        assert!(
            count(&cache, &table, column, "kept") > 0,
            "{table}: rows of the other source stay"
        );
    }
    assert!(cache.get_source("gone").unwrap().is_none());
    assert!(cache.get_source("kept").unwrap().is_some());
    assert_eq!(
        count(&cache, "fix_log", "old_source", "gone"),
        1,
        "the fix log history stays"
    );
    assert_eq!(count(&cache, "pending_fixes", "source", "gone"), 1);
}

#[test]
fn delete_remote_source_is_idempotent_and_removes_rows_a_crash_left_without_a_source_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache.delete_remote_source("never-was").unwrap();
    seed_remote(&cache, "g", "github");
    // The core row is gone but the remote rows are not (an older build, a
    // crash): the next delete finishes the job.
    cache
        .connection()
        .execute("DELETE FROM sources WHERE name = 'g'", [])
        .unwrap();
    cache.delete_remote_source("g").unwrap();
    cache.delete_remote_source("g").unwrap();
    for (table, column) in REMOTE_SOURCE_TABLES {
        assert_eq!(count(&cache, table, column, "g"), 0, "{table}");
    }
}

#[test]
fn delete_remote_source_is_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed_remote(&cache, "g", "drive");
    // A table the delete reaches last cannot be written: nothing before it
    // stays deleted.
    cache
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_clone_state_delete BEFORE DELETE ON remote_clone_state \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    assert!(cache.delete_remote_source("g").is_err());
    for (table, column) in REMOTE_SOURCE_TABLES {
        assert!(
            count(&cache, table, column, "g") > 0,
            "{table} keeps its rows"
        );
    }
    assert!(
        cache.get_source("g").unwrap().is_some(),
        "the source row stays"
    );

    cache
        .connection()
        .execute("DROP TRIGGER refuse_clone_state_delete", [])
        .unwrap();
    cache.delete_remote_source("g").unwrap();
    for (table, column) in REMOTE_SOURCE_TABLES {
        assert_eq!(
            count(&cache, table, column, "g"),
            0,
            "{table} after the retry"
        );
    }
}

#[test]
fn delete_remote_source_joins_a_transaction_the_caller_has_open() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed_remote(&cache, "g", "github");
    let tx = cache.connection().unchecked_transaction().unwrap();
    cache.delete_remote_source("g").unwrap();
    drop(tx); // rolled back
    assert!(cache.get_source("g").unwrap().is_some());
    assert_eq!(count(&cache, "remote_items", "source", "g"), 2);
    cache.abandon_remote_source("g").unwrap();
    assert_eq!(count(&cache, "remote_items", "source", "g"), 0);
}

// ---- the unsent work a removal would throw away ------------------------------------

fn drive(cache: &Cache) {
    cache
        .connection()
        .execute_batch(
            "INSERT INTO sources (name, path, status) VALUES ('d', 'remote:d', 'active');
             INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('d', 'drive', 'f', 1);",
        )
        .unwrap();
}

fn item(cache: &Cache, path: &str, state: &str) {
    cache
        .connection()
        .execute(
            "INSERT INTO remote_items (source, path, kind, file_id, state) VALUES ('d', ?1, 'prose', ?1, ?2)",
            [path, state],
        )
        .unwrap();
}

fn unsent(cache: &Cache, source: &str) -> UnsentWork {
    cache.remote_unsent_work(source).unwrap()
}

#[test]
fn a_drive_source_with_only_cached_pinned_and_remote_files_has_nothing_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    drive(&cache);
    for (i, state) in ["remote", "downloading", "cached", "pinned", "missing"]
        .iter()
        .enumerate()
    {
        item(&cache, &format!("f{i}.md"), state);
    }
    assert!(unsent(&cache, "d").is_empty());
}

#[test]
fn a_drive_source_counts_pending_uploads_queued_uploads_and_held_files() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    drive(&cache);
    item(&cache, "p1.md", "pending_upload");
    item(&cache, "p2.md", "pending_upload");
    item(&cache, "held.md", "needs_attention");
    cache
        .connection()
        .execute_batch(
            "INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'p1.md', 1);
             INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'p2.md', 1);
             INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'held.md', 1);",
        )
        .unwrap();
    assert_eq!(
        unsent(&cache, "d"),
        UnsentWork {
            not_pushed: 0,
            pending_upload: 2,
            needs_attention: 1
        },
        "a queue row of a pending or held file is not counted twice"
    );
}

#[test]
fn a_queued_upload_of_a_file_that_is_not_marked_pending_still_counts() {
    // A re-created evicted file, or an upload that landed its state change
    // before the queue row went: the row is an edit to send.
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    drive(&cache);
    item(&cache, "evicted.md", "remote");
    item(&cache, "cached.md", "cached");
    cache
        .connection()
        .execute_batch(
            "INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'evicted.md', 1);
             INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'cached.md', 1);
             INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'no-item.md', 1);",
        )
        .unwrap();
    assert_eq!(unsent(&cache, "d").pending_upload, 3);
    assert_eq!(unsent(&cache, "d").needs_attention, 0);
}

#[test]
fn a_github_source_has_the_clone_count_the_source_list_shows_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .connection()
        .execute_batch(
            "INSERT INTO sources (name, path, status) VALUES ('g', 'remote:g', 'active');
             INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('g', 'github', 'o/r', 1);
             INSERT INTO remote_items (source, path, kind, state) VALUES ('g', 'a.md', 'prose', 'not_pushed');",
        )
        .unwrap();
    // No clone state yet: nothing the list would show.
    assert!(unsent(&cache, "g").is_empty());
    cache
        .set_remote_clone_state(
            "g",
            &CloneState {
                tip: Some("t".into()),
                behind: 3,
                not_pushed: 0,
            },
        )
        .unwrap();
    assert!(
        unsent(&cache, "g").is_empty(),
        "a clone only behind the remote holds nothing unsent"
    );
    cache
        .set_remote_clone_state(
            "g",
            &CloneState {
                tip: Some("t".into()),
                behind: 0,
                not_pushed: 2,
            },
        )
        .unwrap();
    assert_eq!(
        unsent(&cache, "g"),
        UnsentWork {
            not_pushed: 2,
            pending_upload: 0,
            needs_attention: 0
        }
    );
}

#[test]
fn a_name_that_is_no_remote_source_has_nothing_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    assert!(unsent(&cache, "nope").is_empty());
    common::seed_source_with_units(
        &cache,
        "local",
        "/tmp/local",
        tephra_cache::SourceStatus::Active,
    );
    assert!(unsent(&cache, "local").is_empty());
}
