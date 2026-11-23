//! What keeping a remote source current writes (PR-6): the cursor with its
//! ETag, the item steps of a poll, a deleted file whose edited copy is kept,
//! and a source whose cursor row is lost. Real SQLite in a tempdir.

mod common;

use tephra_cache::{
    is_kept_deleted, Cache, FaultInjector, NewRemoteItem, NewRemoteSource, PollState,
    ITEM_STATE_NEEDS_ATTENTION, REASON_DELETED_ON_REMOTE, REASON_LFS_POINTER,
    REASON_READDED_AFTER_DELETE, STATUS_ACTIVE, STATUS_INDEXING, STATUS_OFFLINE,
};

const CAP: u64 = 20 * 1024 * 1024;
const NAME: &str = "github-atlas";

fn item<'a>(path: &'a str, sha: &'a str) -> NewRemoteItem<'a> {
    NewRemoteItem {
        path,
        kind: "prose",
        sha,
        size: Some(10),
        file_id: None,
        revision: None,
    }
}

fn add(cache: &Cache) -> i64 {
    let items = [item("a.md", "s1"), item("b.md", "s2"), item("c.md", "s3")];
    cache
        .add_remote_source(
            &NewRemoteSource {
                name: NAME,
                kind: "github",
                target: "danrfletcher/atlas",
                branch: Some("main"),
                cursor: "c0ffee",
                now: 1_000,
            },
            &items,
            CAP,
        )
        .expect("add")
}

fn units(cache: &Cache, id: i64) -> Vec<String> {
    let mut paths: Vec<String> = cache
        .units_with_status_for_source(NAME)
        .unwrap()
        .into_iter()
        .map(|u| u.unit.rel_path)
        .collect();
    paths.sort();
    assert_eq!(paths.len() as i64, count(cache, "units", id));
    paths
}

fn count(cache: &Cache, table: &str, source_id: i64) -> i64 {
    cache
        .connection()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE source_id = ?1"),
            [source_id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn the_cursor_keeps_the_commit_and_its_etag_and_a_bare_commit_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    assert_eq!(
        cache.remote_poll_state(NAME).unwrap(),
        Some(PollState {
            commit: Some("c0ffee".into()),
            etag: None,
            index_complete: false
        }),
        "a cursor written before the ETag was kept is just the commit"
    );
    cache
        .set_remote_poll_state(NAME, "abc123", Some("W/\"e1\""), 2_000)
        .unwrap();
    let state = cache.remote_poll_state(NAME).unwrap().unwrap();
    assert_eq!(state.commit.as_deref(), Some("abc123"));
    assert_eq!(state.etag.as_deref(), Some("W/\"e1\""));
    assert!(cache.remote_poll_state("nope").unwrap().is_none());
}

#[test]
fn a_304_poll_advances_last_sync_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);
    cache
        .set_remote_status(NAME, STATUS_ACTIVE, None, Some(1_000))
        .unwrap();
    let before = (
        cache.remote_items(NAME).unwrap(),
        cache.remote_cursor(NAME).unwrap(),
        units(&cache, id),
    );
    cache.set_remote_last_sync(NAME, 1_060).unwrap();
    let row = cache.remote_source(NAME).unwrap().unwrap();
    assert_eq!(row.last_sync_at, Some(1_060));
    assert_eq!(row.status, STATUS_ACTIVE);
    assert_eq!(
        before,
        (
            cache.remote_items(NAME).unwrap(),
            cache.remote_cursor(NAME).unwrap(),
            units(&cache, id)
        )
    );
}

#[test]
fn a_changed_item_gets_its_new_sha_and_loses_the_reason_of_an_old_read() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    cache
        .set_remote_item_reason(NAME, "a.md", Some(REASON_LFS_POINTER))
        .unwrap();
    cache.upsert_remote_item(NAME, &item("a.md", "s9")).unwrap();
    let rows = cache.remote_items(NAME).unwrap();
    let a = rows.iter().find(|r| r.path == "a.md").unwrap();
    assert_eq!(a.sha.as_deref(), Some("s9"));
    assert_eq!(a.reason, None);
    assert_eq!(rows.len(), 3);
}

#[test]
fn a_replaced_listing_unit_is_bare_and_has_no_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);
    let before = cache.get_unit_by_rel_path(id, "a.md").unwrap().unwrap();
    cache
        .connection()
        .execute("UPDATE units SET hash = 'x' WHERE id = ?1", [before.id])
        .unwrap();
    cache
        .replace_remote_unit_listing(id, &item("a.md", "s9"), CAP)
        .unwrap();
    let after = cache.get_unit_by_rel_path(id, "a.md").unwrap().unwrap();
    assert_ne!(
        after.id, before.id,
        "the old row is gone, a new one is made"
    );
    assert_eq!(after.hash, None);
    assert_eq!(units(&cache, id), ["a.md", "b.md", "c.md"]);
}

#[test]
fn deleting_an_item_removes_its_file_pin_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    let conn = cache.connection();
    conn.execute(
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES \
         (?1, 'a.md', 0, 1), (?1, 'docs', 1, 1)",
        [NAME],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO upload_queue (source, path, created_at) VALUES (?1, 'a.md', 1)",
        [NAME],
    )
    .unwrap();
    cache.delete_remote_item(NAME, "a.md").unwrap();
    let pins: Vec<String> = conn
        .prepare("SELECT path FROM remote_pins WHERE source = ?1")
        .unwrap()
        .query_map([NAME], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        pins,
        ["docs"],
        "the folder pin stays: it covers later files"
    );
    assert!(
        cache.has_queued_upload(NAME, "a.md").unwrap(),
        "an edit waiting to be sent is kept"
    );
    assert!(!cache.has_queued_upload(NAME, "b.md").unwrap());
    assert_eq!(cache.remote_items(NAME).unwrap().len(), 2);
}

#[test]
fn a_kept_deleted_item_has_the_mark_no_unit_and_survives_a_rebuild_of_the_units() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);
    // The remote deleted a.md; its copy has edits.
    let unit = cache.get_unit_by_rel_path(id, "a.md").unwrap().unwrap();
    cache.delete_unit(unit.id).unwrap();
    cache.keep_deleted_remote_item(NAME, "a.md").unwrap();
    let rows = cache.remote_items(NAME).unwrap();
    let a = rows.iter().find(|r| r.path == "a.md").unwrap();
    assert_eq!(a.state, ITEM_STATE_NEEDS_ATTENTION);
    assert_eq!(a.reason.as_deref(), Some("deleted on GitHub"));
    assert!(is_kept_deleted(a));
    assert_eq!(units(&cache, id), ["b.md", "c.md"]);

    // A schema rebuild drops `units`; start-up makes them again from the
    // items. The kept file gets no unit back and keeps its mark.
    cache.delete_units_for_source(id).unwrap();
    assert!(cache.restore_remote_units(NAME, CAP).unwrap());
    assert_eq!(units(&cache, id), ["b.md", "c.md"]);
    let a = cache
        .remote_items(NAME)
        .unwrap()
        .into_iter()
        .find(|r| r.path == "a.md")
        .unwrap();
    assert_eq!(a.reason.as_deref(), Some(REASON_DELETED_ON_REMOTE));
    assert_eq!(a.state, ITEM_STATE_NEEDS_ATTENTION);
}

#[test]
fn a_kept_file_added_again_stays_needs_attention_with_its_own_reason() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    cache.keep_deleted_remote_item(NAME, "a.md").unwrap();
    cache
        .upsert_remote_item(NAME, &item("a.md", "s-new"))
        .unwrap();
    let a = cache
        .remote_items(NAME)
        .unwrap()
        .into_iter()
        .find(|r| r.path == "a.md")
        .unwrap();
    assert_eq!(a.sha.as_deref(), Some("s-new"));
    assert_eq!(a.state, ITEM_STATE_NEEDS_ATTENTION);
    assert_eq!(a.reason.as_deref(), Some(REASON_READDED_AFTER_DELETE));
    assert!(!is_kept_deleted(&a));
}

#[test]
fn a_lost_cursor_row_is_made_again_and_the_index_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);
    cache.set_remote_index_complete(NAME, true, 5).unwrap();
    cache
        .set_remote_status(NAME, STATUS_OFFLINE, Some("cannot reach GitHub"), None)
        .unwrap();
    cache
        .set_remote_item_reason(NAME, "b.md", Some(REASON_LFS_POINTER))
        .unwrap();
    cache.keep_deleted_remote_item(NAME, "c.md").unwrap();
    let c = cache.get_unit_by_rel_path(id, "c.md").unwrap().unwrap();
    cache.delete_unit(c.id).unwrap();
    cache
        .connection()
        .execute("UPDATE units SET hash = 'x' WHERE source_id = ?1", [id])
        .unwrap();
    cache
        .connection()
        .execute("DELETE FROM remote_cursors WHERE source = ?1", [NAME])
        .unwrap();

    assert!(cache.reset_remote_index(NAME, 77, CAP).unwrap());

    assert_eq!(
        cache.remote_poll_state(NAME).unwrap(),
        Some(PollState {
            commit: None,
            etag: None,
            index_complete: false
        })
    );
    assert_eq!(
        cache.remote_source(NAME).unwrap().unwrap().status,
        STATUS_INDEXING
    );
    assert_eq!(units(&cache, id), ["a.md", "b.md"]);
    for p in ["a.md", "b.md"] {
        let u = cache.get_unit_by_rel_path(id, p).unwrap().unwrap();
        assert_eq!(u.hash, None, "{p}: to be read again");
    }
    let rows = cache.remote_items(NAME).unwrap();
    let reason = |p: &str| rows.iter().find(|r| r.path == p).unwrap().reason.clone();
    assert_eq!(reason("b.md"), None, "an old read's reason is cleared");
    assert_eq!(
        reason("c.md").as_deref(),
        Some(REASON_DELETED_ON_REMOTE),
        "the mark of a kept copy is not"
    );
    assert!(!cache.reset_remote_index("nope", 1, CAP).unwrap());
}

#[test]
fn a_schema_rebuild_keeps_the_cursor_with_its_commit_and_etag() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = common::open_temp_cache(&dir);
    add(&cache);
    cache
        .set_remote_poll_state(NAME, "deadbeef", Some("W/\"abc\""), 2_000)
        .unwrap();
    cache.set_remote_index_complete(NAME, true, 2_000).unwrap();
    let before = cache.remote_poll_state(NAME).unwrap().unwrap();
    assert_eq!(before.commit.as_deref(), Some("deadbeef"));
    assert_eq!(before.etag.as_deref(), Some("W/\"abc\""));
    assert!(before.index_complete);

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    assert_eq!(
        cache.remote_poll_state(NAME).unwrap().unwrap(),
        before,
        "the cursor, its ETag and the completed flag survive a rebuild"
    );
}
