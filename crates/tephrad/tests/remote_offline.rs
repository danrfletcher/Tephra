//! PR-6 (G6): a GitHub source that cannot reach GitHub. A refused port, a
//! timeout and a 5xx each make the source `offline` with a reason and its
//! last sync; the index, links, backlinks and `unit.list` rows keep
//! answering unchanged; nothing downloads; a cached file and a queued edit
//! are byte-for-byte what they were; the next good poll is `active` again
//! with no re-index. No sleeps: waits are bounded, time is a fake clock.
#![allow(dead_code)]

mod remote_support;

use std::time::{Duration, Instant};

use remote_support::{
    files_under, index_snapshot, seed_cached_copy, seed_queued_upload, Daemon, Fake, MemStore, Rig,
    REPO, TOKEN,
};
use serde_json::{json, Value};
use tephra_remote::AdapterOptions;
use tephrad::remote::poll::RemoteReadError;

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn listed(r: &Rig) -> Vec<Value> {
    r.d.entries("g")
}

/// What the daemon answers about the source: the listing, a link, the
/// backlinks and the forward links.
fn answers(r: &Rig) -> Value {
    json!({
        "entries": listed(r),
        "resolved": r.d.ok("unit.resolve_link", json!({"link": "[[@g:README.md]]"})),
        "back": r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"})),
        "forward": r.d.ok("graph.get_forward_links", json!({"id": "@g:README.md"})),
    })
}

/// `unit.search` inside one file: whether it says the source is offline.
fn in_file_offline(r: &Rig) -> bool {
    let reply = r.d.ok(
        "unit.search",
        json!({"in": "@g:README.md", "scope": "headings", "query": ""}),
    );
    assert!(
        reply["results"].as_array().is_some_and(|a| !a.is_empty()),
        "{reply}"
    );
    reply["offline"].as_bool().unwrap_or(false)
}

fn fast_timeouts() -> AdapterOptions {
    AdapterOptions {
        request_timeout: Duration::from_millis(400),
        connect_timeout: Duration::from_millis(400),
    }
}

#[test]
fn a_refused_port_is_offline_with_the_reason_and_last_sync_and_the_index_still_answers() {
    let r = Rig::start();
    let cached = seed_cached_copy(&r.d.data_dir, "g", "docs/Guide.md", b"my own edit\n");
    seed_queued_upload(&r.d.cache_path, "g", "docs/Guide.md");
    let before_index = index_snapshot(&r.d.cache_path);
    let before_answers = answers(&r);
    assert!(
        !in_file_offline(&r),
        "an active remote source is not offline"
    );
    let synced = r.d.source_row("g").unwrap()["last_scanned_at"].clone();
    assert!(synced.is_i64(), "a last sync to keep: {synced}");
    r.fake.gh.request_log().clear();

    r.fake.network_down();
    r.advance(s(61));
    let row = r.wait_status("offline");
    let reason = row["status_reason"].as_str().unwrap();
    assert!(reason.contains("cannot reach GitHub"), "{reason}");
    assert_eq!(row["last_scanned_at"], synced, "last_sync kept: {row}");
    assert!(!row.to_string().contains(TOKEN));

    // The index, links, backlinks and listing: unchanged and answering.
    assert_eq!(answers(&r), before_answers);
    assert_eq!(index_snapshot(&r.d.cache_path), before_index);
    // Cache bytes still 0, every file still `remote`, nothing downloaded.
    assert_eq!(row["cache_bytes"], json!(0));
    assert_eq!(
        r.fake.requests(),
        0,
        "nothing reached the fake: the connection was down"
    );

    // The in-file search says the source is offline now, and only now.
    assert!(in_file_offline(&r));

    // Cached file and queued edit: byte for byte.
    assert_eq!(std::fs::read(&cached).unwrap(), b"my own edit\n");

    // Back: the next good poll is active again, with no re-index.
    r.fake.network_up();
    r.advance(s(61));
    let row = r.wait_status("active");
    assert!(row.get("status_reason").is_none_or(Value::is_null), "{row}");
    assert!(
        row["last_scanned_at"].as_i64().unwrap() > synced.as_i64().unwrap(),
        "last_sync moved on: {row}"
    );
    assert_eq!(index_snapshot(&r.d.cache_path), before_index);
    assert_eq!(answers(&r), before_answers);
    assert!(
        r.fake.tree_reads().is_empty(),
        "no tree read after recovery"
    );
    assert!(r.fake.reads().is_empty(), "no blob read after recovery");
    assert_eq!(std::fs::read(&cached).unwrap(), b"my own edit\n");
    assert!(r.fake.non_get_requests().is_empty());
    r.d.close();
}

#[test]
fn a_timeout_is_offline_and_recovers() {
    let r = Rig::start_with(fast_timeouts());
    let before = index_snapshot(&r.d.cache_path);
    r.fake.gh.set_latency(s(30));

    r.advance(s(61));
    let row = r.wait_status("offline");
    assert!(
        row["status_reason"].as_str().unwrap().contains("timed out"),
        "{row}"
    );
    assert_eq!(index_snapshot(&r.d.cache_path), before);

    r.fake.gh.set_latency(Duration::ZERO);
    r.advance(s(61));
    r.wait_status("active");
    assert_eq!(index_snapshot(&r.d.cache_path), before, "no re-index");
    r.d.close();
}

#[test]
fn every_5xx_is_offline_and_the_next_good_poll_recovers() {
    for status in [500u16, 502, 503, 504] {
        let r = Rig::start();
        let before = index_snapshot(&r.d.cache_path);
        let answered = answers(&r);
        r.fake.gh.fail_next(1, status);

        r.advance(s(61));
        let row = r.wait_status("offline");
        let reason = row["status_reason"].as_str().unwrap().to_string();
        assert!(reason.contains(&status.to_string()), "{status}: {reason}");
        assert_eq!(answers(&r), answered, "{status}");

        r.advance(s(61));
        r.wait_status("active");
        assert_eq!(
            index_snapshot(&r.d.cache_path),
            before,
            "{status}: no re-index"
        );
        r.d.close();
    }
}

#[test]
fn offline_for_many_intervals_keeps_polling_and_never_touches_the_index() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    r.fake.network_down();
    for _ in 0..3 {
        r.advance(s(61));
        r.wait_status("offline");
    }
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    r.fake.network_up();
    r.advance(s(61));
    r.wait_status("active");
    r.d.close();
}

#[test]
fn an_uncached_read_while_offline_is_an_offline_error_with_a_reason_in_bounded_time() {
    let r = Rig::start_with(fast_timeouts());
    r.fake.network_down();

    let t = Instant::now();
    let err =
        r.d.registry
            .read_remote_file("g", "README.md")
            .expect_err("offline");
    assert!(t.elapsed() < s(10), "bounded: {:?}", t.elapsed());
    match &err {
        RemoteReadError::Offline { reason } => {
            assert!(reason.contains("cannot reach GitHub"), "{reason}")
        }
        other => panic!("not an offline error: {other:?}"),
    }
    assert!(!err.to_string().contains(TOKEN));
    assert!(
        files_under(&r.d.data_dir.join("remote")).is_empty(),
        "no partial file"
    );

    // A remote that answers too slowly is the same answer, in time.
    r.fake.network_up();
    r.fake.gh.set_latency(s(30));
    let t = Instant::now();
    let err =
        r.d.registry
            .read_remote_file("g", "README.md")
            .expect_err("timed out");
    assert!(t.elapsed() < s(10), "bounded: {:?}", t.elapsed());
    assert!(matches!(err, RemoteReadError::Offline { .. }), "{err:?}");
    assert!(files_under(&r.d.data_dir.join("remote")).is_empty());

    // And once it is reachable the same read works, through the API, with
    // nothing written.
    r.fake.gh.set_latency(Duration::ZERO);
    let bytes = r.d.registry.read_remote_file("g", "README.md").unwrap();
    assert!(bytes.starts_with(b"# Atlas"));
    assert!(files_under(&r.d.data_dir.join("remote")).is_empty());
    r.d.close();
}

#[test]
fn a_read_of_a_file_the_source_does_not_have_is_not_found_not_offline() {
    let r = Rig::start();
    assert_eq!(
        r.d.registry.read_remote_file("g", "nope.md").unwrap_err(),
        RemoteReadError::NotFound
    );
    assert_eq!(
        r.d.registry
            .read_remote_file("nobody", "README.md")
            .unwrap_err(),
        RemoteReadError::NotFound
    );
    r.d.close();
}

/// A short network drop in the middle of the first index: the index gives up
/// after its tries, the source says `offline` (it is not left `indexing`),
/// and when the network is back the poller carries the index on to the end,
/// with no restart of the daemon.
#[test]
fn a_first_index_that_loses_the_network_is_offline_then_finishes_with_no_restart() {
    let clock = std::sync::Arc::new(tephra_remote::FakeClock::at_epoch_2026());
    let fake = Fake::start_gated(clock.clone());
    fake.atlas();
    fake.gh.set_latency(Duration::from_millis(150));
    let d = Daemon::start_full(
        &fake,
        MemStore::signed_in(),
        None,
        clock.clone(),
        Default::default(),
    );
    d.add_github("g", REPO);
    d.wait_for("the index has begun", || {
        d.source_row("g").filter(|r| {
            r["index_done"].as_u64().unwrap_or(0) < r["index_total"].as_u64().unwrap_or(0)
        })
    });

    fake.network_down();
    let row = d.wait_for("g is offline", || {
        d.source_row("g")
            .filter(|r| r["status"] == json!("offline"))
    });
    let reason = row["status_reason"].as_str().unwrap();
    assert!(reason.contains("cannot reach GitHub"), "{reason}");
    assert!(
        row["index_done"].as_u64().unwrap() < row["index_total"].as_u64().unwrap(),
        "the index is not done: {row}"
    );

    // Intervals pass with the network still away: still offline, and the
    // rows are not touched.
    let before = index_snapshot(&d.cache_path);
    // Each interval makes the poller try once more (nothing reaches the
    // fake while the network is away, so its request count cannot show it):
    // wait for the poller's own count of finished polls, bounded.
    for _ in 0..2 {
        let done = d.registry.remote_polls_done();
        clock.advance(s(61));
        d.wait_for("the poller tried again", || {
            (d.registry.remote_polls_done() > done).then_some(())
        });
    }
    assert_eq!(d.source_row("g").unwrap()["status"], json!("offline"));
    assert_eq!(index_snapshot(&d.cache_path), before);

    // The network is back: the next interval reads what is left.
    fake.network_up();
    fake.gh.set_latency(Duration::ZERO);
    clock.advance(s(61));
    let row = d.wait_active("g");
    assert_eq!(row["index_done"], row["index_total"], "{row}");
    assert_eq!(row["index_total"], json!(3));
    assert!(row.get("status_reason").is_none_or(Value::is_null), "{row}");
    let forward = d.ok("graph.get_forward_links", json!({"id": "@g:README.md"}));
    assert!(!forward.as_array().unwrap().is_empty(), "{forward}");
    d.close();
}
