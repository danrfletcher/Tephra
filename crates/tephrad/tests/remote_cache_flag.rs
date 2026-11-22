//! PR-8 (G20, E22): the per-source cache flag. `source.set_cache` sets it,
//! it survives a restart, and with it off a fetched file is dropped once
//! it has been idle for the window even under the cap. Pins win.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{cache_status, exec_sql, FetchRig, Tuning};
use serde_json::{json, Value};

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

fn set_cache(rig: &FetchRig, name: &str, on: bool) -> Value {
    rig.d
        .ok("source.set_cache", json!({"name": name, "cache": on}))
}

fn flag_in_source_list(rig: &FetchRig) -> Value {
    rig.d.source_row("g").unwrap()["cache"].clone()
}

fn flag_in_unit_list(rig: &FetchRig) -> Value {
    rig.d.ok(
        "unit.list",
        json!({ "source": "g", "folder": "", "recursive": false }),
    )["cache"]
        .clone()
}

#[test]
fn the_flag_is_on_by_default_and_both_lists_show_it_with_the_cache_bytes() {
    let rig = FetchRig::start(|_| {});
    assert_eq!(flag_in_source_list(&rig), json!(true));
    assert_eq!(flag_in_unit_list(&rig), json!(true));
    ok(&rig.fetch("README.md"));
    let row = rig.d.source_row("g").unwrap();
    assert!(row["cache_bytes"].as_u64().unwrap() > 0, "{row}");

    let reply = set_cache(&rig, "g", false);
    assert_eq!(reply["cache"], json!(false), "{reply}");
    assert_eq!(reply["was"], json!(true));
    assert_eq!(flag_in_source_list(&rig), json!(false));
    assert_eq!(flag_in_unit_list(&rig), json!(false));
    assert_eq!(cache_status(&rig.d)["sources"][0]["cache"], json!(false));
    // Setting it again says nothing changed.
    assert_eq!(set_cache(&rig, "g", false)["was"], json!(false));
}

#[test]
fn the_flag_survives_a_restart() {
    let rig = FetchRig::start(|_| {});
    set_cache(&rig, "g", false);
    let rig = rig.restart(Tuning::default());
    assert_eq!(flag_in_source_list(&rig), json!(false));
    set_cache(&rig, "g", true);
    let rig = rig.restart(Tuning::default());
    assert_eq!(flag_in_source_list(&rig), json!(true));
}

/// GP8: with the cache off a fetched file is gone once idle for the window,
/// with the cache far under its limit; state `remote`; listing intact.
#[test]
fn with_the_cache_off_a_file_is_dropped_after_the_idle_window_even_under_the_cap() {
    let rig = FetchRig::start(|_| {});
    set_cache(&rig, "g", false);
    let listing = rig.d.entries("g").len();
    ok(&rig.fetch("README.md"));
    assert_eq!(cache_status(&rig.d)["over_limit"], json!(false));
    rig.clock.advance(Duration::from_secs(599));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        rig.row("README.md").state,
        "cached",
        "not before the window"
    );
    rig.clock.advance(Duration::from_secs(2));
    // The row goes first and the file right after.
    rig.d.wait_for("dropped", || {
        (rig.row("README.md").state == "remote" && !rig.cached_path("README.md").exists())
            .then_some(())
    });
    assert_eq!(rig.d.entries("g").len(), listing, "listing intact");
}

/// Turning it back on stops the time-based drops.
#[test]
fn turning_the_cache_back_on_stops_the_drops() {
    let rig = FetchRig::start(|_| {});
    set_cache(&rig, "g", false);
    ok(&rig.fetch("README.md"));
    rig.clock.advance(Duration::from_secs(300));
    set_cache(&rig, "g", true);
    rig.clock.advance(Duration::from_secs(3000));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.row("README.md").state, "cached");
    assert!(rig.cached_path("README.md").exists());
}

/// A fetch makes the file young again, so the off flag drops it a window
/// after the last use, not after the first.
#[test]
fn the_window_runs_from_the_last_use() {
    let rig = FetchRig::start(|_| {});
    set_cache(&rig, "g", false);
    ok(&rig.fetch("README.md"));
    rig.clock.advance(Duration::from_secs(500));
    ok(&rig.fetch("README.md"));
    rig.clock.advance(Duration::from_secs(500));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.row("README.md").state, "cached", "used 500 s ago");
    rig.clock.advance(Duration::from_secs(101));
    rig.d.wait_for("dropped", || {
        (rig.row("README.md").state == "remote").then_some(())
    });
}

/// E22: cache off with pins keeps the pinned files (a pinned file, and a
/// file under a pinned folder), and drops the rest.
#[test]
fn pins_win_over_off() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("docs/Guide.md"));
    ok(&rig.fetch("README.md"));
    ok(&rig.fetch("src/lib.rs"));
    exec_sql(
        &rig.d.cache_path,
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES
            ('g', 'docs', 1, 1), ('g', 'src/lib.rs', 0, 1);",
    );
    set_cache(&rig, "g", false);
    rig.clock.advance(Duration::from_secs(700));
    rig.d.wait_for("the unpinned file is dropped", || {
        (rig.row("README.md").state == "remote").then_some(())
    });
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.row("docs/Guide.md").state, "cached");
    assert_eq!(rig.row("src/lib.rs").state, "cached");
    assert!(rig.cached_path("docs/Guide.md").exists());
    assert!(rig.cached_path("src/lib.rs").exists());
}

/// `pending_upload` and `not_pushed` stay too, flag off or not.
#[test]
fn off_never_drops_an_edit_that_is_not_sent() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    ok(&rig.fetch("src/lib.rs"));
    exec_sql(
        &rig.d.cache_path,
        "UPDATE remote_items SET state = 'pending_upload' WHERE path = 'README.md';
         UPDATE remote_items SET state = 'not_pushed' WHERE path = 'src/lib.rs';",
    );
    set_cache(&rig, "g", false);
    rig.clock.advance(Duration::from_secs(700));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.row("README.md").state, "pending_upload");
    assert_eq!(rig.row("src/lib.rs").state, "not_pushed");
    assert!(rig.cached_path("README.md").exists());
}

#[test]
fn an_unknown_or_local_source_is_refused_with_a_clear_error() {
    let rig = FetchRig::start(|_| {});
    let vault = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("note.md"), "# Note\n").unwrap();
    rig.d.ok(
        "source.register",
        json!({"name": "vault", "root": vault.path().display().to_string()}),
    );
    let local = rig
        .d
        .rpc("source.set_cache", json!({"name": "vault", "cache": false}));
    assert_eq!(local["error"]["code"], json!(-32011), "{local}");
    assert_eq!(local["error"]["data"]["reason"], json!("not_remote"));
    assert!(local["error"]["message"]
        .as_str()
        .unwrap()
        .contains("local"));

    let unknown = rig
        .d
        .rpc("source.set_cache", json!({"name": "nope", "cache": false}));
    assert_eq!(
        unknown["error"]["data"]["reason"],
        json!("unknown_source"),
        "{unknown}"
    );
    assert!(unknown["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nope"));

    for params in [
        json!({"name": "g"}),
        json!({"cache": true}),
        json!({"name": "g", "cache": "off"}),
    ] {
        let reply = rig.d.rpc("source.set_cache", params.clone());
        assert_eq!(reply["error"]["code"], json!(-32602), "{params}: {reply}");
    }
    assert_eq!(flag_in_source_list(&rig), json!(true), "nothing changed");
}
