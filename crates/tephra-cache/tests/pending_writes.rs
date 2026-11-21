//! `pending_writes`: what a fix is about to write into a row's file,
//! recorded before the rename so a run that stops after it can tell the
//! file was written (PR-5 R7). Durable like the other fix tables: a
//! rebuild leaves it alone, and the row's next change removes it. Real
//! SQLite in a tempdir.

mod common;

use tephra_cache::{
    Cache, FaultInjector, NewFixLog, NewPendingFix, PendingOp, PendingWrite, STATE_PENDING,
};

fn one_row(cache: &Cache) -> i64 {
    let log = NewFixLog {
        created_at: 10,
        kind: "rename".into(),
        status: "pending".into(),
        old_source: Some("notes".into()),
        old_path: Some("A.md".into()),
        new_source: Some("notes".into()),
        new_path: Some("B.md".into()),
        files_changed: 0,
        detail_json: None,
    };
    let row = NewPendingFix {
        source: "notes".into(),
        file_path: "Home.md".into(),
        old_source: Some("notes".into()),
        old_path: Some("A.md".into()),
        new_source: Some("notes".into()),
        new_path: Some("B.md".into()),
    };
    let (_, ids) = cache.start_fix(&log, &[row]).unwrap();
    ids[0]
}

fn write() -> PendingWrite {
    PendingWrite {
        len: 42,
        hash: "abc123".into(),
        links: 3,
    }
}

#[test]
fn a_marked_write_is_read_back_and_replaced_by_the_next_mark() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = one_row(&cache);
    assert_eq!(cache.pending_write(id).unwrap(), None);
    cache.mark_pending_write(id, &write(), 11).unwrap();
    assert_eq!(cache.pending_write(id).unwrap(), Some(write()));
    let newer = PendingWrite {
        len: 50,
        hash: "def".into(),
        links: 1,
    };
    cache.mark_pending_write(id, &newer, 12).unwrap();
    assert_eq!(cache.pending_write(id).unwrap(), Some(newer));
    assert_eq!(cache.table_row_count("pending_writes").unwrap(), 1);
}

#[test]
fn the_marks_survive_a_reopen_and_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = one_row(&cache);
    cache.mark_pending_write(id, &write(), 11).unwrap();
    let path = dir.path().join("cache.db");
    drop(cache);
    let mut cache = Cache::open(&path).unwrap();
    assert_eq!(cache.pending_write(id).unwrap(), Some(write()), "reopen");
    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    assert_eq!(cache.pending_write(id).unwrap(), Some(write()), "rebuild");
}

#[test]
fn any_change_of_the_row_removes_its_mark() {
    for op in [
        PendingOp::Delete,
        PendingOp::Update {
            state: STATE_PENDING.into(),
            reason: Some("changed".into()),
            attempts: 1,
        },
        PendingOp::Repoint {
            new_source: "notes".into(),
            new_path: "C.md".into(),
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cache = common::open_temp_cache(&dir);
        let id = one_row(&cache);
        cache.mark_pending_write(id, &write(), 11).unwrap();
        assert!(cache.apply_fix_step(id, &op, 12, None, |_| None).unwrap());
        assert_eq!(cache.pending_write(id).unwrap(), None, "{op:?}");
    }
}
