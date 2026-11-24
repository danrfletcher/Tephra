//! PR-11 (G13): the state precedence table, needs_attention > pending_upload
//! / not_pushed > downloading > pinned > cached > remote. Table-driven over
//! every start state and every event that is not the edit's own way out
//! (`resolve` and a landed upload have their tests in `drive_resolve.rs` and
//! `drive_upload.rs`): the cache's own steps are run on a real SQLite file
//! and the state read back, and then the table is checked against the rank
//! itself, so a cell cannot say a lower state beat a higher one. A few rows
//! run end to end on a real daemon.
#![allow(dead_code)]

mod remote_support;

use remote_support::{save, DriveDaemon};
use serde_json::json;
use tephra_cache::{
    Cache, NewRemoteItem, UploadLanded, ITEM_STATE_CACHED, ITEM_STATE_DOWNLOADING,
    ITEM_STATE_NEEDS_ATTENTION, ITEM_STATE_NOT_PUSHED, ITEM_STATE_PENDING_UPLOAD,
    ITEM_STATE_PINNED, ITEM_STATE_REMOTE, REASON_CHANGED_ON_DRIVE, UPLOAD_REFUSED_PREFIX,
};

/// The precedence table: a higher number wins when two causes apply.
fn rank(state: &str) -> u8 {
    match state {
        s if s == ITEM_STATE_NEEDS_ATTENTION => 5,
        s if s == ITEM_STATE_PENDING_UPLOAD || s == ITEM_STATE_NOT_PUSHED => 4,
        s if s == ITEM_STATE_DOWNLOADING => 3,
        s if s == ITEM_STATE_PINNED => 2,
        s if s == ITEM_STATE_CACHED => 1,
        s if s == ITEM_STATE_REMOTE => 0,
        // `missing` is its own thing (the file is gone from the remote).
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Start {
    Remote,
    Downloading,
    Cached,
    Pinned,
    PendingUpload,
    NeedsAttentionConflict,
    NeedsAttentionRefused,
    NotPushed,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Event {
    /// The person saves the file (the edit is seen).
    Save,
    /// A fetch asks for the file.
    FetchBegins,
    /// A poll sees a new revision of the file on Drive.
    PollSeesNewRevision,
    /// Eviction takes the copy.
    EvictionClaim,
    /// A download finishes.
    DownloadFinishes,
    /// A download fails.
    DownloadFails,
    /// The worker finds Drive's head moved under the edit.
    ConflictFound,
}

const STARTS: [Start; 9] = [
    Start::Remote,
    Start::Downloading,
    Start::Cached,
    Start::Pinned,
    Start::PendingUpload,
    Start::NeedsAttentionConflict,
    Start::NeedsAttentionRefused,
    Start::NotPushed,
    Start::Missing,
];

const EVENTS: [Event; 7] = [
    Event::Save,
    Event::FetchBegins,
    Event::PollSeesNewRevision,
    Event::EvictionClaim,
    Event::DownloadFinishes,
    Event::DownloadFails,
    Event::ConflictFound,
];

/// What each start state becomes under each event, in the order of
/// [`EVENTS`]. Written out in full on purpose: a change of the rules is a
/// change of this table.
fn expected(start: Start) -> [&'static str; 7] {
    const R: &str = ITEM_STATE_REMOTE;
    const D: &str = ITEM_STATE_DOWNLOADING;
    const C: &str = ITEM_STATE_CACHED;
    const P: &str = ITEM_STATE_PINNED;
    const U: &str = ITEM_STATE_PENDING_UPLOAD;
    const N: &str = ITEM_STATE_NEEDS_ATTENTION;
    //                     Save  Fetch Poll  Evict Done  Fail  Conflict
    match start {
        // A file dropped from the cache and saved again is an edit (E24).
        Start::Remote => [U, D, R, R, R, R, R],
        // Not queued while it downloads: the scan finds the edit once the
        // download is done (checked below).
        Start::Downloading => [D, D, D, D, C, R, D],
        // Eviction took it: it is `remote`, the only case where a lower
        // state wins, because nothing outranks it.
        Start::Cached => [U, C, C, R, C, C, C],
        Start::Pinned => [U, P, P, P, P, P, P],
        // pending_upload beats downloading, pinned, cached and remote:
        // nothing but a conflict moves it.
        Start::PendingUpload => [U, U, U, U, U, U, N],
        // needs_attention beats everything, the save included.
        Start::NeedsAttentionConflict => [N, N, N, N, N, N, N],
        // A Drive refusal is lifted by a save of new bytes, and by nothing
        // else (a save is a new try; the rule is in `queue_drive_edit`).
        Start::NeedsAttentionRefused => [U, N, N, N, N, N, N],
        // GitHub's `not_pushed` has the rank of `pending_upload`; Drive's
        // edit worker never touches it.
        Start::NotPushed => [
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
            ITEM_STATE_NOT_PUSHED,
        ],
        Start::Missing => ["missing"; 7],
    }
}

/// A cache with one Drive source and one item `a.md` in `start`.
fn seeded(start: Start) -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(dir.path().join("cache.db")).unwrap();
    let db = cache.connection();
    db.execute_batch(
        "INSERT INTO remote_sources (name, kind, target, created_at) \
         VALUES ('d', 'drive', 'folder-1', 1);",
    )
    .unwrap();
    let state = match start {
        Start::Remote => "remote",
        Start::Downloading => "downloading",
        Start::Cached | Start::PendingUpload => "cached",
        Start::Pinned => "pinned",
        Start::NeedsAttentionConflict | Start::NeedsAttentionRefused => "cached",
        Start::NotPushed => "not_pushed",
        Start::Missing => "missing",
    };
    db.execute(
        "INSERT INTO remote_items (source, path, kind, file_id, revision, size, state, last_used) \
         VALUES ('d', 'a.md', 'prose', 'id-a', 'rev-1', 10, ?1, 100)",
        [state],
    )
    .unwrap();
    if start == Start::Pinned {
        db.execute(
            "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('d', 'a.md', 0, 1)",
            [],
        )
        .unwrap();
    }
    if matches!(
        start,
        Start::PendingUpload | Start::NeedsAttentionConflict | Start::NeedsAttentionRefused
    ) {
        cache.queue_drive_edit("d", "a.md", 5).unwrap();
    }
    match start {
        Start::NeedsAttentionConflict => {
            assert!(cache
                .hold_upload(
                    "d",
                    "a.md",
                    Some("rev-1"),
                    REASON_CHANGED_ON_DRIVE,
                    Some("rev-9")
                )
                .unwrap());
        }
        Start::NeedsAttentionRefused => {
            assert!(cache
                .hold_upload(
                    "d",
                    "a.md",
                    Some("rev-1"),
                    &format!("{UPLOAD_REFUSED_PREFIX}Drive says no"),
                    None,
                )
                .unwrap());
        }
        _ => {}
    }
    (dir, cache)
}

fn state(cache: &Cache) -> String {
    cache
        .connection()
        .query_row(
            "SELECT state FROM remote_items WHERE source = 'd' AND path = 'a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn apply(cache: &Cache, event: Event) {
    match event {
        Event::Save => {
            cache.queue_drive_edit("d", "a.md", 9).unwrap();
        }
        Event::FetchBegins => {
            cache.begin_remote_download("d", "a.md").unwrap();
        }
        Event::PollSeesNewRevision => {
            cache
                .upsert_remote_item(
                    "d",
                    &NewRemoteItem {
                        path: "a.md",
                        kind: "prose",
                        sha: "",
                        size: Some(11),
                        file_id: Some("id-a"),
                        revision: Some("rev-2"),
                    },
                )
                .unwrap();
        }
        Event::EvictionClaim => {
            cache.drop_remote_copy("d", "a.md", 1_000, false).unwrap();
        }
        Event::DownloadFinishes => {
            cache
                .finish_remote_download("d", "a.md", None, Some(10), 50)
                .unwrap();
        }
        Event::DownloadFails => {
            cache
                .fail_remote_download("d", "a.md", "the network went")
                .unwrap();
        }
        Event::ConflictFound => {
            cache
                .hold_upload(
                    "d",
                    "a.md",
                    Some("rev-1"),
                    REASON_CHANGED_ON_DRIVE,
                    Some("rev-9"),
                )
                .unwrap();
        }
    }
}

#[test]
fn every_state_under_every_event_is_what_the_table_says() {
    let mut wrong = Vec::new();
    for start in STARTS {
        for (event, want) in EVENTS.iter().zip(expected(start)) {
            let (_dir, cache) = seeded(start);
            apply(&cache, *event);
            let got = state(&cache);
            if got != want {
                wrong.push(format!(
                    "{start:?} + {event:?}: got {got}, table says {want}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The table itself obeys the rank: a lower cause never takes a state off a
/// higher one. Two causes of the same row are the only ways down, and both
/// are named.
#[test]
fn the_table_never_lets_a_lower_state_beat_a_higher_one() {
    for start in STARTS {
        let before = match start {
            Start::Remote => ITEM_STATE_REMOTE,
            Start::Downloading => ITEM_STATE_DOWNLOADING,
            Start::Cached => ITEM_STATE_CACHED,
            Start::Pinned => ITEM_STATE_PINNED,
            Start::PendingUpload => ITEM_STATE_PENDING_UPLOAD,
            Start::NeedsAttentionConflict | Start::NeedsAttentionRefused => {
                ITEM_STATE_NEEDS_ATTENTION
            }
            Start::NotPushed => ITEM_STATE_NOT_PUSHED,
            Start::Missing => "missing",
        };
        for (event, after) in EVENTS.iter().zip(expected(start)) {
            if rank(before) < rank(ITEM_STATE_PENDING_UPLOAD) {
                continue; // The pairs that matter start at pending_upload.
            }
            let allowed_up = *event == Event::ConflictFound
                && before == ITEM_STATE_PENDING_UPLOAD
                && after == ITEM_STATE_NEEDS_ATTENTION;
            let allowed_reopen = *event == Event::Save
                && start == Start::NeedsAttentionRefused
                && after == ITEM_STATE_PENDING_UPLOAD;
            assert!(
                after == before || allowed_up || allowed_reopen,
                "{start:?} + {event:?} -> {after}: a state of rank {} moved by something of lower rank",
                rank(before)
            );
        }
    }
    // The other direction: what a save or a conflict makes beats what was.
    for lower in [
        ITEM_STATE_REMOTE,
        ITEM_STATE_CACHED,
        ITEM_STATE_PINNED,
        ITEM_STATE_DOWNLOADING,
    ] {
        assert!(rank(ITEM_STATE_PENDING_UPLOAD) > rank(lower), "{lower}");
    }
    assert!(rank(ITEM_STATE_NEEDS_ATTENTION) > rank(ITEM_STATE_PENDING_UPLOAD));
    assert_eq!(rank(ITEM_STATE_NOT_PUSHED), rank(ITEM_STATE_PENDING_UPLOAD));
}

#[test]
fn an_edit_of_a_file_that_was_downloading_is_queued_once_the_download_is_done() {
    let (_dir, cache) = seeded(Start::Downloading);
    cache.queue_drive_edit("d", "a.md", 9).unwrap();
    assert_eq!(state(&cache), ITEM_STATE_DOWNLOADING);
    assert!(cache.upload_entry("d", "a.md").unwrap().is_none());
    assert!(cache
        .finish_remote_download("d", "a.md", None, Some(10), 50)
        .unwrap());
    cache.queue_drive_edit("d", "a.md", 10).unwrap();
    assert_eq!(state(&cache), ITEM_STATE_PENDING_UPLOAD);
    assert_eq!(
        cache
            .upload_entry("d", "a.md")
            .unwrap()
            .unwrap()
            .base_revision
            .as_deref(),
        Some("rev-1")
    );
}

/// A file that was pinned is pinned again once its edit has landed: the pin
/// is not lost while `pending_upload` outranks it.
#[test]
fn a_pinned_file_goes_back_to_pinned_when_its_edit_has_landed() {
    let (_dir, cache) = seeded(Start::Pinned);
    cache.queue_drive_edit("d", "a.md", 9).unwrap();
    assert_eq!(state(&cache), ITEM_STATE_PENDING_UPLOAD);
    assert!(cache
        .complete_upload(
            &UploadLanded {
                source: "d",
                path: "a.md",
                base_seen: Some("rev-1"),
                new_revision: "rev-2",
                size: 12,
                stamp: Some("sha"),
                file_unchanged: true,
            },
            10,
        )
        .unwrap());
    assert_eq!(state(&cache), ITEM_STATE_PINNED);
}

// ---- end to end ----

#[test]
fn a_poll_that_sees_drive_change_under_a_queued_edit_leaves_it_pending_with_its_base() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let fetched_at = dd.row("Plan.md").revision;
    // The upload cannot go for now, so the edit stays queued.
    dd.drive.fail_uploads(1000, 503);
    save(&file, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    dd.poll();
    // The poll updates what Drive has, never the edit: it is not `cached` or
    // `remote` after it, and its base is still the one the copy was fetched
    // at. (The poll moved the fake clock past the retry wait, so the worker
    // may already have looked at Drive's head: then it is held.)
    let row = dd.row("Plan.md");
    assert!(
        row.state == "pending_upload" || row.state == "needs_attention",
        "the poll did not lower it: {row:?}"
    );
    assert_ne!(
        row.revision, fetched_at,
        "the poll did record Drive's change"
    );
    assert_eq!(dd.queue()[0].base_revision, fetched_at);
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.queue()[0].base_revision, fetched_at);
    assert_eq!(dd.drive.upload_count("p1"), 0, "no byte was sent");
    assert_eq!(std::fs::read(&file).unwrap(), b"# Plan\nmine\n");
    dd.d.close();
}

#[test]
fn a_fetch_of_a_pending_file_serves_the_edit_and_downloads_nothing() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1000, 503);
    save(&file, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    let reads = dd.media_reads();
    let again = dd.fetch_file("Plan.md");
    assert_eq!(again, file);
    assert_eq!(
        std::fs::read(&again).unwrap(),
        b"# Plan\nmine\n",
        "the edit, not Drive's"
    );
    assert_eq!(dd.media_reads(), reads, "nothing was downloaded over it");
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    dd.d.close();
}

#[test]
fn a_held_file_stays_held_through_a_poll_a_fetch_and_a_new_save_of_the_same_edit() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    save(&file, b"# Plan\nmine\n");
    dd.wait_state("Plan.md", "needs_attention");

    dd.drive.update_file("p1", b"# Plan\nsomeone else, again\n");
    dd.poll();
    assert_eq!(dd.row("Plan.md").state, "needs_attention");
    assert_eq!(
        dd.row("Plan.md").reason.as_deref(),
        Some("changed on Drive")
    );

    let reads = dd.media_reads();
    let fetched = dd.d.rpc("unit.fetch", json!({"unit": "@gdrive:Plan.md"}));
    assert!(fetched.get("result").is_some() || fetched.get("error").is_some());
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"# Plan\nmine\n",
        "never overwritten"
    );
    assert_eq!(dd.media_reads(), reads);
    assert_eq!(dd.row("Plan.md").state, "needs_attention");

    save(&file, b"# Plan\nmine, once more\n");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(
        dd.row("Plan.md").state,
        "needs_attention",
        "a save does not lift a conflict"
    );
    assert_eq!(dd.drive.upload_count("p1"), 0);
    dd.d.close();
}
