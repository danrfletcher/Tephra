//! PR-11 (G13, G14, GP7, E3, E14, E24, E2): Dan's edits to cached Drive files
//! go back to Drive as new revisions. A saved edit is queued on the revision
//! the copy was fetched at; right before the bytes go the worker reads the
//! head, and a head that moved refuses the upload. Real registry, RPC server
//! and watcher against the PR-3 fake Drive on a fake clock: nothing sleeps for
//! a backoff.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::{item_rows, save, DriveDaemon, GOOGLE_TOKEN};
use serde_json::json;
use tephra_cache::upload_backoff_secs;
use tephra_remote::testing::{DriveLimit, UploadHold};

const MD5_OF_NOTHING: &str = "d41d8cd98f00b204e9800998ecf8427e";

fn queue_rows_of(dd: &DriveDaemon, path: &str) -> usize {
    dd.queue().iter().filter(|e| e.path == path).count()
}

#[test]
fn gp7_a_saved_edit_is_pending_then_a_new_revision_on_drive_and_cached_again() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    assert_eq!(dd.row("Plan.md").state, "cached");
    let first = dd.drive.head_revision("p1");
    let after_fetch = dd.media_reads();

    // The upload is held at Drive, so the pending state can be looked at.
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"# Plan\nedited by Dan\n");
    let pending = dd.wait_state("Plan.md", "pending_upload");
    assert_eq!(pending.reason, None);
    let queued = dd.queue();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].path, "Plan.md");
    assert_eq!(
        queued[0].base_revision.as_deref(),
        Some(first.as_str()),
        "based on the revision the copy was fetched at"
    );

    // `unit.list` shows it, and shows it without reading anything.
    let reads = dd.media_reads();
    assert_eq!(reads, after_fetch, "saving read no body");
    let entries = dd.d.entries("gdrive");
    let plan = entries
        .iter()
        .find(|e| e["rel_path"] == json!("Plan.md"))
        .expect("Plan.md is listed");
    assert_eq!(plan["state"], json!("pending_upload"), "{plan}");
    assert_eq!(dd.media_reads(), reads);
    // So does `source.list`, with a count and nothing else.
    let list = dd.d.ok("source.list", json!({}));
    let row = list
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == json!("gdrive")))
        .unwrap_or_else(|| panic!("no gdrive row in {list}"));
    assert_eq!(row["pending_upload"], json!(1), "{row}");
    assert!(row.get("needs_attention").is_none(), "{row}");
    assert_eq!(dd.media_reads(), reads);

    dd.drive.release_uploads();
    let done = dd.wait_state("Plan.md", "cached");
    let head = dd.drive.head_revision("p1");
    assert_ne!(head, first, "Drive has a new revision");
    assert_eq!(dd.drive.upload_count("p1"), 1);
    assert_eq!(dd.drive.content("p1"), b"# Plan\nedited by Dan\n");
    assert_eq!(
        done.revision.as_deref(),
        Some(head.as_str()),
        "the new revision is the item's"
    );
    assert!(dd.queue().is_empty(), "the queue is empty again");
    assert_eq!(dd.media_reads(), reads, "no file body was read to do it");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"# Plan\nedited by Dan\n",
        "the local bytes are untouched"
    );
    // Only the one write there is: a media PATCH of that one file.
    assert_eq!(
        dd.writes(),
        vec!["PATCH /upload/drive/v3/files/p1".to_string()]
    );
    dd.d.close();
}

#[test]
fn a_second_save_while_queued_sends_the_newest_bytes_from_the_original_base() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"one\n");
    dd.wait_state("Plan.md", "pending_upload");
    dd.d.wait_for("the upload to reach Drive", || {
        (dd.drive.uploads_held() == 1).then_some(())
    });
    // A save while the upload is in flight.
    save(&file, b"two\n");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(queue_rows_of(&dd, "Plan.md"), 1, "never two rows");
    assert_eq!(
        dd.queue()[0].base_revision.as_deref(),
        Some(first.as_str()),
        "the base is the first one"
    );
    dd.drive.release_uploads();
    // The upload that was in flight carried "one"; the save after it is an
    // edit of its own, based on what that upload made.
    dd.d.wait_for("Drive to hold the newest bytes", || {
        (dd.drive.content("p1") == b"two\n").then_some(())
    });
    dd.wait_state("Plan.md", "cached");
    assert_eq!(std::fs::read(&file).unwrap(), b"two\n");
    assert!(dd.queue().is_empty());
    assert!(dd.drive.upload_count("p1") <= 2);
    dd.d.close();
}

#[test]
fn a_second_save_before_the_upload_starts_is_one_row_and_one_upload() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    // The first try fails, so the row waits for its backoff.
    dd.drive.fail_uploads(1, 503);
    save(&file, b"one\n");
    let row = dd.wait_attempts("Plan.md", 1);
    assert_eq!(row.base_revision.as_deref(), Some(first.as_str()));
    save(&file, b"two\n");
    std::thread::sleep(Duration::from_millis(700));
    let rows = dd.queue();
    assert_eq!(rows.len(), 1, "never two rows");
    assert_eq!(rows[0].id, row.id, "the same row");
    assert_eq!(rows[0].base_revision.as_deref(), Some(first.as_str()));
    assert_eq!(rows[0].attempts, 1, "the wait is not reset by a save");
    dd.advance(Duration::from_secs(2));
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.content("p1"), b"two\n");
    assert_eq!(
        dd.drive.upload_count("p1"),
        1,
        "one upload, the newest bytes"
    );
    dd.d.close();
}

#[test]
fn a_save_of_the_bytes_drive_has_sends_nothing_and_makes_no_revision() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    // Saved again with the very same bytes: not an edit.
    let same = std::fs::read(&file).unwrap();
    save(&file, &same);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(dd.row("Plan.md").state, "cached");
    assert!(dd.queue().is_empty());
    assert!(dd.writes().is_empty());
    assert_eq!(dd.drive.head_revision("p1"), first);

    // After an upload, saving the uploaded bytes again is not an edit either.
    save(&file, b"changed\n");
    dd.wait_state("Plan.md", "cached");
    dd.d.wait_for("the upload", || {
        (dd.drive.upload_count("p1") == 1).then_some(())
    });
    let after = dd.drive.head_revision("p1");
    save(&file, b"changed\n");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(dd.drive.upload_count("p1"), 1);
    assert_eq!(dd.drive.head_revision("p1"), after);
    assert_eq!(dd.row("Plan.md").state, "cached");
    dd.d.close();
}

#[test]
fn an_edit_put_back_to_the_bytes_drive_has_while_queued_makes_no_revision() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    let original = std::fs::read(&file).unwrap();
    dd.drive.fail_uploads(1, 503);
    save(&file, b"oops\n");
    dd.wait_attempts("Plan.md", 1);
    // Dan undoes it before the retry.
    save(&file, &original);
    dd.advance(Duration::from_secs(2));
    dd.wait_state("Plan.md", "cached");
    assert!(dd.queue().is_empty(), "the entry completed");
    assert_eq!(dd.drive.head_revision("p1"), first, "no revision");
    assert_eq!(dd.drive.upload_count("p1"), 0, "nothing was applied");
    assert_eq!(dd.writes().len(), 1, "only the try that was refused");
    dd.d.close();
}

#[test]
fn a_zero_byte_edit_uploads_the_empty_file() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let after_fetch = dd.media_reads();
    save(&file, b"");
    dd.d.wait_for("the empty upload", || {
        (dd.drive.upload_count("p1") == 1).then_some(())
    });
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.content("p1"), b"");
    assert_eq!(dd.media_reads(), after_fetch, "the fetch only");
    let _ = MD5_OF_NOTHING;
    dd.d.close();
}

#[test]
fn a_file_of_twenty_megabytes_and_more_uploads_whole() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let big: Vec<u8> = (0..21_000_000u32).map(|i| (i % 253) as u8).collect();
    save(&file, &big);
    // A file this size is written in pieces, and a person's editor does the
    // same: what is left on Drive is the finished file, however many
    // pieces were seen on the way.
    dd.d.wait_for("the big upload", || {
        (dd.drive.content("p1") == big).then_some(())
    });
    dd.wait_state("Plan.md", "cached");
    assert!(dd.drive.content("p1") == big, "Drive holds every byte");
    assert_eq!(dd.row("Plan.md").size, Some(big.len() as i64));
    dd.d.close();
}

// ---- the revision check ---------------------------------------------------------------

#[test]
fn e3_a_file_changed_on_drive_under_an_edit_is_refused_and_no_bytes_are_sent() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    // Drive moves on after the copy was fetched; Dan saves his edit later.
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    let theirs = dd.drive.head_revision("p1");
    save(&file, b"# Plan\nmine\n");
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0, "no bytes were sent");
    assert!(dd.writes().is_empty(), "no write request was made");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsomeone else\n");
    assert_eq!(dd.drive.head_revision("p1"), theirs);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"# Plan\nmine\n",
        "Dan's bytes are kept"
    );
    assert_eq!(dd.queue().len(), 1, "the entry stays for resolve");
    // `source list` and `unit.list` say so.
    let entries = dd.d.entries("gdrive");
    let plan = entries
        .iter()
        .find(|e| e["rel_path"] == json!("Plan.md"))
        .expect("listed");
    assert_eq!(plan["state"], json!("needs_attention"), "{plan}");
    assert_eq!(plan["reason"], json!("changed on Drive"), "{plan}");
    // A poll does not wipe the hold (the feed saw the change too).
    dd.poll();
    let after = dd.row("Plan.md");
    assert_eq!(after.state, "needs_attention");
    assert_eq!(after.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    dd.d.close();
}

#[test]
fn e3_a_poll_after_the_change_on_drive_and_before_the_save_does_not_hide_it() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let fetched_at = dd.row("Plan.md").revision;
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    let theirs = dd.drive.head_revision("p1");
    // The poll that runs every minute sees the new head and notes it on the
    // item; it does not refresh the cached copy.
    dd.poll();
    assert_eq!(dd.row("Plan.md").revision.as_deref(), Some(theirs.as_str()));
    assert_ne!(dd.row("Plan.md").revision, fetched_at);
    save(&file, b"# Plan\nmine\n");
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0, "no bytes were sent");
    assert!(dd.writes().is_empty(), "no write request was made");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsomeone else\n");
    assert_eq!(std::fs::read(&file).unwrap(), b"# Plan\nmine\n");
    dd.d.close();
}

#[test]
fn e3_a_change_made_between_the_edit_and_the_retry_is_caught_too() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1, 503);
    save(&file, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    // Between the edit and the retry.
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    dd.advance(Duration::from_secs(2));
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    assert_eq!(
        dd.writes().len(),
        1,
        "only the first try, which was refused"
    );
    dd.d.close();
}

#[test]
fn a_native_google_doc_is_never_queued() {
    let dd = DriveDaemon::start_edits();
    // Nothing of a Doc is on disk; a stray file at its place is not an edit.
    let doc = item_rows(&dd.d.cache_path, "gdrive")
        .into_iter()
        .find(|r| r.path.starts_with("Doc"))
        .expect("the Doc is listed");
    assert_eq!(doc.state, "remote");
    let stray = dd.root().join(&doc.path);
    std::fs::create_dir_all(dd.root()).unwrap();
    save(&stray, b"not an edit\n");
    std::thread::sleep(Duration::from_millis(800));
    assert!(dd.queue().is_empty());
    assert!(dd.writes().is_empty());
    assert_eq!(
        dd.row(&doc.path).state,
        "remote",
        "still a Doc, not an edit"
    );
    dd.d.close();
}

// ---- failures --------------------------------------------------------------------------

#[test]
fn transient_failures_retry_with_a_doubling_wait_capped_at_sixty_seconds_for_ever() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_next(1000, 503);
    save(&file, b"# Plan\nmine\n");
    // 1, 2, 4, 8, 16, 32, 60, 60, 60, 60 ... and no limit.
    for tries in 1..=12u32 {
        let row = dd.wait_attempts("Plan.md", tries);
        let now = dd.now_secs();
        let wait = upload_backoff_secs(tries - 1);
        assert_eq!(
            row.next_retry_at,
            Some(now + wait as i64),
            "after {tries} tries the next is {wait} s away"
        );
        assert!(wait <= 60);
        assert_eq!(dd.row("Plan.md").state, "pending_upload", "never given up");
        // Not before it is due: the clock is moved by less than the wait.
        if wait > 1 {
            dd.advance(Duration::from_secs(wait - 1));
            std::thread::sleep(Duration::from_millis(120));
            assert_eq!(dd.queue()[0].attempts, tries, "not tried early");
            dd.advance(Duration::from_secs(1));
        } else {
            dd.advance(Duration::from_secs(1));
        }
    }
    assert_eq!(upload_backoff_secs(6), 60);
    assert_eq!(upload_backoff_secs(11), 60);
    let error = dd.queue()[0].last_error.clone().unwrap_or_default();
    assert!(
        !error.contains(GOOGLE_TOKEN),
        "no token in the queue: {error}"
    );
    dd.d.close();
}

#[test]
fn the_retry_schedule_is_stored_and_survives_a_restart() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_next(1000, 503);
    save(&file, b"# Plan\nmine\n");
    for tries in 1..=3u32 {
        dd.wait_attempts("Plan.md", tries);
        dd.advance(Duration::from_secs(upload_backoff_secs(tries - 1)));
    }
    let before = dd.wait_attempts("Plan.md", 4);
    assert_eq!(before.next_retry_at, Some(dd.now_secs() + 8));
    let requests = dd.drive.request_log().count();

    let dd = dd.restart();
    std::thread::sleep(Duration::from_millis(500));
    let after = dd.queue().remove(0);
    assert_eq!(after.attempts, 4, "the count is kept");
    assert_eq!(after.next_retry_at, before.next_retry_at, "so is the time");
    assert_eq!(
        dd.drive.request_log().count(),
        requests,
        "nothing is tried before it is due"
    );
    assert_eq!(dd.row("Plan.md").state, "pending_upload");

    // Due again: the schedule goes on from 4, it does not start over.
    dd.advance(Duration::from_secs(8));
    let next = dd.wait_attempts("Plan.md", 5);
    assert_eq!(next.next_retry_at, Some(dd.now_secs() + 16));
    dd.d.close();
}

#[test]
fn a_rate_limit_is_waited_for_by_its_own_time_and_the_edit_then_goes_up() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.rate_limit_for(
        Duration::from_secs(300),
        DriveLimit::Status429 { retry_after: true },
    );
    save(&file, b"# Plan\nmine\n");
    let row = dd.wait_attempts("Plan.md", 1);
    let wait = row.next_retry_at.expect("a retry time") - dd.now_secs();
    assert!(wait >= 1, "at least the backoff: {wait}");
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    dd.drive.clear_rate_limit();
    dd.advance(Duration::from_secs(400));
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.upload_count("p1"), 1);
    dd.d.close();
}

#[test]
fn e2_a_rejected_token_keeps_the_edit_and_it_goes_up_after_signing_in_again() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.revoke_token();
    save(&file, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    let row = dd.row("Plan.md");
    assert_eq!(row.state, "pending_upload", "the edit queues: {row:?}");
    assert_eq!(dd.drive.upload_count("p1"), 0);
    assert_eq!(dd.queue().len(), 1, "the queue is kept");

    dd.drive.accept_only_token("ya29.SECOND_sentinel");
    dd.sign_in_again("ya29.SECOND_sentinel");
    dd.advance(Duration::from_secs(120));
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.upload_count("p1"), 1);
    let sent = dd.drive.request_log().matching("/upload/drive/v3/files/p1");
    assert_eq!(
        sent.last().and_then(|r| r.header("authorization")),
        Some("Bearer ya29.SECOND_sentinel")
    );
    dd.d.close();
}

#[test]
fn a_file_drive_will_not_take_is_held_with_drives_reason_and_never_retried() {
    for (status, word) in [(404u16, "not found"), (403, "may not write")] {
        let dd = DriveDaemon::start_edits();
        let file = dd.fetch_file("Plan.md");
        // The check goes through; the upload itself is refused.
        let head_reads = dd.drive.request_log().matching("/files/p1").len();
        let _ = head_reads;
        dd.drive.fail_uploads(1, status);
        save(&file, b"# Plan\nmine\n");
        let held = dd.wait_state("Plan.md", "needs_attention");
        let reason = held.reason.clone().unwrap_or_default();
        assert!(
            reason.starts_with("Drive refused the upload: "),
            "{status}: {reason}"
        );
        assert!(reason.contains(word), "{reason}");
        assert!(!reason.contains(GOOGLE_TOKEN));
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"# Plan\nmine\n",
            "the local bytes are kept"
        );
        // Not a transient error: no retry, however long it waits.
        let sent = dd.drive.request_log().matching("/upload/").len();
        dd.advance(Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(dd.drive.request_log().matching("/upload/").len(), sent);
        assert_eq!(dd.row("Plan.md").state, "needs_attention");
        assert_eq!(dd.queue().len(), 1);
        dd.d.close();
    }
}

// ---- E24: a file the cache dropped, saved again --------------------------------------------

#[test]
fn e24_a_file_evicted_while_open_and_saved_again_is_an_edit_on_the_recorded_revision() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    // The cache drops the copy (what an eviction leaves): row `remote`, no
    // stamp, no file.
    remote_support::exec_sql(
        &dd.d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'gdrive' AND path = 'Plan.md'; \
         DELETE FROM remote_copy_stamps WHERE source = 'gdrive' AND path = 'Plan.md';",
    );
    std::fs::remove_file(&file).unwrap();
    // Obsidian still has it open and saves: the file is back.
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"# Plan\nsaved after eviction\n");
    let pending = dd.wait_state("Plan.md", "pending_upload");
    assert_eq!(pending.reason, None);
    assert_eq!(
        dd.queue()[0].base_revision.as_deref(),
        Some(first.as_str()),
        "on the last recorded revision"
    );
    dd.drive.release_uploads();
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsaved after eviction\n");
    dd.d.close();
}

#[test]
fn e24_the_re_created_file_is_refused_when_the_remote_moved() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    remote_support::exec_sql(
        &dd.d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'gdrive' AND path = 'Plan.md'; \
         DELETE FROM remote_copy_stamps WHERE source = 'gdrive' AND path = 'Plan.md';",
    );
    std::fs::remove_file(&file).unwrap();
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    save(&file, b"# Plan\nsaved after eviction\n");
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    dd.d.close();
}

#[test]
fn e24_a_poll_between_the_eviction_and_the_save_does_not_hide_the_change_on_drive() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    // The cache drops the copy the way eviction does: the file goes, the
    // item is `remote` and its stamp is forgotten.
    remote_support::exec_sql(
        &dd.d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'gdrive' AND path = 'Plan.md'; \
         DELETE FROM remote_copy_stamps WHERE source = 'gdrive' AND path = 'Plan.md';",
    );
    std::fs::remove_file(&file).unwrap();
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    dd.poll();
    save(&file, b"# Plan\nsaved after eviction\n");
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0, "no bytes were sent");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsomeone else\n");
    dd.d.close();
}

#[test]
fn eviction_never_removes_a_file_that_is_pending_upload_or_needs_attention() {
    let dd = DriveDaemon::start_edits();
    let plan = dd.fetch_file("Plan.md");
    let other = dd.fetch_file("Other.md");
    // Both unsent: one is waiting for its retry, one is held.
    dd.drive.fail_uploads(1000, 503);
    save(&plan, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    dd.drive.update_file("o1", b"# Other\nchanged\n");
    save(&other, b"# Other\nmine\n");
    dd.d.wait_for("Other.md to be held", || {
        item_rows(&dd.d.cache_path, "gdrive")
            .iter()
            .any(|r| r.path == "Other.md" && r.state == "needs_attention")
            .then_some(())
    });
    // Days pass with the cache over its cap.
    dd.d.ok("cache.set_limit", json!({ "limit": "0" }));
    dd.advance(Duration::from_secs(86_400));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(std::fs::read(&plan).unwrap(), b"# Plan\nmine\n");
    assert_eq!(std::fs::read(&other).unwrap(), b"# Other\nmine\n");
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    assert_eq!(dd.row("Other.md").state, "needs_attention");
    dd.d.close();
}

// ---- E14: the daemon is killed around the Drive call -----------------------------------------

#[test]
fn e14_killed_before_the_bytes_are_applied_the_entry_stays_and_uploads_once_after_restart() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"# Plan\nmine\n");
    dd.d.wait_for("the request to be at Drive", || {
        (dd.drive.uploads_held() == 1).then_some(())
    });
    assert_eq!(dd.drive.upload_count("p1"), 0);

    let dd = dd.restart();
    // The held request died with the daemon: letting the fake go changes
    // nothing for it.
    dd.drive.release_uploads();
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.upload_count("p1"), 1, "uploaded exactly once");
    assert_ne!(dd.drive.head_revision("p1"), first);
    assert_eq!(dd.drive.content("p1"), b"# Plan\nmine\n");
    assert!(dd.queue().is_empty());
    dd.d.close();
}

#[test]
fn e14_killed_after_the_bytes_landed_the_retry_completes_with_no_second_revision() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    dd.drive.hold_uploads(UploadHold::AfterApply);
    save(&file, b"# Plan\nmine\n");
    dd.d.wait_for("the upload to be applied", || {
        (dd.drive.upload_count("p1") == 1).then_some(())
    });
    let landed = dd.drive.head_revision("p1");
    assert_ne!(landed, first);
    // Killed before it heard the answer.
    assert_eq!(dd.row("Plan.md").state, "pending_upload");

    let dd = dd.restart();
    dd.drive.release_uploads();
    let done = dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.upload_count("p1"), 1, "no second revision");
    assert_eq!(dd.drive.head_revision("p1"), landed);
    assert_eq!(done.revision.as_deref(), Some(landed.as_str()));
    assert!(dd.queue().is_empty(), "the entry completed");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nmine\n");
    dd.d.close();
}

#[test]
fn e14_killed_after_the_answer_before_the_row_was_closed_the_retry_completes_too() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let first = dd.drive.head_revision("p1");
    save(&file, b"# Plan\nmine\n");
    dd.wait_state("Plan.md", "cached");
    dd.d.wait_for("the upload", || {
        (dd.drive.upload_count("p1") == 1).then_some(())
    });
    let landed = dd.drive.head_revision("p1");
    let md5 = {
        let mut h = tephra_remote::drive::Md5Hasher::new();
        h.update(b"# Plan\nmine\n");
        h.finish()
    };
    // The state a kill between Drive's answer and the cache's write leaves:
    // the item and its row as they were while the bytes were going.
    let dd = dd.restart_after(|cache| {
        remote_support::exec_sql(
            cache,
            &format!(
                "UPDATE remote_items SET state = 'pending_upload', revision = '{first}' \
                 WHERE source = 'gdrive' AND path = 'Plan.md'; \
                 INSERT INTO upload_queue (source, path, base_revision, attempts, last_error, created_at) \
                 VALUES ('gdrive', 'Plan.md', '{first}', 0, 'sending:{md5}', 0);"
            ),
        );
    });
    let done = dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.upload_count("p1"), 1, "no second revision");
    assert_eq!(dd.drive.head_revision("p1"), landed);
    assert_eq!(done.revision.as_deref(), Some(landed.as_str()));
    assert!(dd.queue().is_empty());
    dd.d.close();
}

#[test]
fn e14_a_change_of_somebody_else_is_not_mistaken_for_our_upload_that_landed() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"# Plan\nmine\n");
    dd.d.wait_for("the request to be at Drive", || {
        (dd.drive.uploads_held() == 1).then_some(())
    });
    // Killed; and while the daemon is down somebody else changes the file
    // (the unapplied request dies with the daemon, so nothing of ours is on
    // Drive).
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    let dd = dd.restart();
    dd.drive.release_uploads();
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(held.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsomeone else\n");
    dd.d.close();
}

// ---- fences ----------------------------------------------------------------------------------

#[test]
fn f2_f12_every_request_an_upload_makes_is_a_get_of_metadata_or_the_one_media_patch() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    let before = dd.drive.request_log().count();
    let reads = dd.media_reads();
    dd.drive.fail_uploads(2, 503);
    save(&file, b"# Plan\nmine\n");
    dd.wait_attempts("Plan.md", 1);
    dd.advance(Duration::from_secs(2));
    dd.wait_attempts("Plan.md", 2);
    dd.advance(Duration::from_secs(10));
    dd.wait_state("Plan.md", "cached");
    let all = dd.drive.request_log().all();
    for r in &all[before..] {
        let ok = (r.method == "GET"
            && !r.target().contains("alt=media")
            && !r.target().contains("/export"))
            || (r.method == "PATCH" && r.path == "/upload/drive/v3/files/p1");
        assert!(ok, "unexpected request: {} {}", r.method, r.target());
    }
    assert!(all[before..].iter().all(|r| !r.path.contains("trash")));
    assert!(dd.writes().iter().all(|w| w.starts_with("PATCH ")));
    assert_eq!(
        dd.writes().len(),
        3,
        "two refused tries and the one that landed"
    );
    assert_eq!(dd.drive.upload_count("p1"), 1);
    assert_eq!(dd.media_reads(), reads, "no file body was read to upload");
    dd.d.close();
}

#[test]
fn f9_no_token_is_in_the_queue_the_answers_or_the_item_rows_on_any_failure() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1, 403);
    save(&file, b"# Plan\nmine\n");
    dd.wait_state("Plan.md", "needs_attention").reason.unwrap();
    let everything = format!(
        "{:?} {:?} {} {}",
        dd.queue(),
        item_rows(&dd.d.cache_path, "gdrive"),
        dd.d.ok("source.list", json!({})),
        dd.d.ok("unit.list", json!({ "source": "gdrive" })),
    );
    assert!(!everything.contains(GOOGLE_TOKEN), "{everything}");
    assert!(!everything.contains("Bearer"), "{everything}");
    dd.d.close();
}

#[test]
fn a_save_while_the_source_is_offline_queues_and_goes_up_after_it_reconnects() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    // Drive stops answering: the next poll puts the source offline, and an
    // upload would not get through either.
    dd.drive.fail_next(1, 503);
    dd.drive.fail_uploads(1000, 503);
    dd.poll();
    let source = dd.d.source_row("gdrive").unwrap();
    assert_eq!(source["status"], json!("offline"), "{source}");

    save(&file, b"# Plan\nsaved offline\n");
    let tried = dd.wait_attempts("Plan.md", 1);
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    assert_eq!(dd.queue().len(), 1, "the queue is kept");
    assert!(tried.next_retry_at.is_some(), "and the retry is scheduled");
    assert_eq!(dd.drive.upload_count("p1"), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"# Plan\nsaved offline\n");

    // Reconnects: a good poll brings the source back and the edit goes up.
    dd.drive.fail_uploads(0, 503);
    dd.poll();
    dd.wait_state("Plan.md", "cached");
    assert_eq!(dd.drive.content("p1"), b"# Plan\nsaved offline\n");
    assert_eq!(dd.drive.upload_count("p1"), 1);
    assert!(dd.queue().is_empty());
    assert_eq!(
        dd.d.source_row("gdrive").unwrap()["status"],
        json!("active")
    );
    dd.d.close();
}
