//! `Cache::begin_source_rename` (G13): the registry row, the durable
//! tables that hold a source by name, the log entry and the pending list
//! change in one transaction; a refusal changes nothing.

mod common;

use tephra_cache::{
    Cache, CacheError, NewFixLog, NewPendingFix, NewSourceRename, NewUnit, SourceStatus,
};

fn log() -> NewFixLog {
    NewFixLog {
        created_at: 10,
        kind: "source_rename".into(),
        status: "partial".into(),
        old_source: Some("tephrad".into()),
        old_path: Some("@tephrad:".into()),
        new_source: Some("daemon".into()),
        new_path: Some("@daemon:".into()),
        files_changed: 0,
        detail_json: None,
    }
}

fn pending(source: &str, file: &str) -> NewPendingFix {
    NewPendingFix {
        source: source.into(),
        file_path: file.into(),
        old_source: Some("tephrad".into()),
        old_path: None,
        new_source: Some("daemon".into()),
        new_path: None,
    }
}

fn setup(cache: &Cache) -> i64 {
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    let id = cache
        .register_source("tephrad", "/repo/crates/tephrad", SourceStatus::Active)
        .unwrap();
    cache
        .insert_unit(&NewUnit {
            source_id: id,
            rel_path: "a.md",
            mtime: Some(1),
            frontmatter_json: None,
            kind: "prose",
            size: Some(1),
            too_large: false,
            hash: None,
            inode: None,
            device: None,
        })
        .unwrap();
    id
}

#[test]
fn renames_the_row_in_place_and_keeps_its_units() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = setup(&cache);
    let log = log();
    let pend = [pending("tephra", "note.md"), pending("tephrad", "a.md")];
    let done = cache
        .begin_source_rename(&NewSourceRename {
            old_name: "tephrad",
            new_name: "daemon",
            created_at: 10,
            log: &log,
            pending: &pend,
        })
        .unwrap();
    assert_eq!(done.source_id, id);
    assert!(cache.get_source("tephrad").unwrap().is_none());
    let row = cache.get_source("daemon").unwrap().unwrap();
    assert_eq!(row.id, id);
    assert!(cache.get_unit_by_rel_path(id, "a.md").unwrap().is_some());
    let rows = cache.pending_fixes(None, Some(done.fix_id)).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(cache.list_fix_log(None, 10).unwrap().len(), 1);
}

#[test]
fn a_taken_name_is_refused_without_regard_to_case_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    setup(&cache);
    let log = log();
    let pend = [pending("tephra", "note.md")];
    let err = cache
        .begin_source_rename(&NewSourceRename {
            old_name: "tephrad",
            new_name: "TEPHRA",
            created_at: 10,
            log: &log,
            pending: &pend,
        })
        .unwrap_err();
    assert!(matches!(err, CacheError::DuplicateSource(_)), "{err:?}");
    assert!(cache.get_source("tephrad").unwrap().is_some());
    assert!(cache.pending_fixes(None, None).unwrap().is_empty());
    assert!(cache.list_fix_log(None, 10).unwrap().is_empty());
}

#[test]
fn source_rename_logged_is_true_only_for_a_committed_rename_of_that_pair() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    setup(&cache);
    assert!(!cache.source_rename_logged("tephrad", "daemon").unwrap());
    let log = log();
    cache
        .begin_source_rename(&NewSourceRename {
            old_name: "tephrad",
            new_name: "daemon",
            created_at: 10,
            log: &log,
            pending: &[],
        })
        .unwrap();
    assert!(cache.source_rename_logged("tephrad", "daemon").unwrap());
    assert!(!cache.source_rename_logged("daemon", "tephrad").unwrap());
    assert!(!cache.source_rename_logged("tephrad", "other").unwrap());
    assert!(!cache.source_rename_logged("ghost", "daemon").unwrap());
}

#[test]
fn an_unknown_name_is_refused_and_a_case_only_change_of_the_own_name_is_not_taken() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    setup(&cache);
    let log = log();
    let err = cache
        .begin_source_rename(&NewSourceRename {
            old_name: "ghost",
            new_name: "x",
            created_at: 10,
            log: &log,
            pending: &[],
        })
        .unwrap_err();
    assert!(matches!(err, CacheError::SourceNotFound(_)), "{err:?}");
    cache
        .begin_source_rename(&NewSourceRename {
            old_name: "tephrad",
            new_name: "Tephrad",
            created_at: 10,
            log: &log,
            pending: &[],
        })
        .unwrap();
    assert!(cache.get_source("Tephrad").unwrap().is_some());
}
