//! The cache half of a promote (FR22): the one transaction, the
//! re-owning of rows and the simulation that leaves nothing behind.

mod common;

use tephra_cache::{
    Cache, CacheError, NewFixLog, NewPendingFix, NewPromote, NewUnit, SourceStatus,
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

fn log() -> NewFixLog {
    NewFixLog {
        created_at: 10,
        kind: "promote".into(),
        status: "partial".into(),
        old_source: Some("tephra".into()),
        old_path: Some("crates/tephrad".into()),
        new_source: Some("tephrad".into()),
        new_path: Some("@tephrad:".into()),
        files_changed: 0,
        detail_json: None,
    }
}

fn pending(file: &str) -> NewPendingFix {
    NewPendingFix {
        source: "tephra".into(),
        file_path: file.into(),
        old_source: Some("tephra".into()),
        old_path: Some("crates/tephrad/a.md".into()),
        new_source: Some("tephrad".into()),
        new_path: Some("a.md".into()),
    }
}

fn promote_request<'a>(
    log: &'a NewFixLog,
    pending: &'a [NewPendingFix],
    name: &'a str,
) -> NewPromote<'a> {
    NewPromote {
        name,
        root: "/repo/crates/tephrad",
        parent: "tephra",
        rel_path: "crates/tephrad",
        created_at: 10,
        log,
        pending,
    }
}

#[test]
fn begin_promote_writes_the_row_the_exclusion_the_log_and_the_list_together() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    let (log, rows) = (log(), [pending("README.md"), pending("docs/x.md")]);
    let done = cache
        .begin_promote(&promote_request(&log, &rows, "tephrad"))
        .unwrap();

    let source = cache.get_source("tephrad").unwrap().unwrap();
    assert_eq!(source.id, done.source_id);
    assert_eq!(source.status, SourceStatus::Active);
    let roots = cache.promoted_roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(
        (
            roots[0].parent.as_str(),
            roots[0].rel_path.as_str(),
            roots[0].source.as_str()
        ),
        ("tephra", "crates/tephrad", "tephrad")
    );
    assert_eq!(cache.promoted_roots_of("tephra").unwrap(), roots);
    assert!(cache.promoted_roots_of("tephrad").unwrap().is_empty());
    assert_eq!(
        cache.get_fix_log(done.fix_id).unwrap().unwrap().kind,
        "promote"
    );
    let pending_rows = cache.pending_fixes(None, Some(done.fix_id)).unwrap();
    assert_eq!(pending_rows.len(), 2);
    assert!(
        !cache.scan_finished(done.source_id).unwrap(),
        "the new source's scan is open until the caller finishes it"
    );
    assert_eq!(
        cache.sources_with_unfinished_scan().unwrap().len(),
        1,
        "a crash now has the next start scan the new source"
    );
}

#[test]
fn a_taken_name_or_a_missing_parent_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    cache
        .register_source("taken", "/elsewhere", SourceStatus::Active)
        .unwrap();
    let (log, rows) = (log(), [pending("README.md")]);

    let err = cache
        .begin_promote(&promote_request(&log, &rows, "taken"))
        .unwrap_err();
    assert!(matches!(err, CacheError::DuplicateSource(_)), "{err:?}");

    let mut missing = promote_request(&log, &rows, "tephrad");
    missing.parent = "nope";
    let err = cache.begin_promote(&missing).unwrap_err();
    assert!(matches!(err, CacheError::SourceNotFound(_)), "{err:?}");

    for table in ["promoted_roots", "fix_log", "pending_fixes", "scan_state"] {
        assert_eq!(cache.table_row_count(table).unwrap(), 0, "{table}");
    }
    assert_eq!(cache.source_row_count().unwrap(), 2);
}

#[test]
fn reown_moves_the_rows_under_the_folder_in_place_and_only_those() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let parent = cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    let child = cache
        .register_source("tephrad", "/repo/café notes", SourceStatus::Active)
        .unwrap();
    let inside = add_unit(&cache, parent, "café notes/src/a.md");
    let top = add_unit(&cache, parent, "café notes/b.md");
    let sibling_name = add_unit(&cache, parent, "café notes 2/c.md");
    let elsewhere = add_unit(&cache, parent, "README.md");

    let moved = cache.reown_units(parent, child, "café notes").unwrap();
    assert_eq!(moved, 2);

    let row = |id| cache.get_unit(id).unwrap().unwrap();
    assert_eq!(
        (row(inside).source_id, row(inside).rel_path.as_str()),
        (child, "src/a.md")
    );
    assert_eq!(
        (row(top).source_id, row(top).rel_path.as_str()),
        (child, "b.md")
    );
    assert_eq!(
        row(sibling_name).source_id,
        parent,
        "a folder whose name starts the same stays"
    );
    assert_eq!(row(elsewhere).source_id, parent);

    assert_eq!(
        cache.reown_units(parent, child, "café notes").unwrap(),
        0,
        "a second run finds nothing to move"
    );
}

#[test]
fn simulate_promote_shows_the_result_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let parent = cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    add_unit(&cache, parent, "crates/tephrad/a.md");
    add_unit(&cache, parent, "README.md");

    let seen = cache
        .simulate_promote(
            parent,
            "tephrad",
            "/repo/crates/tephrad",
            "crates/tephrad",
            |c| {
                let child = c.get_source("tephrad").unwrap().unwrap();
                (
                    c.get_unit_by_rel_path(child.id, "a.md").unwrap().is_some(),
                    c.get_unit_by_rel_path(parent, "crates/tephrad/a.md")
                        .unwrap()
                        .is_some(),
                )
            },
        )
        .unwrap();
    assert_eq!(seen, (true, false));

    assert!(cache.get_source("tephrad").unwrap().is_none());
    assert!(cache
        .get_unit_by_rel_path(parent, "crates/tephrad/a.md")
        .unwrap()
        .is_some());
    assert_eq!(cache.table_row_count("promoted_roots").unwrap(), 0);
}

#[test]
fn a_source_already_promoted_out_of_the_folder_becomes_a_child_of_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    let parent = cache.get_source("tephra").unwrap().unwrap();
    add_unit(&cache, parent.id, "crates/a.md");
    let log = log();
    // The inner source was promoted first, then an outer folder over it.
    cache
        .begin_promote(&NewPromote {
            name: "inner",
            root: "/repo/crates/tephrad/inner",
            parent: "tephra",
            rel_path: "crates/tephrad/inner",
            created_at: 1,
            log: &log,
            pending: &[],
        })
        .unwrap();
    cache
        .begin_promote(&NewPromote {
            name: "crates",
            root: "/repo/crates",
            parent: "tephra",
            rel_path: "crates",
            created_at: 2,
            log: &log,
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
            ("crates".into(), "tephrad/inner".into(), "inner".into()),
            ("tephra".into(), "crates".into(), "crates".into()),
        ]
    );
    // A sibling whose name only starts like the folder is not touched.
    assert_eq!(cache.promoted_roots_of("tephra").unwrap().len(), 1);
}
