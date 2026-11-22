//! PR-8 fences: F1 (an evicted file has no path and leaves no placeholder),
//! F2 and F12 (eviction, `cache set-limit` and `cache on|off` make no
//! request at all to GitHub, delete nothing remote, download nothing, and
//! work with the source offline), F3 (nothing in a clone is committed,
//! pushed, merged or rebased: no git is run and no `.git` is made).
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{Daemon, FetchRig, Tuning};
use serde_json::{json, Value};

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

#[test]
fn eviction_set_limit_and_cache_off_make_no_request_and_leave_no_trace() {
    // A poller re-reads the fake clock every `poll_tick` of real time and
    // runs its poll once the clock says it is due. This test moves the
    // clock 700 s, so the tick is longer than the test: no poll can run
    // between the first count and the last, and any request seen is the
    // eviction's own.
    let rig = FetchRig::start_capped(
        Tuning {
            poll_tick: Duration::from_secs(3600),
            ..Tuning::default()
        },
        |_| {},
    );
    for p in ["README.md", "docs/Guide.md", "src/lib.rs"] {
        ok(&rig.fetch(p));
    }
    // Let the poller's own traffic settle, then count from here.
    rig.d.wait_active("g");
    let requests = rig.fake.requests();
    let reads = rig.fake.reads().len();

    rig.d
        .ok("source.set_cache", json!({"name": "g", "cache": false}));
    rig.d
        .ok("source.set_cache", json!({"name": "g", "cache": true}));
    rig.d.ok("cache.set_limit", json!({"limit": "1 MB"}));
    rig.clock.advance(Duration::from_secs(700));
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(3), "{set}");

    // F1: no path, no placeholder, no symlink.
    for p in ["README.md", "docs/Guide.md", "src/lib.rs"] {
        assert!(!rig.cached_path(p).exists(), "{p}");
        assert!(
            std::fs::symlink_metadata(rig.cached_path(p)).is_err(),
            "{p}: nothing is left at the path"
        );
    }
    assert!(rig.cache_files().is_empty(), "{:?}", rig.cache_files());
    // F2, F12: no request, so no delete, no download. (The poller's tick is
    // an hour of real time, so the 700 s the clock moved started no poll.)
    assert!(rig.fake.non_get_requests().is_empty());
    assert_eq!(rig.fake.reads().len(), reads, "nothing was downloaded");
    assert_eq!(rig.fake.requests(), requests, "not one request");
    // F3: eviction makes no git data.
    let root = rig.d.data_dir.join("remote").join("g");
    assert!(!root.join(".git").exists());
    assert!(!root.join("clone").join(".git").exists());
}

#[test]
fn eviction_works_with_the_source_offline() {
    let rig = FetchRig::start_capped(
        Tuning {
            gated: true,
            ..Tuning::default()
        },
        |_| {},
    );
    ok(&rig.fetch("README.md"));
    rig.fake.network_down();
    rig.clock.advance(Duration::from_secs(700));
    let requests = rig.fake.requests();
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(1), "{set}");
    assert_eq!(rig.row("README.md").state, "remote");
    assert_eq!(rig.fake.requests(), requests);
    rig.fake.network_up();
}

#[test]
fn eviction_works_with_the_source_signed_out() {
    let fake = remote_support::Fake::start();
    fake.atlas();
    let d = Daemon::start_signed_out(&fake);
    // No credential: the source cannot even be added, but a daemon with
    // none still answers the cache verbs.
    let status = d.ok("cache.status", json!({}));
    assert_eq!(status["cache_bytes"], json!(0));
    let set = d.ok("cache.set_limit", json!({"limit": "1 GB"}));
    assert_eq!(set["limit_bytes"], json!(1_000_000_000u64));
    assert_eq!(fake.requests(), 0);
    d.close();
}
