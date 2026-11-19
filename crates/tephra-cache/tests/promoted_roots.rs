//! The `promoted_roots` row across an unpromote (FR19, FR22): it lives
//! while the promoted source exists, survives a schema rebuild, and goes
//! with the source in the one transaction of `begin_unpromote`.

mod common;

use tephra_cache::{
    Cache, CacheError, FaultInjector, NewFixLog, NewPendingFix, NewPromote, NewUnit, NewUnpromote,
    SourceStatus,
};

fn add_unit(cache: &Cache, source_id: i64, rel_path: &str) -> i64 {
    cache
        .insert_unit(&NewUnit {
            source_id,
            rel_path,
            mtime: Some(1),
            frontmatter_json: None,
            kind: "prose",
            size: Some(1),
            too_large: false,
            hash: None,
            inode: None,
            device: None,
        })
        .expect("insert unit")
}

fn log(kind: &str) -> NewFixLog {
    NewFixLog {
        created_at: 10,
        kind: kind.into(),
        status: "partial".into(),
        old_source: Some("tephra".into()),
        old_path: Some("@tephrad:".into()),
        new_source: Some("tephra".into()),
        new_path: Some("crates/tephrad".into()),
        files_changed: 0,
        detail_json: None,
    }
}

fn pending(file: &str) -> NewPendingFix {
    NewPendingFix {
        source: "tephra".into(),
        file_path: file.into(),
        old_source: Some("tephrad".into()),
        old_path: Some("src/a.md".into()),
        new_source: Some("tephra".into()),
        new_path: Some("crates/tephrad/src/a.md".into()),
    }
}

/// `tephra` with `crates/tephrad` promoted as `tephrad` (its two files
/// re-owned under the new root) and one file left in the parent.
fn promoted(cache: &Cache) -> (i64, i64) {
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    let parent = cache.get_source("tephra").unwrap().unwrap();
    add_unit(cache, parent.id, "README.md");
    add_unit(cache, parent.id, "crates/tephrad/src/a.md");
    add_unit(cache, parent.id, "crates/tephrad/b.md");
    // Same sibling prefix, not inside the folder.
    add_unit(cache, parent.id, "crates/tephrad-old/c.md");
    let promote_log = log("promote");
    let done = cache
        .begin_promote(&NewPromote {
            name: "tephrad",
            root: "/repo/crates/tephrad",
            parent: "tephra",
            rel_path: "crates/tephrad",
            created_at: 5,
            log: &promote_log,
            pending: &[],
        })
        .unwrap();
    cache
        .reown_units(parent.id, done.source_id, "crates/tephrad")
        .unwrap();
    (parent.id, done.source_id)
}

fn unpromote<'a>(log: &'a NewFixLog, pending: &'a [NewPendingFix]) -> NewUnpromote<'a> {
    NewUnpromote {
        name: "tephrad",
        parent: "tephra",
        rel_path: "crates/tephrad",
        created_at: 20,
        log,
        pending,
    }
}

#[test]
fn unpromote_deletes_the_row_and_the_source_and_moves_the_units_back_in_one_go() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (parent_id, child_id) = promoted(&cache);
    assert_eq!(cache.promoted_roots().unwrap().len(), 1);
    assert!(cache
        .get_unit_by_rel_path(child_id, "src/a.md")
        .unwrap()
        .is_some());

    let (log, rows) = (log("unpromote"), [pending("README.md")]);
    let done = cache.begin_unpromote(&unpromote(&log, &rows)).unwrap();

    assert_eq!(done.units_moved, 2);
    assert!(cache.promoted_roots().unwrap().is_empty());
    assert!(cache.get_source("tephrad").unwrap().is_none());
    for rel in [
        "crates/tephrad/src/a.md",
        "crates/tephrad/b.md",
        "README.md",
    ] {
        assert!(
            cache
                .get_unit_by_rel_path(parent_id, rel)
                .unwrap()
                .is_some(),
            "{rel}"
        );
    }
    assert_eq!(
        cache.get_fix_log(done.fix_id).unwrap().unwrap().kind,
        "unpromote"
    );
    assert_eq!(
        cache.pending_fixes(None, Some(done.fix_id)).unwrap().len(),
        1
    );
    // The promote's own entry is a record that stays.
    assert_eq!(cache.table_row_count("fix_log").unwrap(), 2);
    assert!(cache.units_of_source(child_id).unwrap().is_empty());
}

#[test]
fn a_refusal_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    cache
        .register_source("plain", "/elsewhere", SourceStatus::Active)
        .unwrap();
    let (log, rows) = (log("unpromote"), [pending("README.md")]);

    let not_promoted = NewUnpromote {
        name: "plain",
        ..unpromote(&log, &rows)
    };
    assert!(matches!(
        cache.begin_unpromote(&not_promoted),
        Err(CacheError::NotPromoted(_))
    ));
    let unknown = NewUnpromote {
        name: "ghost",
        ..unpromote(&log, &rows)
    };
    assert!(matches!(
        cache.begin_unpromote(&unknown),
        Err(CacheError::SourceNotFound(_))
    ));
    for table in ["fix_log", "pending_fixes", "promoted_roots"] {
        assert_eq!(cache.table_row_count(table).unwrap(), 0, "{table}");
    }
    assert!(cache.get_source("plain").unwrap().is_some());
}

#[test]
fn the_row_survives_a_schema_rebuild_while_the_source_exists_and_not_after_unpromote() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = common::open_temp_cache(&dir);
    promoted(&cache);
    let roots = cache.promoted_roots().unwrap();
    assert_eq!(roots.len(), 1);

    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    assert_eq!(cache.promoted_roots().unwrap(), roots, "FR19");
    assert!(cache.get_source("tephrad").unwrap().is_some());

    let (log, rows) = (log("unpromote"), []);
    cache
        .begin_unpromote(&unpromote(&log, &rows))
        .expect("unpromote after a rebuild");
    cache
        .rebuild(&FaultInjector::none())
        .expect("rebuild again");
    assert!(cache.promoted_roots().unwrap().is_empty());
}

#[test]
fn a_source_promoted_out_of_the_one_that_folds_back_becomes_a_child_of_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (_, child_id) = promoted(&cache);
    add_unit(&cache, child_id, "src/deep/x.md");
    let log_row = log("promote");
    let deep = cache
        .begin_promote(&NewPromote {
            name: "deep",
            root: "/repo/crates/tephrad/src/deep",
            parent: "tephrad",
            rel_path: "src/deep",
            created_at: 6,
            log: &log_row,
            pending: &[],
        })
        .unwrap();
    cache
        .reown_units(child_id, deep.source_id, "src/deep")
        .unwrap();

    let (log, rows) = (log("unpromote"), []);
    cache.begin_unpromote(&unpromote(&log, &rows)).unwrap();

    let roots: Vec<(String, String, String)> = cache
        .promoted_roots()
        .unwrap()
        .into_iter()
        .map(|r| (r.parent, r.rel_path, r.source))
        .collect();
    assert_eq!(
        roots,
        vec![(
            "tephra".into(),
            "crates/tephrad/src/deep".into(),
            "deep".into()
        )]
    );
}

#[test]
fn deleting_a_parent_source_leaves_its_promoted_child_a_source_of_its_own() {
    // The row stays (history, like the fix log; durable_fix_tables.rs), but
    // the parent is gone, so `begin_unpromote` has nothing to fold into and
    // refuses: the child is top-level (FR13, FR22).
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    promoted(&cache);
    cache.delete_source("tephra").unwrap();
    assert!(cache.get_source("tephrad").unwrap().is_some());
    let (log, rows) = (log("unpromote"), []);
    assert!(matches!(
        cache.begin_unpromote(&unpromote(&log, &rows)),
        Err(CacheError::SourceNotFound(_) | CacheError::NotPromoted(_))
    ));
    assert_eq!(
        cache.table_row_count("fix_log").unwrap(),
        1,
        "only the promote's"
    );
}

/// R1: the parent is deleted and a DIFFERENT source takes its name. The
/// row names `tephra` and `tephrad` still, but `tephrad` is not under
/// the new `tephra`'s root: the row is history, not a promote of it.
fn parent_replaced_by_another_source_of_the_same_name(cache: &Cache) -> i64 {
    promoted(cache);
    cache.delete_source("tephra").unwrap();
    cache
        .register_source("tephra", "/unrelated", SourceStatus::Active)
        .unwrap();
    let id = cache.get_source("tephra").unwrap().unwrap().id;
    add_unit(cache, id, "crates/tephrad/src/a.md");
    id
}

#[test]
fn a_stale_row_is_history_when_another_source_has_taken_the_parents_name() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let new_parent_id = parent_replaced_by_another_source_of_the_same_name(&cache);

    // History stays readable; nothing in force is left.
    assert_eq!(cache.promoted_roots().unwrap().len(), 1);
    assert!(cache.live_promoted_roots().unwrap().is_empty());
    assert!(
        cache.promoted_roots_of("tephra").unwrap().is_empty(),
        "the new `tephra` must not skip a folder it never gave up"
    );

    // The unpromote refuses and changes nothing, simulated or real.
    let (log, rows) = (log("unpromote"), [pending("README.md")]);
    assert!(matches!(
        cache.begin_unpromote(&unpromote(&log, &rows)),
        Err(CacheError::NotPromoted(_))
    ));
    assert!(matches!(
        cache.simulate_unpromote("tephrad", "tephra", "crates/tephrad", |_| ()),
        Err(CacheError::NotPromoted(_))
    ));
    assert!(cache.get_source("tephrad").unwrap().is_some());
    assert_eq!(cache.promoted_roots().unwrap().len(), 1);
    assert_eq!(cache.table_row_count("fix_log").unwrap(), 1);
    assert_eq!(cache.table_row_count("pending_fixes").unwrap(), 0);
    assert!(cache
        .get_unit_by_rel_path(new_parent_id, "crates/tephrad/src/a.md")
        .unwrap()
        .is_some());
}

#[test]
fn a_stale_row_is_not_moved_by_a_later_promote_of_the_new_parent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    parent_replaced_by_another_source_of_the_same_name(&cache);
    let log_row = log("promote");
    // A folder of the NEW `tephra` that contains the stale row's path.
    cache
        .begin_promote(&NewPromote {
            name: "crates",
            root: "/unrelated/crates",
            parent: "tephra",
            rel_path: "crates",
            created_at: 30,
            log: &log_row,
            pending: &[],
        })
        .unwrap();
    let roots: Vec<(String, String, String)> = cache
        .promoted_roots()
        .unwrap()
        .into_iter()
        .map(|r| (r.parent, r.rel_path, r.source))
        .collect();
    assert_eq!(
        roots,
        vec![
            // The stale row, untouched: still history of the old parent.
            ("tephra".into(), "crates/tephrad".into(), "tephrad".into()),
            ("tephra".into(), "crates".into(), "crates".into()),
        ]
    );
    assert_eq!(cache.live_promoted_roots().unwrap().len(), 1);
}
