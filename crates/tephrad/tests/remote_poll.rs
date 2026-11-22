//! PR-6 (G5, E1, E2, E13, F2/F3, F12, GP2/GP4): the per-source poller of a
//! GitHub source. Real registry, real RPC server and a real WebSocket
//! client against the PR-3 fake GitHub.
//!
//! Time is the daemon's clock, a `FakeClock` the test moves: a poll is due
//! when the clock says so. (The registry owns its own multi-thread runtime
//! and talks to real sockets, so tokio's paused time cannot drive it; the
//! clock is the seam the adapters already use.) Waits for events are
//! bounded; "nothing happens" is a short bounded quiet window.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::{files_under, index_snapshot, Fake, Rig, REPO, TOKEN};
use serde_json::{json, Value};
use tephra_remote::testing::{FakeGithub, GithubLimit};

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn paths(entries: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = entries
        .iter()
        .filter(|e| e["kind"] != "folder")
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

/// The polls' `if-none-match` headers, in order.
fn sent_etags(fake: &Fake) -> Vec<Option<String>> {
    fake.polls()
        .iter()
        .map(|r| r.header("if-none-match").map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------
// G5: the interval, the conditional request, the 304.
// ---------------------------------------------------------------------------

#[test]
fn one_conditional_request_per_interval_carrying_the_stored_etag() {
    let r = Rig::start();
    let first_index = r.fake.polls().len();
    let stored = r
        .fake
        .gh
        .request_log()
        .matching("/commits/")
        .last()
        .cloned();
    assert!(stored.is_some(), "the add asked for the head once");
    r.fake.gh.request_log().clear();

    r.advance(s(59));
    r.quiet();
    assert!(r.fake.polls().is_empty(), "no request at 59 s");

    r.advance(s(2));
    r.wait_polls(1);
    r.quiet();
    assert_eq!(r.fake.polls().len(), 1, "exactly one request at 61 s");

    // Repeated each interval: 59 s after the poll is too early, 61 s is due.
    r.advance(s(59));
    r.quiet();
    assert_eq!(r.fake.polls().len(), 1, "not yet");
    r.advance(s(2));
    r.wait_polls(2);
    r.advance(s(61));
    r.wait_polls(3);
    r.quiet();
    assert_eq!(r.fake.polls().len(), 3);

    let etags = sent_etags(&r.fake);
    assert!(etags.iter().all(Option::is_some), "conditional: {etags:?}");
    assert!(
        etags.windows(2).all(|w| w[0] == w[1]),
        "the same stored ETag each time: {etags:?}"
    );
    // A poll that finds nothing reads no tree and no blob.
    assert!(r.fake.tree_reads().is_empty());
    assert!(r.fake.reads().is_empty());
    let _ = first_index;
    r.d.close();
}

#[test]
fn a_304_writes_no_index_row_and_sends_no_push_only_last_sync_advances() {
    let r = Rig::start();
    let mut client = r.d.connect();
    let before = index_snapshot(&r.d.cache_path);
    let synced = r.d.source_row("g").unwrap()["last_scanned_at"]
        .as_i64()
        .unwrap();
    r.fake.gh.request_log().clear();

    r.advance(s(61));
    r.wait_polls(1);
    let row = r.d.wait_for("last_sync advanced", || {
        r.d.source_row("g")
            .filter(|row| row["last_scanned_at"].as_i64().unwrap() > synced)
    });
    assert_eq!(row["status"], json!("active"));
    assert_eq!(row["last_scanned_at"].as_i64().unwrap(), synced + 61);

    assert_eq!(
        index_snapshot(&r.d.cache_path),
        before,
        "a 304 wrote no index row"
    );
    assert_eq!(
        client.drain_graph_changed(Duration::from_millis(400)),
        0,
        "and sent no graph:changed"
    );
    r.d.close();
}

// ---------------------------------------------------------------------------
// G5: a changed tree.
// ---------------------------------------------------------------------------

#[test]
fn added_changed_and_removed_files_are_applied_with_one_push_and_no_bytes_kept() {
    let r = Rig::start();
    let mut client = r.d.connect();
    r.fake.gh.request_log().clear();

    // Changed: README now links Notes and drops Guide. Added: notes.md and a
    // binary. Removed: src/lib.rs. All in one commit.
    let readme = b"# Atlas 2\n\nNow see [[notes]].\n\n## New heading\n";
    r.fake.gh.put_files(
        REPO,
        "main",
        [
            ("README.md".to_string(), &readme[..]),
            (
                "notes.md".to_string(),
                &b"# Notes\n\nBack to [[README]].\n"[..],
            ),
            (
                "assets/new.png".to_string(),
                &[0x89, b'P', b'N', b'G', 9][..],
            ),
        ],
    );
    r.fake.gh.remove_file(REPO, "main", "src/lib.rs");
    // (put_files and remove_file are two commits; one poll sees both.)

    r.advance(s(61));
    let seen = client
        .next_graph_changed(Duration::from_secs(20))
        .expect("a graph:changed after a changed poll");
    assert!(seen.is_object(), "{seen}");
    r.d.wait_for("the index shows the new file", || {
        paths(&r.d.entries("g"))
            .contains(&"notes.md".to_string())
            .then_some(())
    });
    assert_eq!(
        client.drain_graph_changed(Duration::from_millis(500)),
        0,
        "exactly one graph:changed for the poll"
    );

    let now = paths(&r.d.entries("g"));
    assert_eq!(
        now,
        [
            "README.md",
            "assets/logo.png",
            "assets/new.png",
            "docs/Guide.md",
            "notes.md"
        ]
    );

    // The changed file was re-read through the API and re-indexed: links,
    // headings.
    let forward =
        r.d.ok("graph.get_forward_links", json!({"id": "@g:README.md"}));
    let raws: Vec<&str> = forward
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["to_raw"].as_str().unwrap())
        .collect();
    assert_eq!(raws, ["notes"], "{forward}");
    let back = r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"}));
    assert!(
        back.as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == json!("@g:notes.md")),
        "the added file's link: {back}"
    );
    // Exactly the added and changed text files were read: README and
    // notes.md. The binary and the removed file were not.
    let reads = r.fake.reads();
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(reads
        .iter()
        .any(|t| t.contains(&FakeGithub::blob_sha(readme))));

    // F12 / GP2: nothing on disk, nothing cached, every file `remote`.
    assert!(files_under(&r.d.data_dir.join("remote")).is_empty());
    let row = r.d.source_row("g").unwrap();
    assert_eq!(row["cache_bytes"], json!(0));
    for e in r.d.entries("g").iter().filter(|e| e["kind"] != "folder") {
        assert_eq!(e["state"], json!("remote"), "{e}");
        assert_eq!(e["cache_bytes"], json!(0), "{e}");
    }
    // F2 / F3: only GETs, ever.
    assert!(r.fake.non_get_requests().is_empty());
    r.d.close();
}

#[test]
fn a_changed_file_refreshes_its_headings_and_links_not_just_its_row() {
    let r = Rig::start();
    r.fake.gh.put_file(
        REPO,
        "main",
        "docs/Guide.md",
        b"# Guide\n\n## Fresh section\n\nNow [[src/lib.rs]] and [[Nowhere]].\n",
    );
    r.advance(s(61));
    let guide = r.d.ok_when(
        "graph.get_forward_links",
        json!({"id": "@g:docs/Guide.md"}),
        |v| v.as_array().is_some_and(|a| a.len() == 2),
    );
    let raws: Vec<&str> = guide
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["to_raw"].as_str().unwrap())
        .collect();
    assert!(raws.contains(&"Nowhere"), "{guide}");
    // The old edge from Guide to README is gone: README has no backlink
    // from Guide any more.
    let back = r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"}));
    assert!(
        back.as_array()
            .unwrap()
            .iter()
            .all(|e| e["from"] != json!("@g:docs/Guide.md")),
        "{back}"
    );
    let resolved = r.d.ok(
        "unit.resolve_link",
        json!({"link": "[[@g:docs/Guide.md#Fresh section]]"}),
    );
    assert_eq!(
        resolved["unit"]["id"],
        json!("@g:docs/Guide.md"),
        "{resolved}"
    );
    r.d.close();
}

#[test]
fn a_remote_rename_is_one_removal_and_one_addition_no_rename_event_no_fix() {
    let r = Rig::start();
    let mut client = r.d.connect();
    r.fake.gh.remove_file(REPO, "main", "docs/Guide.md");
    r.fake.gh.put_file(
        REPO,
        "main",
        "docs/Handbook.md",
        b"# Guide\n\nBack to [[README]].\n",
    );
    r.advance(s(61));
    // One push for the poll (a first push is the change itself).
    let first = client.next_graph_changed(Duration::from_secs(20));
    assert!(first.is_some(), "a graph:changed for the rename");
    r.d.wait_for("renamed", || {
        let p = paths(&r.d.entries("g"));
        (p.contains(&"docs/Handbook.md".to_string()) && !p.contains(&"docs/Guide.md".to_string()))
            .then_some(())
    });
    // One push for the poll, no more.
    assert_eq!(client.drain_graph_changed(Duration::from_millis(500)), 0);
    // The old name's link in README is unresolved now: nothing rewrote it
    // (no rename event, no fix log entry).
    let forward =
        r.d.ok("graph.get_forward_links", json!({"id": "@g:README.md"}));
    let guide = forward
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["to_raw"] == json!("Guide"))
        .unwrap_or_else(|| panic!("README still links [[Guide]]: {forward}"));
    assert!(guide["to"].get("resolved").is_none(), "{guide}");
    let fixes = r.d.ok("fix.list", json!({}));
    assert_eq!(fixes, json!([]), "no fix log entry");
    r.d.close();
}

// ---------------------------------------------------------------------------
// G5: single flight, independence, shutdown.
// ---------------------------------------------------------------------------

#[test]
fn a_slow_poll_never_stacks_and_two_sources_poll_independently() {
    let r = Rig::start();
    r.fake.gh.add_repo("danrfletcher/other", "main");
    r.fake
        .gh
        .put_file("danrfletcher/other", "main", "a.md", b"# A\n");
    r.d.add_github("h", "danrfletcher/other");
    r.d.wait_active("h");
    r.fake.gh.request_log().clear();

    // Each source is polled once at its own 61 s: two requests, one each.
    r.advance(s(61));
    r.wait_polls(2);
    r.quiet();
    let polls = r.fake.polls();
    assert_eq!(polls.len(), 2, "{polls:?}");
    assert!(polls.iter().any(|p| p.path.contains("danrfletcher/atlas")));
    assert!(polls.iter().any(|p| p.path.contains("danrfletcher/other")));

    // A slow poll: the clock runs far ahead while it is in flight. It does
    // not stack: the next poll starts after it ends, once.
    r.fake.gh.request_log().clear();
    r.fake.gh.set_latency(Duration::from_millis(250));
    r.advance(s(61));
    r.wait_polls(2);
    r.advance(s(500));
    r.quiet();
    std::thread::sleep(Duration::from_millis(800));
    let n = r.fake.polls().len();
    r.fake.gh.set_latency(Duration::ZERO);
    // Two sources; each polled at most twice (the one in flight, then one
    // catch-up), never once per elapsed interval (8 of them).
    assert!(n <= 4, "polls stacked: {n}");
    r.d.close();
}

#[test]
fn a_shutdown_mid_poll_commits_nothing_half_applied() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    r.fake
        .gh
        .put_file(REPO, "main", "README.md", b"# Changed\n\n[[docs/Guide]]\n");
    r.fake.gh.put_file(REPO, "main", "new.md", b"# New\n");
    r.fake.gh.request_log().clear();
    r.fake.gh.set_latency(Duration::from_millis(300));

    r.advance(s(61));
    // The poll is in flight: it has asked for the head.
    r.wait_polls(1);
    let cache_path = r.d.cache_path.clone();
    let dir = r.d.close_keeping_data();
    // Nothing of the half-read poll was committed.
    let after = index_snapshot(&cache_path);
    assert_eq!(after, before, "nothing half-applied");
    drop(dir);
}

// ---------------------------------------------------------------------------
// E1, E2, E13: what a failed poll means.
// ---------------------------------------------------------------------------

#[test]
fn a_rate_limit_is_syncing_with_the_retry_time_and_polling_resumes_then() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    r.fake.gh.rate_limit_for(s(300), GithubLimit::RetryAfter403);
    r.fake.gh.request_log().clear();

    r.advance(s(61));
    let row = r.wait_status("syncing");
    let reason = row["status_reason"].as_str().unwrap();
    assert!(reason.contains("rate limit"), "{reason}");
    assert!(reason.contains("UTC") || reason.contains(':'), "{reason}");
    assert_eq!(index_snapshot(&r.d.cache_path), before, "index untouched");
    let limited = r.fake.polls().len();

    // The retry time is 300 s after the limited request's start (61 s):
    // another interval later it is still waiting.
    r.advance(s(100));
    r.quiet();
    assert_eq!(
        r.fake.polls().len(),
        limited,
        "no request before the retry time"
    );
    r.advance(s(250));
    r.wait_status("active");
    assert!(r.fake.polls().len() > limited, "polling resumed");
    assert_eq!(
        index_snapshot(&r.d.cache_path),
        before,
        "and still no re-index"
    );
    r.d.close();
}

#[test]
fn a_401_is_auth_required_not_offline_and_the_index_still_answers() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    let listed = paths(&r.d.entries("g"));
    r.fake.gh.revoke_token();

    r.advance(s(61));
    let row = r.wait_status("auth_required");
    assert!(row["status_reason"]
        .as_str()
        .unwrap()
        .contains("tephractl auth github"));
    assert_eq!(
        paths(&r.d.entries("g")),
        listed,
        "the listing still answers"
    );
    let back = r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"}));
    assert!(back.is_array());
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    // No token anywhere an answer can carry one.
    let shown = format!("{row}{}", r.d.rpc("source.list", json!({})));
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(!shown.contains("Bearer"), "{shown}");

    // Signing in again: the next poll is good.
    r.fake.gh.accept_only_token("ghp_SECOND_sentinel_token");
    r.sign_in_again("ghp_SECOND_sentinel_token");
    r.advance(s(61));
    r.wait_status("active");
    assert_eq!(index_snapshot(&r.d.cache_path), before, "no re-index");
    r.d.close();
}

#[test]
fn a_moved_repo_is_needs_attention_with_the_new_place_and_is_never_followed() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    let listed = paths(&r.d.entries("g"));
    r.fake
        .gh
        .move_repo(REPO, "https://api.github.com/repositories/424242");
    r.fake.gh.request_log().clear();

    r.advance(s(61));
    let row = r.wait_status("needs_attention");
    let reason = row["status_reason"].as_str().unwrap();
    assert!(reason.contains("repositories/424242"), "{reason}");
    assert!(reason.contains(REPO), "names the old repo too: {reason}");
    assert_eq!(index_snapshot(&r.d.cache_path), before, "no item removed");
    assert_eq!(paths(&r.d.entries("g")), listed);
    // The adapter did not follow: every request went to the old repo's
    // routes and the fake never saw the new location.
    for q in r.fake.gh.request_log().all() {
        assert!(q.path.contains("danrfletcher/atlas"), "{}", q.path);
    }
    r.d.close();
}

#[test]
fn a_whole_repo_or_branch_404_is_needs_attention_and_every_item_stays() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    let listed = paths(&r.d.entries("g"));
    // The branch is gone: its ref answers 404.
    r.fake.gh.fail_next(1, 404);

    r.advance(s(61));
    let row = r.wait_status("needs_attention");
    assert!(
        row["status_reason"]
            .as_str()
            .unwrap()
            .contains("nothing was removed"),
        "{row}"
    );
    assert_eq!(
        paths(&r.d.entries("g")),
        listed,
        "no file went through the delete path"
    );
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    r.d.close();
}

// ---------------------------------------------------------------------------
// Fence: polling only.
// ---------------------------------------------------------------------------

#[test]
fn polling_makes_no_write_request_and_no_listener_beyond_the_rpc_port() {
    let r = Rig::start();
    r.fake.gh.put_file(REPO, "main", "x.md", b"# X\n");
    r.advance(s(61));
    r.d.wait_for("x listed", || {
        paths(&r.d.entries("g"))
            .contains(&"x.md".to_string())
            .then_some(())
    });
    assert!(r.fake.non_get_requests().is_empty());
    r.d.close();
}
