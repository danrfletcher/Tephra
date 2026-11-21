//! The fix log and the pending-fixes work list on the real cache (G9,
//! G10, G18): insert, update in place, order, the `since` filter, the
//! limit clamp, the one-transaction rule, and survival of a schema
//! rebuild. Real SQLite in a tempdir.

mod common;

use tephra_cache::{
    Cache, FaultInjector, FixLogChange, NewFixLog, NewPendingFix, PendingOp, SourceStatus,
    CURRENT_SCHEMA_VERSION, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT, STATE_NEEDS_MANUAL_FIX,
    STATE_PENDING,
};

fn entry(at: i64, old: &str, new: &str) -> NewFixLog {
    NewFixLog {
        created_at: at,
        kind: "rename".into(),
        status: "partial".into(),
        old_source: Some("notes".into()),
        old_path: Some(old.into()),
        new_source: Some("notes".into()),
        new_path: Some(new.into()),
        files_changed: 0,
        detail_json: Some("{}".into()),
    }
}

fn pending(file: &str, old: &str, new: &str) -> NewPendingFix {
    NewPendingFix {
        source: "notes".into(),
        file_path: file.into(),
        old_source: Some("notes".into()),
        old_path: Some(old.into()),
        new_source: Some("notes".into()),
        new_path: Some(new.into()),
    }
}

fn cache() -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    (dir, cache)
}

#[test]
fn an_empty_log_lists_nothing() {
    let (_d, cache) = cache();
    assert!(cache
        .list_fix_log(None, DEFAULT_LIST_LIMIT)
        .unwrap()
        .is_empty());
}

#[test]
fn insert_gives_ids_and_reads_back_every_field() {
    let (_d, cache) = cache();
    let a = cache.insert_fix_log(&entry(100, "A.md", "B.md")).unwrap();
    let b = cache.insert_fix_log(&entry(200, "C.md", "D.md")).unwrap();
    assert_ne!(a, b);
    let row = cache.get_fix_log(a).unwrap().unwrap();
    assert_eq!(row.id, a);
    assert_eq!(row.created_at, 100);
    assert_eq!(row.kind, "rename");
    assert_eq!(row.status, "partial");
    assert_eq!(row.old_path.as_deref(), Some("A.md"));
    assert_eq!(row.new_path.as_deref(), Some("B.md"));
    assert_eq!(row.files_changed, 0);
    assert_eq!(cache.get_fix_log(999).unwrap(), None);
}

#[test]
fn update_changes_the_same_row_and_never_adds_one_or_moves_its_time() {
    let (_d, cache) = cache();
    let id = cache.insert_fix_log(&entry(100, "A.md", "B.md")).unwrap();
    let changed = cache
        .update_fix_log(
            id,
            &FixLogChange {
                status: "applied".into(),
                files_changed: 3,
                detail_json: Some("{\"files\":[]}".into()),
            },
        )
        .unwrap();
    assert!(changed);
    let rows = cache.list_fix_log(None, 50).unwrap();
    assert_eq!(rows.len(), 1, "an update is not a second entry");
    assert_eq!(rows[0].status, "applied");
    assert_eq!(rows[0].files_changed, 3);
    assert_eq!(rows[0].created_at, 100);
    assert_eq!(rows[0].old_path.as_deref(), Some("A.md"));
    assert!(!cache
        .update_fix_log(
            999,
            &FixLogChange {
                status: "applied".into(),
                files_changed: 0,
                detail_json: None
            }
        )
        .unwrap());
}

#[test]
fn list_is_newest_first_and_ties_go_to_the_later_entry() {
    let (_d, cache) = cache();
    let first = cache.insert_fix_log(&entry(100, "A.md", "B.md")).unwrap();
    let second = cache.insert_fix_log(&entry(300, "C.md", "D.md")).unwrap();
    let third = cache.insert_fix_log(&entry(300, "E.md", "F.md")).unwrap();
    let fourth = cache.insert_fix_log(&entry(200, "G.md", "H.md")).unwrap();
    let ids: Vec<i64> = cache
        .list_fix_log(None, 50)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, [third, second, fourth, first]);
}

#[test]
fn since_keeps_entries_at_or_after_that_second() {
    let (_d, cache) = cache();
    for (t, name) in [(100, "a"), (200, "b"), (300, "c")] {
        cache
            .insert_fix_log(&entry(t, &format!("{name}.md"), "z.md"))
            .unwrap();
    }
    let at = |since| -> Vec<i64> {
        cache
            .list_fix_log(Some(since), 50)
            .unwrap()
            .iter()
            .map(|r| r.created_at)
            .collect()
    };
    assert_eq!(at(0), [300, 200, 100]);
    assert_eq!(at(200), [300, 200], "the boundary second is included");
    assert_eq!(at(201), [300]);
    assert!(at(301).is_empty());
}

#[test]
fn limit_is_honoured_and_clamped_to_the_maximum() {
    let (_d, cache) = cache();
    for t in 0..(MAX_LIST_LIMIT as i64 + 20) {
        cache.insert_fix_log(&entry(t, "a.md", "b.md")).unwrap();
    }
    let top = cache.list_fix_log(None, 3).unwrap();
    assert_eq!(top.len(), 3);
    assert_eq!(top[0].created_at, MAX_LIST_LIMIT as i64 + 19);
    assert_eq!(
        cache.list_fix_log(None, 5000).unwrap().len(),
        MAX_LIST_LIMIT,
        "5000 is clamped to the maximum"
    );
    assert_eq!(MAX_LIST_LIMIT, 1000);
    assert_eq!(DEFAULT_LIST_LIMIT, 50);
}

#[test]
fn start_fix_writes_the_entry_and_every_pending_row_together() {
    let (_d, cache) = cache();
    let (fix_id, ids) = cache
        .start_fix(
            &entry(100, "A.md", "B.md"),
            &[
                pending("x.md", "A.md", "B.md"),
                pending("y.md", "A.md", "B.md"),
            ],
        )
        .unwrap();
    assert_eq!(ids.len(), 2);
    let rows = cache.pending_fixes(None, Some(fix_id)).unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), ids);
    assert!(rows
        .iter()
        .all(|r| r.state == STATE_PENDING && r.attempts == 0));
    assert!(rows.iter().all(|r| r.fix_id == Some(fix_id)));
    assert_eq!(rows[0].created_at, 100);
    assert_eq!(rows[0].file_path, "x.md");
    assert_eq!(cache.pending_fixes(Some("elsewhere"), None).unwrap(), []);
}

#[test]
fn a_failed_start_fix_leaves_neither_the_entry_nor_any_row() {
    let (_d, cache) = cache();
    // The second row has a NULL where the table forbids one: its insert
    // fails after the entry and the first row were already inserted.
    cache
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_second BEFORE INSERT ON pending_fixes \
             WHEN NEW.file_path = 'boom.md' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let result = cache.start_fix(
        &entry(100, "A.md", "B.md"),
        &[
            pending("ok.md", "A.md", "B.md"),
            pending("boom.md", "A.md", "B.md"),
        ],
    );
    assert!(result.is_err());
    assert_eq!(cache.table_row_count("fix_log").unwrap(), 0);
    assert_eq!(cache.table_row_count("pending_fixes").unwrap(), 0);
}

#[test]
fn a_step_changes_the_row_and_the_entry_together_or_not_at_all() {
    let (_d, cache) = cache();
    let (fix_id, ids) = cache
        .start_fix(
            &entry(100, "A.md", "B.md"),
            &[
                pending("x.md", "A.md", "B.md"),
                pending("y.md", "A.md", "B.md"),
            ],
        )
        .unwrap();

    let done = cache
        .apply_fix_step(ids[0], &PendingOp::Delete, 150, Some(fix_id), |row| {
            Some(FixLogChange {
                status: row.status.clone(),
                files_changed: row.files_changed + 1,
                detail_json: row.detail_json.clone(),
            })
        })
        .unwrap();
    assert!(done);
    assert_eq!(cache.get_pending_fix(ids[0]).unwrap(), None);
    assert_eq!(cache.get_fix_log(fix_id).unwrap().unwrap().files_changed, 1);

    // The log update fails (a trigger refuses it): the row change rolls back.
    cache
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_log_update BEFORE UPDATE ON fix_log \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let result = cache.apply_fix_step(ids[1], &PendingOp::Delete, 160, Some(fix_id), |row| {
        Some(FixLogChange {
            status: "applied".into(),
            files_changed: row.files_changed + 1,
            detail_json: None,
        })
    });
    assert!(result.is_err());
    assert!(
        cache.get_pending_fix(ids[1]).unwrap().is_some(),
        "the row delete was rolled back with the failed log update"
    );
    assert_eq!(cache.get_fix_log(fix_id).unwrap().unwrap().files_changed, 1);
}

#[test]
fn pending_ops_update_and_repoint_a_row() {
    let (_d, cache) = cache();
    let (fix_id, ids) = cache
        .start_fix(
            &entry(100, "A.md", "B.md"),
            &[pending("x.md", "A.md", "B.md")],
        )
        .unwrap();
    cache
        .apply_fix_step(
            ids[0],
            &PendingOp::Update {
                state: STATE_NEEDS_MANUAL_FIX.into(),
                reason: Some("read-only".into()),
                attempts: 4,
            },
            200,
            None,
            |_| None,
        )
        .unwrap();
    let row = cache.get_pending_fix(ids[0]).unwrap().unwrap();
    assert_eq!(
        (
            row.state.as_str(),
            row.reason.as_deref(),
            row.attempts,
            row.updated_at
        ),
        (STATE_NEEDS_MANUAL_FIX, Some("read-only"), 4, 200)
    );
    assert_eq!(row.created_at, 100);

    cache
        .apply_fix_step(
            ids[0],
            &PendingOp::Repoint {
                new_source: "notes".into(),
                new_path: "C.md".into(),
            },
            300,
            None,
            |_| None,
        )
        .unwrap();
    let row = cache.get_pending_fix(ids[0]).unwrap().unwrap();
    assert_eq!(row.new_path.as_deref(), Some("C.md"));
    assert_eq!(row.old_path.as_deref(), Some("A.md"));
    assert_eq!(row.state, STATE_PENDING);
    assert_eq!(row.reason, None);
    assert_eq!(row.fix_id, Some(fix_id));
    assert!(!cache
        .apply_fix_step(999, &PendingOp::Delete, 1, None, |_| None)
        .unwrap());
}

#[test]
fn renames_from_finds_only_later_renames_of_that_old_path() {
    let (_d, cache) = cache();
    let a = cache.insert_fix_log(&entry(100, "A.md", "B.md")).unwrap();
    let b = cache.insert_fix_log(&entry(200, "B.md", "C.md")).unwrap();
    let mut uncertain = entry(300, "B.md", "X.md");
    uncertain.kind = "uncertain".into();
    cache.insert_fix_log(&uncertain).unwrap();
    let found = cache.fix_renames_from("notes", "B.md", a).unwrap();
    assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), [b]);
    assert!(cache
        .fix_renames_from("notes", "B.md", b)
        .unwrap()
        .is_empty());
    assert!(cache
        .fix_renames_from("other", "B.md", 0)
        .unwrap()
        .is_empty());
}

#[test]
fn log_and_pending_rows_survive_a_schema_version_rebuild_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let (log_before, pending_before);
    {
        let cache = Cache::open(&db_path).unwrap();
        cache
            .start_fix(
                &entry(100, "A.md", "B.md"),
                &[pending("x.md", "A.md", "B.md")],
            )
            .unwrap();
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
        log_before = common::dump_table(cache.connection(), "fix_log");
        pending_before = common::dump_table(cache.connection(), "pending_fixes");
        assert_eq!(log_before.len(), 1);
        cache
            .connection()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [CURRENT_SCHEMA_VERSION - 1],
            )
            .unwrap();
    }
    let mut cache = Cache::open(&db_path).unwrap();
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(CURRENT_SCHEMA_VERSION - 1)
    );
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(
        common::dump_table(cache.connection(), "fix_log"),
        log_before
    );
    assert_eq!(
        common::dump_table(cache.connection(), "pending_fixes"),
        pending_before
    );

    cache.rebuild(&FaultInjector::none()).unwrap();
    assert_eq!(
        common::dump_table(cache.connection(), "fix_log"),
        log_before
    );
    assert_eq!(
        cache.pending_fixes(None, None).unwrap().len(),
        1,
        "the work list reads back through the typed API after a rebuild"
    );
}

/// What decides whether a move is already logged and still stands is read
/// by the database: the exact move, and the entries that took a file away
/// from the new place or brought one to the old place; nothing else.
#[test]
fn the_moves_between_two_places_are_selected_by_the_database() {
    let (_dir, cache) = cache();
    let put = |kind: &str, os: &str, op: &str, ns: &str, np: &str| {
        let mut e = entry(1, op, np);
        e.kind = kind.into();
        e.old_source = Some(os.into());
        e.new_source = Some(ns.into());
        cache.insert_fix_log(&e).unwrap()
    };
    let exact = put("move", "a", "x.md", "b", "x.md");
    let back = put("move", "b", "x.md", "a", "x.md");
    let away = put("rename", "b", "x.md", "b", "y.md");
    let to_old = put("rename", "a", "w.md", "a", "x.md");
    put("move", "a", "other.md", "b", "other.md");
    put("rename", "notes", "p.md", "notes", "q.md");
    put("uncertain", "a", "x.md", "b", "x.md");

    let ids: Vec<i64> = cache
        .fix_log_moves_between(("a", "x.md"), ("b", "x.md"))
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(ids, vec![exact, back, away, to_old]);
}
