//! PR-7 (E6): simultaneous fetches of one file share one download, every
//! caller gets the same answer, and a failed shared download fails all of
//! them with the same reason; the next fetch starts fresh.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{FetchRig, REPO};
use serde_json::{json, Value};

const README: &[u8] = b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n";

/// Fetches `path` from `n` threads at once, each over its own HTTP call.
fn fetch_together(rig: &FetchRig, path: &str, n: usize) -> Vec<Value> {
    let barrier = std::sync::Barrier::new(n);
    std::thread::scope(|scope| {
        let calls: Vec<_> = (0..n)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    rig.fetch(path)
                })
            })
            .collect();
        calls.into_iter().map(|c| c.join().unwrap()).collect()
    })
}

fn slow(rig: &FetchRig) {
    // Long enough that every thread of the batch arrives while one runs.
    rig.fake.gh.set_latency(Duration::from_millis(1500));
}

#[test]
fn two_simultaneous_fetches_make_one_content_request_and_both_get_the_path() {
    let rig = FetchRig::start(|_| {});
    slow(&rig);
    let reads = rig.reads();
    let replies = fetch_together(&rig, "README.md", 2);
    assert_eq!(
        rig.reads() - reads,
        1,
        "one content request for two callers"
    );
    let expected = rig.cached_path("README.md").display().to_string();
    for reply in &replies {
        assert_eq!(reply["result"]["path"], json!(expected), "{reply}");
        assert_eq!(reply["result"]["state"], json!("cached"));
    }
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), README);
}

#[test]
fn twenty_simultaneous_fetches_make_one_content_request_and_one_file() {
    let rig = FetchRig::start(|_| {});
    slow(&rig);
    let reads = rig.reads();
    let replies = fetch_together(&rig, "README.md", 20);
    assert_eq!(
        rig.reads() - reads,
        1,
        "one content request for twenty callers"
    );
    assert_eq!(replies.len(), 20);
    for reply in &replies {
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(reply["result"], replies[0]["result"]);
    }
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), README);
    assert_eq!(
        rig.cache_files(),
        vec![std::path::PathBuf::from("clone/README.md")],
        "one file, no temporary file"
    );
    assert_eq!(rig.row("README.md").state, "cached");
}

/// The coalescing is per file: two different files download side by side.
#[test]
fn fetches_of_different_files_do_not_wait_for_each_other_or_share() {
    let rig = FetchRig::start(|_| {});
    rig.fake.gh.set_latency(Duration::from_millis(700));
    let reads = rig.reads();
    let started = std::time::Instant::now();
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| rig.fetch("README.md"));
        let b = scope.spawn(|| rig.fetch("docs/Guide.md"));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(
        a.get("error").is_none() && b.get("error").is_none(),
        "{a} {b}"
    );
    assert_eq!(rig.reads() - reads, 2);
    assert!(
        started.elapsed() < Duration::from_millis(1350),
        "the two downloads ran together ({:?})",
        started.elapsed()
    );
}

#[test]
fn a_failed_shared_download_fails_every_waiter_the_same_way_and_the_next_fetch_starts_fresh() {
    let rig = FetchRig::start(|_| {});
    slow(&rig);
    rig.fake.gh.truncate_next_bodies(1);
    let reads = rig.reads();
    let replies = fetch_together(&rig, "README.md", 8);
    assert_eq!(rig.reads() - reads, 1, "one request, one failure, shared");
    let first = &replies[0]["error"];
    assert!(first.is_object(), "{}", replies[0]);
    for reply in &replies {
        assert_eq!(&reply["error"], first, "every waiter gets the same answer");
    }
    assert_eq!(
        first["data"]["reason"],
        json!("connection_dropped"),
        "{first}"
    );
    assert!(!rig.cached_path("README.md").exists());
    assert!(rig.cache_files().is_empty(), "no temporary file either");
    assert_eq!(rig.row("README.md").state, "remote");

    // The next fetch is its own download and succeeds.
    rig.fake.gh.set_latency(Duration::from_millis(0));
    let reply = rig.fetch("README.md");
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(rig.reads() - reads, 2);
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), README);
    assert_eq!(rig.row("README.md").state, "cached");
    assert_eq!(rig.row("README.md").reason, None, "the old reason is gone");
}

/// The CLI is a client of the same RPC: `tephractl fetch` from several
/// processes shares the download too (checked in the CLI test binary, which
/// owns the binary); here two WebSocket clients and an HTTP call do.
#[test]
fn a_websocket_client_and_http_callers_share_one_download() {
    let rig = FetchRig::start(|_| {});
    slow(&rig);
    let reads = rig.reads();
    let mut ws = rig.d.connect();
    let replies = std::thread::scope(|scope| {
        let http = scope.spawn(|| fetch_together(&rig, "README.md", 3));
        std::thread::sleep(Duration::from_millis(300));
        let over_ws = ws.request("unit.fetch", json!({ "unit": "@g:README.md" }));
        (http.join().unwrap(), over_ws)
    });
    assert_eq!(rig.reads() - reads, 1);
    let (http, over_ws) = replies;
    for reply in &http {
        assert_eq!(reply["result"]["path"], over_ws["path"], "{over_ws}");
    }
    let _ = REPO;
}
