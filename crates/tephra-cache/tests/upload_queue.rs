//! The upload buffer of a Drive source (atlas-27 PR-11): one row per file,
//! a second save changes nothing, the doubling backoff, the conditional
//! steps of a refusal, a landed upload and `resolve`, and the rows
//! surviving a cache rebuild. Real SQLite in a tempdir.

mod common;

use tephra_cache::{
    upload_backoff_secs, Cache, FaultInjector, QueuedEdit, UploadLanded, ITEM_STATE_CACHED,
    ITEM_STATE_PENDING_UPLOAD, REASON_CHANGED_ON_DRIVE, UPLOAD_REFUSED_PREFIX,
};

fn seeded(dir: &tempfile::TempDir) -> Cache {
    let cache = common::open_temp_cache(dir);
    cache
        .connection()
        .execute_batch(
            "INSERT INTO remote_sources (name, kind, target, created_at) \
                VALUES ('d', 'drive', 'folder-1', 1);
             INSERT INTO remote_items (source, path, kind, file_id, revision, size, state, last_used) VALUES
                ('d', 'a.md', 'prose', 'id-a', 'rev-1', 10, 'cached', 100),
                ('d', 'b.md', 'prose', 'id-b', 'rev-7', 20, 'remote', NULL),
                ('d', 'c.md', 'prose', 'id-c', 'rev-3', 30, 'downloading', NULL),
                ('d', 'p.md', 'prose', 'id-p', 'rev-4', 40, 'pinned', 100);
             INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('d', 'p.md', 0, 1);
             INSERT INTO remote_copy_stamps (source, path, hash) VALUES ('d', 'a.md', 'sha-old');",
        )
        .expect("seed");
    cache
}

fn state_of(cache: &Cache, path: &str) -> (String, Option<String>) {
    cache
        .connection()
        .query_row(
            "SELECT state, reason FROM remote_items WHERE source = 'd' AND path = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn the_backoff_doubles_from_one_second_to_the_cap_of_sixty_and_stays_there() {
    let table: [(u32, u64); 11] = [
        (0, 1),
        (1, 2),
        (2, 4),
        (3, 8),
        (4, 16),
        (5, 32),
        (6, 60),
        (7, 60),
        (8, 60),
        (9, 60),
        (10, 60),
    ];
    for (attempts, secs) in table {
        assert_eq!(upload_backoff_secs(attempts), secs, "after {attempts}");
    }
    assert_eq!(upload_backoff_secs(u32::MAX), 60, "no overflow, no limit");
}

#[test]
fn a_saved_edit_queues_one_row_based_on_the_recorded_revision() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 500).unwrap(),
        QueuedEdit::Queued
    );
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_PENDING_UPLOAD);
    let entry = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(entry.base_revision.as_deref(), Some("rev-1"));
    assert_eq!(entry.attempts, 0);
    assert_eq!(entry.created_at, 500);
}

#[test]
fn a_second_save_keeps_the_row_and_its_original_base() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 500).unwrap();
    // Drive moves on and a poll records the new revision on the item.
    cache
        .connection()
        .execute(
            "UPDATE remote_items SET revision = 'rev-2' WHERE path = 'a.md'",
            [],
        )
        .unwrap();
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 600).unwrap(),
        QueuedEdit::AlreadyQueued
    );
    let rows = cache.upload_entries("d").unwrap();
    assert_eq!(rows.len(), 1, "never two rows for one file");
    assert_eq!(rows[0].base_revision.as_deref(), Some("rev-1"));
    assert_eq!(rows[0].created_at, 500);
}

#[test]
fn a_re_created_evicted_file_is_queued_on_the_last_recorded_revision() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert_eq!(
        cache.queue_drive_edit("d", "b.md", 500).unwrap(),
        QueuedEdit::Queued
    );
    assert_eq!(state_of(&cache, "b.md").0, ITEM_STATE_PENDING_UPLOAD);
    assert_eq!(
        cache
            .upload_entry("d", "b.md")
            .unwrap()
            .unwrap()
            .base_revision
            .as_deref(),
        Some("rev-7")
    );
}

#[test]
fn an_item_being_fetched_or_unknown_is_not_queued() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert_eq!(
        cache.queue_drive_edit("d", "c.md", 1).unwrap(),
        QueuedEdit::Ignored
    );
    assert_eq!(
        cache.queue_drive_edit("d", "nope.md", 1).unwrap(),
        QueuedEdit::Ignored
    );
    assert!(cache.upload_entries("d").unwrap().is_empty());
    assert_eq!(state_of(&cache, "c.md").0, "downloading");
}

#[test]
fn only_a_due_row_of_a_pending_item_is_handed_to_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 500).unwrap();
    cache.queue_drive_edit("d", "b.md", 501).unwrap();
    let a = cache.upload_entry("d", "a.md").unwrap().unwrap();
    let at = cache
        .fail_upload_attempt(a.id, 0, 1_000, None, "offline")
        .unwrap();
    assert_eq!(at, Some(1_001));
    let due = |now| {
        cache
            .due_uploads("d", now)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect::<Vec<_>>()
    };
    assert_eq!(due(1_000), ["b.md"], "a.md waits one second");
    assert_eq!(due(1_001), ["a.md", "b.md"]);
}

#[test]
fn failed_tries_walk_the_backoff_table_and_a_stale_count_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    let id = cache.upload_entry("d", "a.md").unwrap().unwrap().id;
    let mut waits = Vec::new();
    for attempts in 0..9u32 {
        let at = cache
            .fail_upload_attempt(id, attempts, 10_000, None, "5xx")
            .unwrap()
            .expect("the count was current");
        waits.push(at - 10_000);
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(row.attempts, 9);
    assert_eq!(row.last_error.as_deref(), Some("5xx"));
    // A caller that read an old count loses.
    assert_eq!(
        cache
            .fail_upload_attempt(id, 2, 10_000, None, "late")
            .unwrap(),
        None
    );
    assert_eq!(
        cache
            .upload_entry("d", "a.md")
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("5xx")
    );
    // A rate limit's own time raises the next try.
    assert_eq!(
        cache
            .fail_upload_attempt(id, 9, 10_000, Some(10_300), "429")
            .unwrap(),
        Some(10_300)
    );
}

#[test]
fn the_retry_state_is_stored_so_the_schedule_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let cache = seeded(&dir);
        cache.queue_drive_edit("d", "a.md", 0).unwrap();
        let id = cache.upload_entry("d", "a.md").unwrap().unwrap().id;
        cache
            .fail_upload_attempt(id, 0, 50, None, "offline")
            .unwrap();
        cache
            .fail_upload_attempt(id, 1, 51, None, "offline")
            .unwrap();
    }
    let cache = common::open_temp_cache(&dir);
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!((row.attempts, row.next_retry_at), (2, Some(53)));
}

#[test]
fn a_conflict_hold_keeps_the_row_and_sets_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    assert!(cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9")
        )
        .unwrap());
    assert_eq!(
        state_of(&cache, "a.md"),
        (
            "needs_attention".to_string(),
            Some("changed on Drive".to_string())
        )
    );
    let conflict = cache.drive_conflict("d", "a.md").unwrap().unwrap();
    assert_eq!(conflict.head.as_deref(), Some("rev-9"));
    // A held item is not handed to the worker, and a new save changes
    // nothing.
    assert!(cache.due_uploads("d", i64::MAX).unwrap().is_empty());
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 9).unwrap(),
        QueuedEdit::Held
    );
    // Held twice: the second hold lost the race (the item is not pending).
    assert!(!cache
        .hold_upload("d", "a.md", Some("rev-1"), "again", None)
        .unwrap());
}

#[test]
fn drive_turning_an_upload_down_for_good_is_held_and_a_new_save_reopens_it() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    let reason = format!("{UPLOAD_REFUSED_PREFIX}the file was not found");
    assert!(cache
        .hold_upload("d", "a.md", Some("rev-1"), &reason, None)
        .unwrap());
    assert_eq!(
        cache.drive_conflict("d", "a.md").unwrap(),
        None,
        "no conflict"
    );
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 5).unwrap(),
        QueuedEdit::Queued
    );
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!((row.attempts, row.last_error), (0, None));
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_PENDING_UPLOAD);
    assert_eq!(cache.upload_entries("d").unwrap().len(), 1);
}

#[test]
fn a_landed_upload_with_the_file_unchanged_ends_the_row_and_stamps_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    let landed = UploadLanded {
        source: "d",
        path: "a.md",
        base_seen: Some("rev-1"),
        new_revision: "rev-2",
        size: 11,
        stamp: Some("sha-new"),
        file_unchanged: true,
    };
    assert!(cache.complete_upload(&landed, 700).unwrap());
    assert_eq!(
        state_of(&cache, "a.md"),
        (ITEM_STATE_CACHED.to_string(), None)
    );
    assert!(cache.upload_entries("d").unwrap().is_empty());
    assert_eq!(
        cache.remote_copy_stamp("d", "a.md").unwrap().as_deref(),
        Some("sha-new")
    );
    let (revision, size): (String, i64) = cache
        .connection()
        .query_row(
            "SELECT revision, size FROM remote_items WHERE path = 'a.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((revision.as_str(), size), ("rev-2", 11));
    // Done twice: the second call finds no row and changes nothing.
    assert!(!cache.complete_upload(&landed, 701).unwrap());
}

#[test]
fn a_save_during_the_upload_leaves_the_row_based_on_the_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    let id = cache.upload_entry("d", "a.md").unwrap().unwrap().id;
    cache.fail_upload_attempt(id, 0, 10, None, "x").unwrap();
    let landed = UploadLanded {
        source: "d",
        path: "a.md",
        base_seen: Some("rev-1"),
        new_revision: "rev-2",
        size: 11,
        stamp: Some("sha-sent"),
        file_unchanged: false,
    };
    assert!(cache.complete_upload(&landed, 700).unwrap());
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_PENDING_UPLOAD);
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(row.base_revision.as_deref(), Some("rev-2"));
    assert_eq!(
        (row.attempts, row.next_retry_at, row.last_error),
        (0, None, None)
    );
}

#[test]
fn a_pinned_file_is_pinned_again_after_its_upload() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "p.md", 0).unwrap();
    let landed = UploadLanded {
        source: "d",
        path: "p.md",
        base_seen: Some("rev-4"),
        new_revision: "rev-5",
        size: 41,
        stamp: Some("s"),
        file_unchanged: true,
    };
    assert!(cache.complete_upload(&landed, 1).unwrap());
    assert_eq!(state_of(&cache, "p.md").0, "pinned");
}

#[test]
fn resolve_mine_rebases_on_the_head_the_conflict_saw_and_only_one_caller_wins() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    let first = cache.drive_conflict("d", "a.md").unwrap().unwrap();
    let second = first.clone();
    assert!(cache.claim_conflict_for_mine(&first).unwrap());
    assert!(
        !cache.claim_conflict_for_mine(&second).unwrap(),
        "the second one finds nothing to resolve"
    );
    assert_eq!(
        state_of(&cache, "a.md"),
        ("pending_upload".to_string(), None)
    );
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(row.base_revision.as_deref(), Some("rev-9"));
    assert_eq!(row.last_error, None);
    assert_eq!(cache.drive_conflict("d", "a.md").unwrap(), None);
}

#[test]
fn resolve_theirs_drops_the_queue_row_and_makes_the_item_remote_on_drives_revision() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    let conflict = cache.drive_conflict("d", "a.md").unwrap().unwrap();
    assert!(cache
        .claim_conflict_for_theirs(&conflict, Some("rev-9"), Some(99))
        .unwrap());
    assert_eq!(state_of(&cache, "a.md"), ("remote".to_string(), None));
    assert!(cache.upload_entries("d").unwrap().is_empty());
    assert_eq!(cache.remote_copy_stamp("d", "a.md").unwrap(), None);
    assert!(
        !cache
            .claim_conflict_for_theirs(&conflict, Some("rev-9"), Some(99))
            .unwrap(),
        "a second resolve of the same conflict changes nothing"
    );
}

#[test]
fn a_poll_that_updates_the_item_keeps_a_conflict_reason() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    cache
        .upsert_remote_item(
            "d",
            &tephra_cache::NewRemoteItem {
                path: "a.md",
                kind: "prose",
                sha: "",
                size: Some(12),
                file_id: Some("id-a"),
                revision: Some("rev-10"),
            },
        )
        .unwrap();
    assert_eq!(
        state_of(&cache, "a.md"),
        (
            "needs_attention".to_string(),
            Some(REASON_CHANGED_ON_DRIVE.to_string())
        )
    );
    // The head the conflict saw is the queue's, not the item's.
    let conflict = cache.drive_conflict("d", "a.md").unwrap().unwrap();
    assert_eq!(conflict.head.as_deref(), Some("rev-9"));
}

#[test]
fn a_reverted_edit_cancels_the_row_only_while_the_item_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert!(!cache.cancel_queued_edit("d", "a.md").unwrap());
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    assert!(cache.cancel_queued_edit("d", "a.md").unwrap());
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_CACHED);
    assert!(cache.upload_entries("d").unwrap().is_empty());
}

#[test]
fn the_queue_rows_survive_a_rebuild_and_stay_one_per_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 5).unwrap();
    let id = cache.upload_entry("d", "a.md").unwrap().unwrap().id;
    cache
        .fail_upload_attempt(id, 0, 10, None, "offline")
        .unwrap();
    let before = cache.upload_entries("d").unwrap();
    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    assert_eq!(cache.upload_entries("d").unwrap(), before);
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_PENDING_UPLOAD);
    // The table itself refuses a second row for one file.
    let second = cache.connection().execute(
        "INSERT INTO upload_queue (source, path, created_at) VALUES ('d', 'a.md', 1)",
        [],
    );
    assert!(second.is_err());
}

#[test]
fn a_queued_or_held_file_is_never_dropped_by_the_eviction_claim() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    assert!(
        !cache
            .drop_remote_copy("d", "a.md", i64::MAX, false)
            .unwrap(),
        "pending_upload"
    );
    cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    assert!(
        !cache
            .drop_remote_copy("d", "a.md", i64::MAX, false)
            .unwrap(),
        "needs_attention"
    );
    assert_eq!(state_of(&cache, "a.md").0, "needs_attention");
}

#[test]
fn the_sending_marker_survives_failed_tries_and_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let cache = seeded(&dir);
        cache.queue_drive_edit("d", "a.md", 0).unwrap();
        let id = cache.upload_entry("d", "a.md").unwrap().unwrap().id;
        assert!(cache.mark_upload_sending(id, 0, "abc123").unwrap());
        assert!(
            !cache.mark_upload_sending(id, 5, "zzz").unwrap(),
            "a stale count marks nothing"
        );
        cache
            .fail_upload_attempt(id, 0, 10, None, "the answer was cut")
            .unwrap();
    }
    let cache = common::open_temp_cache(&dir);
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(row.sending_md5(), Some("abc123"));
    assert_eq!(
        row.last_error.as_deref(),
        Some("sending:abc123 the answer was cut")
    );
    // A second failure keeps it, and does not stack the text.
    cache
        .fail_upload_attempt(row.id, 1, 20, None, "5xx")
        .unwrap();
    let row = cache.upload_entry("d", "a.md").unwrap().unwrap();
    assert_eq!(row.last_error.as_deref(), Some("sending:abc123 5xx"));
    assert_eq!(row.attempts, 2);
}

#[test]
fn a_queued_edit_whose_file_is_gone_is_abandoned_and_the_item_is_remote_again() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    assert!(cache.abandon_queued_edit("d", "a.md").unwrap());
    assert_eq!(state_of(&cache, "a.md"), ("remote".to_string(), None));
    assert!(cache.upload_entries("d").unwrap().is_empty());
    assert_eq!(cache.remote_copy_stamp("d", "a.md").unwrap(), None);
    assert!(
        !cache.abandon_queued_edit("d", "a.md").unwrap(),
        "not pending any more: nothing to do"
    );
}

#[test]
fn drive_edit_candidates_leave_out_native_files_folders_and_items_being_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache
        .connection()
        .execute_batch(
            "INSERT INTO remote_items (source, path, kind, file_id, revision, size, state) VALUES
                ('d', 'doc.md', 'gdoc', 'id-d', 'v1', 5, 'cached'),
                ('d', 'sheet', 'gsheet', 'id-s', 'v1', 5, 'remote');",
        )
        .unwrap();
    let mut paths: Vec<String> = cache
        .drive_edit_candidates("d")
        .unwrap()
        .into_iter()
        .map(|(p, _, _)| p)
        .collect();
    paths.sort();
    assert_eq!(paths, ["a.md", "b.md", "p.md"]);
}

/// What a Drive poll does to an item: the listing's newest revision.
fn poll_moves_revision(cache: &Cache, path: &str, revision: &str) {
    cache
        .upsert_remote_item(
            "d",
            &tephra_cache::NewRemoteItem {
                path,
                kind: "prose",
                sha: "",
                size: Some(12),
                file_id: Some("id-x"),
                revision: Some(revision),
            },
        )
        .unwrap();
}

fn base_of(cache: &Cache, path: &str) -> Option<String> {
    cache
        .upload_entry("d", path)
        .unwrap()
        .unwrap()
        .base_revision
}

#[test]
fn a_poll_between_the_fetch_and_the_save_does_not_move_the_base_of_the_edit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    // The copy was fetched when Drive's head was rev-1.
    cache
        .record_remote_copy_revision("d", "a.md", Some("rev-1"))
        .unwrap();
    // Somebody changed the file on Drive and a poll noted the new head.
    poll_moves_revision(&cache, "a.md", "rev-2");
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 5).unwrap(),
        QueuedEdit::Queued
    );
    assert_eq!(base_of(&cache, "a.md").as_deref(), Some("rev-1"));
    let revision: String = cache
        .connection()
        .query_row(
            "SELECT revision FROM remote_items WHERE path = 'a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revision, "rev-2", "the item still shows Drive's head");
}

#[test]
fn an_evicted_file_saved_again_after_a_poll_is_queued_on_the_revision_it_was_read_at() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache
        .record_remote_copy_revision("d", "a.md", Some("rev-1"))
        .unwrap();
    // `a.md` is dropped from the cache (last used at 100), a poll sees a new
    // head, then the open editor saves the file anew.
    assert!(cache.drop_remote_copy("d", "a.md", 1000, false).unwrap());
    assert_eq!(state_of(&cache, "a.md").0, "remote");
    poll_moves_revision(&cache, "a.md", "rev-2");
    assert_eq!(
        cache.queue_drive_edit("d", "a.md", 5).unwrap(),
        QueuedEdit::Queued
    );
    assert_eq!(base_of(&cache, "a.md").as_deref(), Some("rev-1"));
}

#[test]
fn the_revision_of_a_copy_is_noted_by_a_landed_upload_and_forgotten_with_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    cache
        .record_remote_copy_revision("d", "a.md", Some("rev-1"))
        .unwrap();
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    assert!(cache
        .complete_upload(
            &UploadLanded {
                source: "d",
                path: "a.md",
                base_seen: Some("rev-1"),
                new_revision: "rev-5",
                size: 11,
                stamp: None,
                file_unchanged: true,
            },
            700,
        )
        .unwrap());
    assert_eq!(
        cache.remote_copy_revision("d", "a.md").unwrap().as_deref(),
        Some("rev-5")
    );
    // The next edit is based on what the upload made.
    poll_moves_revision(&cache, "a.md", "rev-6");
    cache.queue_drive_edit("d", "a.md", 800).unwrap();
    assert_eq!(base_of(&cache, "a.md").as_deref(), Some("rev-5"));
    // A listing that no longer has the file drops the note with the item.
    cache.delete_remote_item("d", "a.md").unwrap();
    assert_eq!(cache.remote_copy_revision("d", "a.md").unwrap(), None);
}

#[test]
fn the_revision_of_a_copy_follows_a_rename() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    let source_id = cache
        .register_source("d", "/tmp/d-root", tephra_cache::SourceStatus::Active)
        .unwrap();
    cache
        .record_remote_copy_revision("d", "a.md", Some("rev-1"))
        .unwrap();
    cache
        .rename_remote_items("d", source_id, &[("a.md".to_string(), "z.md".to_string())])
        .unwrap();
    assert_eq!(cache.remote_copy_revision("d", "a.md").unwrap(), None);
    assert_eq!(
        cache.remote_copy_revision("d", "z.md").unwrap().as_deref(),
        Some("rev-1")
    );
}

#[test]
fn a_held_conflict_survives_a_rebuild_and_a_restore_and_can_still_be_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = seeded(&dir);
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    cache
        .hold_upload(
            "d",
            "a.md",
            Some("rev-1"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    // A second file Drive turned down for good.
    let refused = format!("{UPLOAD_REFUSED_PREFIX}the file is read-only");
    cache.queue_drive_edit("d", "p.md", 0).unwrap();
    cache
        .hold_upload("d", "p.md", Some("rev-4"), &refused, None)
        .unwrap();
    // A reason that is not about an upload goes, as it always did.
    cache
        .connection()
        .execute(
            "UPDATE remote_items SET reason = 'fetch failed: offline' WHERE path = 'b.md'",
            [],
        )
        .unwrap();

    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    assert!(cache.restore_remote_units("d", 1_000_000).unwrap());

    assert_eq!(
        state_of(&cache, "a.md"),
        (
            "needs_attention".to_string(),
            Some(REASON_CHANGED_ON_DRIVE.to_string())
        )
    );
    assert_eq!(
        state_of(&cache, "p.md"),
        ("needs_attention".to_string(), Some(refused))
    );
    assert_eq!(state_of(&cache, "b.md").1, None);
    // `resolve` still finds the conflict, with the head it saw.
    let conflict = cache
        .drive_conflict("d", "a.md")
        .unwrap()
        .expect("conflict");
    assert_eq!(conflict.head.as_deref(), Some("rev-9"));
    assert!(cache.claim_conflict_for_mine(&conflict).unwrap());
    assert_eq!(state_of(&cache, "a.md").0, ITEM_STATE_PENDING_UPLOAD);
    assert_eq!(base_of(&cache, "a.md").as_deref(), Some("rev-9"));
}

#[test]
fn the_holds_of_a_source_count_pending_and_held_files_and_name_the_held_ones() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert_eq!(
        cache.drive_holds("d").unwrap(),
        tephra_cache::DriveHolds::default()
    );
    cache.queue_drive_edit("d", "a.md", 0).unwrap();
    cache.queue_drive_edit("d", "p.md", 0).unwrap();
    cache
        .hold_upload(
            "d",
            "p.md",
            Some("rev-4"),
            REASON_CHANGED_ON_DRIVE,
            Some("rev-9"),
        )
        .unwrap();
    let holds = cache.drive_holds("d").unwrap();
    assert_eq!((holds.pending_upload, holds.needs_attention), (1, 1));
    assert_eq!(
        holds.attention,
        vec![("p.md".to_string(), REASON_CHANGED_ON_DRIVE.to_string())]
    );
    assert_eq!(cache.drive_holds("other").unwrap().pending_upload, 0);
}
