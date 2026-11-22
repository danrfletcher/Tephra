//! PR-7 (F12, GP5): nothing is downloaded unless someone asks. After a source
//! is added, through sync cycles, `unit.list`, `source.list`, link
//! resolution and backlinks, the only content requests are the first-index
//! reads. A fetch the daemon wrote itself is never taken for an edit.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{FetchRig, QUIET};
use serde_json::json;

fn count(rig: &FetchRig, table: &str) -> i64 {
    tephra_cache::Cache::open_read_only(&rig.d.cache_path)
        .unwrap()
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn nothing_is_downloaded_through_sync_lists_and_link_resolution() {
    let rig = FetchRig::start(|_| {});
    let index_reads = rig.reads();
    assert_eq!(index_reads, 3, "the first index read each text file once");

    for _ in 0..3 {
        // A 60 s sync cycle (the poll finds nothing new).
        rig.clock.advance(Duration::from_secs(61));
        std::thread::sleep(QUIET);
        let _ = rig.d.entries("g");
        let _ = rig.d.ok("source.list", json!({}));
        let _ = rig.d.rpc(
            "unit.resolve_link",
            json!({"link": "[[Guide]]", "default_source": "g"}),
        );
        let _ = rig
            .d
            .rpc("graph.get_backlinks", json!({"id": "@g:README.md"}));
        let _ = rig.d.rpc("unit.search", json!({"query": "Setup"}));
    }
    assert!(rig.fake.polls().len() >= 2, "the sync cycles ran");
    assert_eq!(
        rig.reads(),
        index_reads,
        "no content request beyond the index reads"
    );
    assert!(
        rig.cache_files().is_empty(),
        "clone/ holds nothing nobody asked for"
    );
    assert!(!rig
        .d
        .data_dir
        .join("remote")
        .join("g")
        .join("clone")
        .exists());
}

// F12 line 2 is split by owner. PR-7 checks what exists before PR-9: after the
// watcher debounce the upload queue is empty and the file is `cached`, not
// `pending_upload` or `not_pushed`. PR-9 owns the other two parts, because the
// `N changes not pushed` output in `source list` and a git clone (`HEAD`,
// `git status --porcelain`) do not exist until then: it must re-run this fetch
// and assert `0 changes not pushed` and an unchanged `HEAD` and porcelain.
#[test]
fn a_fetch_is_not_seen_as_an_edit_and_starts_no_other_download() {
    let rig = FetchRig::start(|_| {});
    let units = count(&rig, "units");
    let links = count(&rig, "links");
    let before_reads = rig.reads();

    let reply = rig.fetch("README.md");
    assert!(reply.get("error").is_none(), "{reply}");
    // The daemon's own write is not a change to push: wait out a watcher
    // debounce and a sync cycle and look.
    rig.clock.advance(Duration::from_secs(61));
    std::thread::sleep(Duration::from_millis(2500));

    assert_eq!(count(&rig, "upload_queue"), 0, "the upload queue is empty");
    let row = rig.row("README.md");
    assert_eq!(row.state, "cached", "not pending_upload or not_pushed");
    assert_eq!(count(&rig, "units"), units);
    assert_eq!(count(&rig, "links"), links);
    assert_eq!(rig.reads() - before_reads, 1, "only the one fetch read");
    assert_eq!(
        rig.cache_files(),
        vec![std::path::PathBuf::from("clone/README.md")],
        "and only the file asked for"
    );
    let (non_get, requests) = (rig.fake.non_get_requests(), rig.fake.requests());
    assert!(non_get.is_empty(), "{non_get:?}");
    assert!(requests > 0);
}
