//! The five remote tables (`remote_sources`, `remote_items`,
//! `upload_queue`, `remote_pins`, `remote_cursors`) are durable like
//! `fix_log`: created with the bootstrap tables, never dropped by a
//! rebuild, free of foreign keys, and constrained to the closed kind and
//! state sets. Real SQLite in a tempdir; nothing here touches the network.

mod common;

use rusqlite::{params, Connection};
use tephra_cache::{
    Cache, FaultInjector, NewFixLog, NewPromote, NewSourceRename, SourceStatus,
    CURRENT_SCHEMA_VERSION, REMOTE_ITEM_STATES, REMOTE_SOURCE_KINDS, REMOTE_TABLES,
};

type Dump = Vec<Vec<Vec<rusqlite::types::Value>>>;

fn dump_remote(conn: &Connection) -> Dump {
    REMOTE_TABLES
        .iter()
        .map(|t| common::dump_table(conn, t))
        .collect()
}

/// Every ordinary table of the file, by name, with its rows.
fn dump_everything(conn: &Connection) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names
        .into_iter()
        .map(|n| {
            let rows = common::dump_table(conn, &n);
            (n, rows)
        })
        .collect()
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |r| r.get(0),
    )
    .unwrap()
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .unwrap()
        .query_map([table], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn schema_version(conn: &Connection) -> i64 {
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap()
}

/// A seeded cache file with a registered local source beside the rows.
fn seeded_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("cache.db");
    let cache = Cache::open(&path).unwrap();
    common::seed_durable_rows(cache.connection());
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    path
}

// ---- created on open ------------------------------------------------

#[test]
fn a_new_cache_has_all_five_tables_empty_at_the_current_version() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    for table in REMOTE_TABLES {
        assert!(table_exists(conn, table), "{table} exists");
        assert_eq!(cache.table_row_count(table).unwrap(), 0, "{table} is empty");
    }
    assert_eq!(schema_version(conn), CURRENT_SCHEMA_VERSION);
    assert_eq!(cache.rebuilt_from_version(), None);
}

#[test]
fn an_existing_v3_cache_without_the_tables_gets_them_on_open_and_keeps_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_file(&dir);
    // The cache as an earlier v3 build left it: no remote tables.
    {
        let conn = Connection::open(&path).unwrap();
        for table in REMOTE_TABLES {
            conn.execute(&format!("DROP TABLE {table}"), []).unwrap();
        }
        assert!(!table_exists(&conn, "remote_items"));
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
    }
    let local_before = {
        let conn = Connection::open(&path).unwrap();
        (
            common::dump_table(&conn, "units"),
            common::dump_table(&conn, "sources"),
            common::dump_table(&conn, "fix_log"),
        )
    };

    let cache = Cache::open(&path).unwrap();
    assert_eq!(cache.rebuilt_from_version(), None, "not a rebuild");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    for table in REMOTE_TABLES {
        assert!(table_exists(cache.connection(), table), "{table}");
        assert_eq!(cache.table_row_count(table).unwrap(), 0);
    }
    let conn = cache.connection();
    assert_eq!(
        (
            common::dump_table(conn, "units"),
            common::dump_table(conn, "sources"),
            common::dump_table(conn, "fix_log"),
        ),
        local_before,
        "the open touched no local row"
    );
}

#[test]
fn a_second_open_of_an_unchanged_cache_writes_no_row_and_keeps_the_version() {
    let dir = tempfile::tempdir().unwrap();
    // No local units: a cache whose files lack `^^` index rows flags its
    // sources for a rescan on every open, which is not what is measured.
    let path = dir.path().join("cache.db");
    {
        let cache = Cache::open(&path).unwrap();
        common::seed_durable_rows(cache.connection());
        cache
            .register_source("notes", "/vault/notes", SourceStatus::Active)
            .unwrap();
    }
    let before = dump_everything(&Connection::open(&path).unwrap());

    let cache = Cache::open(&path).unwrap();
    assert_eq!(cache.rebuilt_from_version(), None);
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(CURRENT_SCHEMA_VERSION)
    );
    drop(cache);

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        dump_everything(&conn),
        before,
        "no table gained, lost or changed a row"
    );
    assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
}

// ---- shape ------------------------------------------------------------

#[test]
fn the_tables_have_the_columns_the_spec_names() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    let has = |table: &str, wanted: &[&str]| {
        let have = columns(conn, table);
        for w in wanted {
            assert!(
                have.iter().any(|c| c == w),
                "{table} has no column {w}: {have:?}"
            );
        }
    };
    has("remote_sources", &["name", "kind", "cache", "status"]);
    has(
        "remote_items",
        &[
            "source",
            "path",
            "etag",
            "sha",
            "revision",
            "size",
            "state",
            "last_used",
        ],
    );
    has("upload_queue", &["attempts", "next_retry_at"]);
    has("remote_pins", &["source", "path"]);
    has("remote_cursors", &["source", "cursor"]);
}

#[test]
fn the_cache_flag_defaults_to_one_and_a_new_item_starts_remote() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    conn.execute(
        "INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('gh', 'github', 'o/r', 1)",
        [],
    )
    .unwrap();
    let (flag, status): (i64, String) = conn
        .query_row("SELECT cache, status FROM remote_sources", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(flag, 1, "cache is on by default");
    assert_eq!(status, "indexing");

    conn.execute(
        "INSERT INTO remote_items (source, path) VALUES ('gh', 'a.md')",
        [],
    )
    .unwrap();
    let state: String = conn
        .query_row("SELECT state FROM remote_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "remote");
}

#[test]
fn an_unknown_source_kind_or_item_state_is_rejected_by_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();

    for kind in ["gitlab", "GitHub", "", "dropbox"] {
        let err = conn
            .execute(
                "INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('x', ?1, 't', 1)",
                [kind],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{kind:?}: {err}");
    }
    for state in ["Cached", "evicted", "", "queued"] {
        let err = conn
            .execute(
                "INSERT INTO remote_items (source, path, state) VALUES ('x', 'p', ?1)",
                [state],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{state:?}: {err}");
    }
    // The `cache` flag is a boolean too.
    let err = conn
        .execute(
            "INSERT INTO remote_sources (name, kind, target, cache, created_at) VALUES ('x', 'drive', 't', 2, 1)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    assert_eq!(cache.table_row_count("remote_sources").unwrap(), 0);
    assert_eq!(cache.table_row_count("remote_items").unwrap(), 0);
}

#[test]
fn every_listed_kind_and_state_is_accepted_and_the_lists_match_the_check_text() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    for (i, kind) in REMOTE_SOURCE_KINDS.iter().enumerate() {
        conn.execute(
            "INSERT INTO remote_sources (name, kind, target, created_at) VALUES (?1, ?2, 't', 1)",
            params![format!("s{i}"), kind],
        )
        .unwrap();
    }
    assert_eq!(REMOTE_ITEM_STATES.len(), 8);
    for (i, state) in REMOTE_ITEM_STATES.iter().enumerate() {
        conn.execute(
            "INSERT INTO remote_items (source, path, state) VALUES ('s0', ?1, ?2)",
            params![format!("p{i}"), state],
        )
        .unwrap();
    }

    // The other direction: the CHECK names exactly the listed values.
    let quoted = |table: &str| -> Vec<String> {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        let mut out = Vec::new();
        let mut rest = sql.as_str();
        while let Some(start) = rest.find('\'') {
            let after = &rest[start + 1..];
            let end = after.find('\'').unwrap();
            out.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
        out
    };
    let items = quoted("remote_items");
    for state in REMOTE_ITEM_STATES {
        assert!(items.iter().any(|v| v == state), "{state} in the CHECK");
    }
    // 'remote' is also the column default, so the CHECK list plus that
    // default is nine quoted words at most, all of them known states.
    for word in &items {
        assert!(REMOTE_ITEM_STATES.contains(&word.as_str()), "{word}");
    }
    let sources = quoted("remote_sources");
    for kind in REMOTE_SOURCE_KINDS {
        assert!(sources.iter().any(|v| v == kind), "{kind} in the CHECK");
    }
}

#[test]
fn pins_and_upload_rows_are_unique_per_source_and_path_and_a_cursor_per_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    let unique = |sql: &str, table: &str| {
        conn.execute(sql, []).unwrap();
        let err = conn.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{table}: {err}");
        assert_eq!(cache.table_row_count(table).unwrap(), 1, "{table}");
    };
    unique(
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('gh', 'docs', 1, 1)",
        "remote_pins",
    );
    unique(
        "INSERT INTO upload_queue (source, path, created_at) VALUES ('gh', 'a.md', 1)",
        "upload_queue",
    );
    unique(
        "INSERT INTO remote_cursors (source, cursor) VALUES ('gh', 'c1')",
        "remote_cursors",
    );
    unique(
        "INSERT INTO remote_items (source, path) VALUES ('gh', 'a.md')",
        "remote_items",
    );
    unique(
        "INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('gh', 'github', 'o/r', 1)",
        "remote_sources",
    );
    // The same path in another source is a different row.
    conn.execute(
        "INSERT INTO remote_pins (source, path, created_at) VALUES ('gdrive', 'docs', 1)",
        [],
    )
    .unwrap();
    assert_eq!(cache.table_row_count("remote_pins").unwrap(), 2);
    // The queue's retry columns count from zero.
    let (attempts, next): (i64, Option<i64>) = conn
        .query_row(
            "SELECT attempts, next_retry_at FROM upload_queue",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((attempts, next), (0, None));
}

// ---- durability ---------------------------------------------------------

#[test]
fn rows_in_all_five_tables_survive_cache_rebuild_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_file(&dir);
    let mut cache = Cache::open(&path).unwrap();
    for table in REMOTE_TABLES {
        assert_eq!(cache.table_row_count(table).unwrap(), 2, "{table} seeded");
    }
    let before = dump_remote(cache.connection());

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    assert_eq!(
        cache.table_row_count("units").unwrap(),
        0,
        "core tables were rebuilt"
    );
    assert_eq!(dump_remote(cache.connection()), before);
}

#[test]
fn rows_survive_a_version_mismatch_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_file(&dir);
    let before = dump_remote(&Connection::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION - 1],
        )
        .unwrap();

    let cache = Cache::open(&path).unwrap();
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(CURRENT_SCHEMA_VERSION - 1)
    );
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(dump_remote(cache.connection()), before);
}

/// Opens the old-format file at `db_path` (which creates the remote
/// tables and rebuilds), fills them, sets the version back to
/// `old_version` and opens again, so the second open is a rebuild with
/// rows in the tables.
fn rows_survive_the_upgrade_of(db_path: &std::path::Path, old_version: i64) {
    {
        let cache = Cache::open(db_path).unwrap();
        assert_eq!(cache.rebuilt_from_version(), Some(old_version));
        common::seed_durable_rows(cache.connection());
    }
    let before = dump_remote(&Connection::open(db_path).unwrap());
    assert!(before.iter().all(|t| t.len() == 2));
    Connection::open(db_path)
        .unwrap()
        .execute("UPDATE schema_version SET version = ?1", [old_version])
        .unwrap();

    let cache = Cache::open(db_path).unwrap();
    assert_eq!(cache.rebuilt_from_version(), Some(old_version));
    assert_eq!(dump_remote(cache.connection()), before);
}

#[test]
fn rows_survive_the_v1_fixture_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    common::write_v1_cache(&db_path);
    {
        let conn = Connection::open(&db_path).unwrap();
        assert!(!table_exists(&conn, "remote_items"), "a v1 file has none");
    }
    rows_survive_the_upgrade_of(&db_path, 1);
}

#[test]
fn rows_survive_the_v2_fixture_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    common::write_v2_cache(&db_path);
    {
        let conn = Connection::open(&db_path).unwrap();
        assert!(!table_exists(&conn, "remote_items"), "a v2 file has none");
    }
    rows_survive_the_upgrade_of(&db_path, 2);
}

#[test]
fn an_injected_failure_mid_rebuild_leaves_the_remote_rows_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_file(&dir);
    let mut cache = Cache::open(&path).unwrap();
    let before = dump_remote(cache.connection());
    cache
        .rebuild(&FaultInjector {
            fail_after_drop: true,
        })
        .unwrap_err();
    assert_eq!(dump_remote(cache.connection()), before);
}

#[test]
fn the_offline_carry_over_of_a_rebuild_leaves_remote_rows_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let mut cache = Cache::open(&path).unwrap();
    common::seed_source_with_units(&cache, "gone", "/unmounted", SourceStatus::Active);
    cache
        .transition_source_status("gone", SourceStatus::Offline)
        .unwrap();
    // A remote source of the same name as the offline local one.
    common::seed_durable_rows(cache.connection());
    cache
        .connection()
        .execute(
            "INSERT INTO remote_items (source, path) VALUES ('gone', 'x.md')",
            [],
        )
        .unwrap();
    let before = dump_remote(cache.connection());

    cache.rebuild(&FaultInjector::none()).unwrap();

    assert_eq!(
        cache.table_row_count("units").unwrap(),
        2,
        "the offline source's rows were carried across"
    );
    assert_eq!(dump_remote(cache.connection()), before);
}

#[test]
fn unregistering_a_local_source_leaves_remote_rows_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_durable_rows(cache.connection());
    // Remote rows that carry the very name of the local source.
    for sql in [
        "INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('notes', 'drive', 'f', 1)",
        "INSERT INTO remote_items (source, path) VALUES ('notes', 'a.md')",
        "INSERT INTO upload_queue (source, path, created_at) VALUES ('notes', 'a.md', 1)",
        "INSERT INTO remote_pins (source, path, created_at) VALUES ('notes', 'a.md', 1)",
        "INSERT INTO remote_cursors (source, cursor) VALUES ('notes', 'c')",
    ] {
        cache.connection().execute(sql, []).unwrap();
    }
    let before = dump_remote(cache.connection());

    cache.delete_source("notes").unwrap();

    assert!(cache.get_source("notes").unwrap().is_none());
    assert_eq!(dump_remote(cache.connection()), before);
}

fn fix_log() -> NewFixLog {
    NewFixLog {
        created_at: 10,
        kind: "source_rename".into(),
        status: "partial".into(),
        old_source: Some("notes".into()),
        old_path: Some("@notes:".into()),
        new_source: Some("ideas".into()),
        new_path: Some("@ideas:".into()),
        files_changed: 0,
        detail_json: None,
    }
}

#[test]
fn renaming_a_local_source_leaves_remote_rows_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_durable_rows(cache.connection());
    cache
        .connection()
        .execute(
            "INSERT INTO remote_items (source, path) VALUES ('notes', 'a.md')",
            [],
        )
        .unwrap();
    let before = dump_remote(cache.connection());

    let log = fix_log();
    cache
        .begin_source_rename(&NewSourceRename {
            old_name: "notes",
            new_name: "ideas",
            created_at: 10,
            log: &log,
            pending: &[],
        })
        .unwrap();

    assert!(cache.get_source("ideas").unwrap().is_some());
    assert_eq!(
        dump_remote(cache.connection()),
        before,
        "a remote row named `notes` is not renamed with the local source"
    );
}

#[test]
fn promoting_a_local_source_leaves_remote_rows_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_durable_rows(cache.connection());
    let before = dump_remote(cache.connection());

    let log = fix_log();
    cache
        .begin_promote(&NewPromote {
            name: "ideas",
            root: "/vault/notes/Drafts",
            parent: "notes",
            rel_path: "Drafts",
            created_at: 10,
            log: &log,
            pending: &[],
        })
        .unwrap();

    assert!(cache.get_source("ideas").unwrap().is_some());
    assert_eq!(dump_remote(cache.connection()), before);
}

// ---- no foreign keys ----------------------------------------------------

#[test]
fn no_foreign_key_points_into_a_remote_table_or_out_of_one() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();

    for table in REMOTE_TABLES {
        let out: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_foreign_key_list(?1)",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            out, 0,
            "{table} has no foreign key (so none into `sources` or `units`)"
        );
    }

    // And no table of the schema, core or durable, targets a remote table.
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(tables.len() > REMOTE_TABLES.len());
    for table in tables {
        let targets: Vec<String> = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")
            .unwrap()
            .query_map([&table], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for target in targets {
            assert!(
                !REMOTE_TABLES.contains(&target.as_str()),
                "{table} has a foreign key into {target}"
            );
        }
    }
}

#[test]
fn a_rebuild_with_remote_rows_and_foreign_keys_on_finds_no_violation() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_file(&dir);
    let mut cache = Cache::open(&path).unwrap();
    assert!(cache.foreign_keys_enabled().unwrap());
    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    let violations: i64 = cache
        .connection()
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}
