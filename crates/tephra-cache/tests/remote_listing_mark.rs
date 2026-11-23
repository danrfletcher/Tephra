//! PR-10 (E8): a Drive source is registered with its feed position BEFORE
//! its listing is stored (`begin_remote_source`), the listing follows
//! (`fill_remote_source`) and a listing that fails is undone
//! (`abandon_remote_source`). Real SQLite in a tempdir.

mod common;

use tephra_cache::{
    decode_cursor, Cache, NewRemoteItem, NewRemoteSource, LISTING_MARK, REASON_LISTING,
    STATUS_INDEXING, STATUS_SYNCING,
};

const CAP: u64 = 20 * 1024 * 1024;

const NEW: NewRemoteSource<'static> = NewRemoteSource {
    name: "gdrive",
    kind: "drive",
    target: "f",
    branch: None,
    cursor: "103",
    now: 1_700_000_000,
};

fn open() -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(dir.path().join("cache.db")).unwrap();
    (dir, cache)
}

fn files() -> Vec<NewRemoteItem<'static>> {
    vec![NewRemoteItem {
        path: "Plan.md",
        kind: "prose",
        sha: "",
        size: Some(10),
        file_id: Some("d1"),
        revision: Some("r1"),
    }]
}

#[test]
fn begin_keeps_the_token_and_marks_the_listing_pending_with_no_item() {
    let (_dir, cache) = open();
    cache.begin_remote_source(&NEW).unwrap();

    let state = cache.remote_poll_state("gdrive").unwrap().unwrap();
    assert_eq!(state.commit.as_deref(), Some("103"));
    assert_eq!(state.etag.as_deref(), Some(LISTING_MARK));
    assert!(!state.index_complete);
    assert!(cache.remote_listing_pending("gdrive").unwrap());
    assert!(cache.remote_items("gdrive").unwrap().is_empty());
    let row = cache.remote_source("gdrive").unwrap().unwrap();
    assert_eq!(row.status, STATUS_SYNCING);
    assert_eq!(row.status_reason.as_deref(), Some(REASON_LISTING));
}

#[test]
fn fill_stores_the_listing_clears_the_mark_and_keeps_the_token() {
    let (_dir, cache) = open();
    cache.begin_remote_source(&NEW).unwrap();
    cache.fill_remote_source("gdrive", &files(), CAP).unwrap();

    assert!(!cache.remote_listing_pending("gdrive").unwrap());
    let cursor = cache.remote_cursor("gdrive").unwrap().unwrap();
    assert_eq!(
        decode_cursor(cursor.cursor.as_deref()),
        (Some("103".to_string()), None)
    );
    assert!(!cursor.index_complete);
    assert_eq!(cache.remote_items("gdrive").unwrap().len(), 1);
    let row = cache.remote_source("gdrive").unwrap().unwrap();
    assert_eq!(row.status, STATUS_INDEXING);
    assert_eq!(row.status_reason, None);
    // The unit is there for the graph.
    assert_eq!(
        cache.units_with_status_for_source("gdrive").unwrap().len(),
        1
    );
}

#[test]
fn fill_of_a_source_that_is_not_waiting_for_its_listing_writes_nothing() {
    let (_dir, cache) = open();
    assert!(cache.fill_remote_source("gdrive", &files(), CAP).is_err());
    cache.begin_remote_source(&NEW).unwrap();
    cache.fill_remote_source("gdrive", &files(), CAP).unwrap();
    // A second fill would add the rows twice.
    assert!(cache.fill_remote_source("gdrive", &files(), CAP).is_err());
    assert_eq!(cache.remote_items("gdrive").unwrap().len(), 1);
}

#[test]
fn abandon_takes_every_row_of_the_half_made_source_away_and_is_idempotent() {
    let (_dir, cache) = open();
    cache.begin_remote_source(&NEW).unwrap();
    cache.abandon_remote_source("gdrive").unwrap();
    cache.abandon_remote_source("gdrive").unwrap();

    assert!(cache.remote_source("gdrive").unwrap().is_none());
    assert!(cache.remote_cursor("gdrive").unwrap().is_none());
    assert!(cache.get_source("gdrive").unwrap().is_none());
    // The name can be used again.
    cache.begin_remote_source(&NEW).unwrap();
}
