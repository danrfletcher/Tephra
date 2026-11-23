//! A remote source's listing: `sources` + `units` rows for the graph, and
//! the durable `remote_items` / `remote_sources` / `remote_cursors` rows
//! that survive a schema bump (a rebuild drops `units`). Local rows are
//! unchanged by any of it. Real SQLite in a tempdir.

mod common;

use tephra_cache::{
    remote_root, Cache, CacheError, FaultInjector, NewRemoteItem, NewRemoteSource, SourceStatus,
    ITEM_KIND_SUBMODULE, ITEM_KIND_SYMLINK, REASON_LFS_POINTER, STATUS_ACTIVE, STATUS_INDEXING,
};

const CAP: u64 = 20 * 1024 * 1024;

const NEW: NewRemoteSource<'static> = NewRemoteSource {
    name: "github-atlas",
    kind: "github",
    target: "danrfletcher/atlas",
    branch: Some("main"),
    cursor: "c0ffee",
    now: 1_700_000_000,
};

fn items() -> Vec<NewRemoteItem<'static>> {
    vec![
        NewRemoteItem {
            path: "README.md",
            kind: "prose",
            sha: "a1",
            size: Some(120),
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "src/main.rs",
            kind: "code",
            sha: "a2",
            size: Some(900),
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "logo.png",
            kind: "binary",
            sha: "a3",
            size: Some(5000),
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "link.md",
            kind: ITEM_KIND_SYMLINK,
            sha: "a4",
            size: Some(9),
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "vendor/lib",
            kind: ITEM_KIND_SUBMODULE,
            sha: "a5",
            size: None,
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "big.txt",
            kind: "text-other",
            sha: "a6",
            size: Some(CAP as i64 + 1),
            file_id: None,
            revision: None,
        },
    ]
}

fn add(cache: &Cache) -> i64 {
    cache.add_remote_source(&NEW, &items(), CAP).expect("add")
}

#[test]
fn a_listing_makes_source_unit_and_item_rows_in_the_remote_state() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);

    let src = cache.get_source("github-atlas").unwrap().unwrap();
    assert_eq!(src.id, id);
    assert!(src.is_remote());
    assert_eq!(src.path, remote_root("github", "github-atlas"));
    assert_eq!(src.status, SourceStatus::Active);

    let remote = cache.remote_source("github-atlas").unwrap().unwrap();
    assert_eq!(remote.target, "danrfletcher/atlas");
    assert_eq!(remote.branch.as_deref(), Some("main"));
    assert_eq!(remote.status, STATUS_INDEXING);
    assert!(remote.cache);
    let cursor = cache.remote_cursor("github-atlas").unwrap().unwrap();
    assert_eq!(cursor.cursor.as_deref(), Some("c0ffee"));
    assert!(!cursor.index_complete);

    let rows = cache.remote_items("github-atlas").unwrap();
    assert_eq!(rows.len(), 6);
    assert!(rows
        .iter()
        .all(|r| r.state == "remote" && r.reason.is_none()));

    let unit = |p: &str| cache.get_unit_by_rel_path(id, p).unwrap().unwrap();
    assert_eq!(unit("README.md").kind, "prose");
    assert_eq!(unit("link.md").kind, "binary");
    assert_eq!(unit("vendor/lib").kind, "binary");
    assert!(unit("big.txt").too_large, "one byte over the cap");
    assert!(!unit("src/main.rs").too_large);
}

#[test]
fn a_failed_listing_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let dup = [
        NewRemoteItem {
            path: "a.md",
            kind: "prose",
            sha: "1",
            size: Some(1),
            file_id: None,
            revision: None,
        },
        NewRemoteItem {
            path: "a.md",
            kind: "prose",
            sha: "1",
            size: Some(1),
            file_id: None,
            revision: None,
        },
    ];
    assert!(cache.add_remote_source(&NEW, &dup, CAP).is_err());
    assert!(cache.get_source("github-atlas").unwrap().is_none());
    assert!(cache.remote_source("github-atlas").unwrap().is_none());
    assert!(cache.remote_items("github-atlas").unwrap().is_empty());
    assert!(cache.remote_cursor("github-atlas").unwrap().is_none());
    // And the name is free again.
    add(&cache);
}

#[test]
fn a_name_already_taken_is_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("github-atlas", "/some/dir", SourceStatus::Active)
        .unwrap();
    assert!(matches!(
        cache.add_remote_source(&NEW, &items(), CAP),
        Err(CacheError::DuplicateSource(_))
    ));
    assert!(cache.remote_source("github-atlas").unwrap().is_none());
}

#[test]
fn progress_counts_the_text_items_within_the_cap_and_pending_lists_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    // README.md and src/main.rs are the text items within the cap.
    let p = cache.remote_index_progress("github-atlas", CAP).unwrap();
    assert_eq!((p.done, p.total), (0, 2));
    let pending: Vec<String> = cache
        .remote_pending_items("github-atlas", CAP)
        .unwrap()
        .into_iter()
        .map(|i| i.path)
        .collect();
    assert_eq!(pending, ["README.md", "src/main.rs"]);

    cache
        .set_remote_item_reason("github-atlas", "README.md", Some(REASON_LFS_POINTER))
        .unwrap();
    let p = cache.remote_index_progress("github-atlas", CAP).unwrap();
    assert_eq!((p.done, p.total), (1, 2));
    assert_eq!(
        cache
            .remote_pending_items("github-atlas", CAP)
            .unwrap()
            .len(),
        1
    );

    cache
        .set_remote_status("github-atlas", STATUS_ACTIVE, None, Some(5))
        .unwrap();
    let r = cache.remote_source("github-atlas").unwrap().unwrap();
    assert_eq!((r.status.as_str(), r.last_sync_at), ("active", Some(5)));
}

#[test]
fn item_rows_survive_a_schema_bump_and_the_units_are_made_again_without_a_remote_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = common::open_temp_cache(&dir);
    let local = common::seed_source_with_units(&cache, "notes", "/tmp/notes", SourceStatus::Active);
    let id = add(&cache);
    cache
        .set_remote_item_reason("github-atlas", "README.md", Some(REASON_LFS_POINTER))
        .unwrap();
    cache
        .set_remote_status("github-atlas", STATUS_ACTIVE, None, None)
        .unwrap();
    cache
        .set_remote_index_complete("github-atlas", true, 1)
        .unwrap();
    let before = cache.remote_items("github-atlas").unwrap();

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    // The durable rows are exactly as they were.
    assert_eq!(cache.remote_items("github-atlas").unwrap(), before);
    assert!(cache.remote_source("github-atlas").unwrap().is_some());
    assert!(cache
        .get_unit_by_rel_path(id, "README.md")
        .unwrap()
        .is_none());

    // Local sources: the existing rebuild behaviour, untouched by remotes
    // (the rebuild drops their units; the daemon rescans them).
    assert!(cache.get_source("notes").unwrap().is_some());
    let _ = local;

    assert!(cache.restore_remote_units("github-atlas", CAP).unwrap());
    let unit = cache
        .get_unit_by_rel_path(id, "src/main.rs")
        .unwrap()
        .unwrap();
    assert_eq!((unit.kind.as_str(), unit.size), ("code", Some(900)));
    assert!(
        cache
            .get_unit_by_rel_path(id, "big.txt")
            .unwrap()
            .unwrap()
            .too_large
    );
    // The first index is to be redone: bytes were dropped with the rows.
    let cursor = cache.remote_cursor("github-atlas").unwrap().unwrap();
    assert!(!cursor.index_complete);
    assert_eq!(
        cache.remote_source("github-atlas").unwrap().unwrap().status,
        STATUS_INDEXING
    );
    assert!(cache
        .remote_items("github-atlas")
        .unwrap()
        .iter()
        .all(|i| i.reason.is_none()));

    // Nothing to do the second time.
    assert!(!cache.restore_remote_units("github-atlas", CAP).unwrap());
}

#[test]
fn local_sources_are_not_remote() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/tmp/notes", SourceStatus::Active);
    assert!(!cache.get_source("notes").unwrap().unwrap().is_remote());
    assert!(cache.remote_sources().unwrap().is_empty());
    assert!(!cache.restore_remote_units("notes", CAP).unwrap());
}
