//! PR-12 (E4, E21, E22, GP9's unpin): pins against PR-8's cache limit and
//! per-source cache flag, and against remote deletes. Real registry, RPC
//! server, poller and evictor against the fake Drive on a fake clock:
//! nothing sleeps for the idle window.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::{cache_status, item_rows, save, DriveDaemon, MemStore, Tuning};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_remote::testing::UploadHold;

fn rig(limit: &'static str, build: impl FnOnce(&tephra_remote::testing::FakeDrive)) -> DriveDaemon {
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some(limit),
            ..Tuning::default()
        },
        None,
        build,
    );
    dd.add_active("gdrive", "fld");
    dd
}

fn pin(dd: &DriveDaemon, unit: &str) -> Value {
    let reply = dd.d.rpc(
        "unit.pin",
        json!({ "unit": format!("@gdrive:{unit}"), "confirm": true }),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    reply
}

fn unpin(dd: &DriveDaemon, unit: &str) -> Value {
    dd.d.ok("unit.unpin", json!({ "unit": format!("@gdrive:{unit}") }))
}

fn fetch(dd: &DriveDaemon, path: &str) {
    dd.d.ok("unit.fetch", json!({ "unit": format!("@gdrive:{path}") }));
}

fn set_cache(dd: &DriveDaemon, on: bool) {
    dd.d.ok("source.set_cache", json!({ "name": "gdrive", "cache": on }));
}

fn wait_state(dd: &DriveDaemon, path: &str, state: &str) {
    dd.d.wait_for(&format!("{path} to be {state}"), || {
        (dd.row(path).state == state).then_some(())
    });
}

fn pins(dd: &DriveDaemon) -> Vec<(String, bool)> {
    Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap()
        .into_iter()
        .map(|p| (p.path, p.is_folder))
        .collect()
}

fn small(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("fld", "Notes", None);
    d.add_folder("trip", "Trip Plans", Some("fld"));
    d.add_file("a", "a.md", "text/markdown", Some("trip"), b"# a\n");
    d.add_file("b", "b.md", "text/markdown", Some("trip"), b"# b\n");
    d.add_file("c", "c.md", "text/markdown", Some("fld"), b"# c\n");
    d.add_file("e", "e.md", "text/markdown", Some("fld"), b"# e\n");
}

fn mb(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("fld", "Notes", None);
    d.add_folder("trip", "Trip Plans", Some("fld"));
    for (id, byte) in [("w", 1u8), ("x", 2), ("y", 3), ("z", 4)] {
        d.add_file(
            id,
            &format!("{id}.bin"),
            "application/octet-stream",
            Some("trip"),
            &vec![byte; 300_000],
        );
    }
    d.add_file(
        "u",
        "u.bin",
        "application/octet-stream",
        Some("fld"),
        &vec![9; 100_000],
    );
}

// ---- E4: pins alone exceed the cap -----------------------------------------------------

#[test]
fn e4_pins_over_the_cap_show_over_limit_and_a_warning_and_nothing_pinned_is_evicted() {
    let dd = rig("1MB", mb);
    let reply = pin(&dd, "Trip Plans/");
    assert_eq!(reply["result"]["status"], json!("pinned"));
    // 1 200 000 bytes of pins against a 1 MB cap: the pin call warns.
    let warning = reply["result"]["warning"].as_str().expect("a warning");
    assert!(warning.contains("more than the cache limit"), "{warning}");
    for f in ["w", "x", "y", "z"] {
        wait_state(&dd, &format!("Trip Plans/{f}.bin"), "pinned");
    }
    fetch(&dd, "u.bin");

    let status = cache_status(&dd.d);
    assert_eq!(status["over_limit"], json!(true), "{status}");
    assert_eq!(status["pinned_bytes"], json!(1_200_000));
    let warning = status["warning"].as_str().expect("a warning");
    assert!(warning.contains("pinned files alone take"), "{warning}");

    // Every eviction pass there is: time goes by, the limit is set again.
    dd.advance(Duration::from_secs(3_000));
    let _ = dd.d.ok("cache.set_limit", json!({ "limit": "1MB" }));
    wait_state(&dd, "u.bin", "remote");
    std::thread::sleep(Duration::from_millis(200));
    for f in ["w", "x", "y", "z"] {
        let path = format!("Trip Plans/{f}.bin");
        assert_eq!(dd.row(&path).state, "pinned", "{path}");
        assert!(dd.root().join(&path).exists(), "{path}");
    }
    let status = cache_status(&dd.d);
    assert_eq!(status["over_limit"], json!(true), "still over: {status}");
    assert_eq!(status["pinned_bytes"], json!(1_200_000));
    dd.d.close();
}

// ---- GP9: unpin leaves the files cached but evictable --------------------------------

#[test]
fn gp9_unpin_leaves_the_files_cached_then_the_lru_sends_them_back_to_remote() {
    let dd = rig("1MB", mb);
    pin(&dd, "Trip Plans/");
    for f in ["w", "x", "y", "z"] {
        wait_state(&dd, &format!("Trip Plans/{f}.bin"), "pinned");
    }
    let listing_before = dd.d.entries("gdrive").len();
    let backlinks_before = dd.paths("gdrive");

    let off = unpin(&dd, "Trip Plans/");
    assert_eq!(off["status"], json!("unpinned"));
    assert!(pins(&dd).is_empty());
    for f in ["w", "x", "y", "z"] {
        let path = format!("Trip Plans/{f}.bin");
        assert_eq!(dd.row(&path).state, "cached", "{path}");
        assert!(dd.root().join(&path).exists());
    }
    assert_eq!(cache_status(&dd.d)["pinned_bytes"], json!(0));

    // Under pressure (1.2 MB against 1 MB) and once idle, the least
    // recently used go back to `remote`.
    dd.advance(Duration::from_secs(700));
    // The eviction claims a row (`remote`) before it deletes the file, so the
    // wait is for the rows and the disk to agree, not for the row alone.
    let remote =
        dd.d.wait_for("the evicted files to be gone from the disk", || {
            let remote: Vec<_> = item_rows(&dd.d.cache_path, "gdrive")
                .into_iter()
                .filter(|r| r.path.starts_with("Trip Plans/") && r.state == "remote")
                .collect();
            let settled = cache_status(&dd.d)["over_limit"] == json!(false)
                && !remote.is_empty()
                && remote.iter().all(|r| !dd.root().join(&r.path).exists());
            settled.then_some(remote)
        });
    for r in &remote {
        assert!(!dd.root().join(&r.path).exists(), "{}", r.path);
    }
    // The listing and the rows are all still there.
    assert_eq!(dd.d.entries("gdrive").len(), listing_before);
    assert_eq!(dd.paths("gdrive"), backlinks_before);
    dd.d.close();
}

// ---- E22: the cache flag off, with pins ----------------------------------------------------

#[test]
fn e22_with_the_cache_off_a_pinned_file_stays_and_a_fetched_one_is_dropped() {
    let dd = rig("1GB", small);
    set_cache(&dd, false);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    fetch(&dd, "e.md");
    wait_state(&dd, "e.md", "cached");

    dd.advance(Duration::from_secs(601));
    wait_state(&dd, "e.md", "remote");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(dd.row("c.md").state, "pinned", "pinned files stay");
    assert!(dd.root().join("c.md").exists());
    assert!(!dd.root().join("e.md").exists());
    dd.d.close();
}

#[test]
fn e22_after_unpin_the_file_is_dropped_about_ten_minutes_after_its_last_use() {
    let dd = rig("1GB", small);
    set_cache(&dd, false);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    // Used now (the pin's download); five minutes later it is unpinned.
    dd.advance(Duration::from_secs(300));
    unpin(&dd, "c.md");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(dd.row("c.md").state, "cached", "idle for 300 s only");
    dd.advance(Duration::from_secs(250));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(dd.row("c.md").state, "cached", "idle for 550 s");
    dd.advance(Duration::from_secs(100));
    wait_state(&dd, "c.md", "remote");
    assert!(!dd.root().join("c.md").exists());
    dd.d.close();
}

#[test]
fn a_fetch_of_a_pinned_file_touches_last_used_and_never_unpins() {
    let dd = rig("1GB", small);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    let touched = |dd: &DriveDaemon| -> i64 {
        Cache::open_read_only(&dd.d.cache_path)
            .unwrap()
            .connection()
            .query_row(
                "SELECT last_used FROM remote_items WHERE source = 'gdrive' AND path = 'c.md'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let before = touched(&dd);
    dd.advance(Duration::from_secs(120));
    let reads = dd.media_reads();
    fetch(&dd, "c.md");
    assert!(touched(&dd) > before, "last_used moved");
    assert_eq!(dd.row("c.md").state, "pinned");
    assert_eq!(pins(&dd), [("c.md".to_string(), false)]);
    assert_eq!(dd.media_reads(), reads, "the copy was there: no download");
    dd.d.close();
}

// ---- E21: deleted on the remote -------------------------------------------------------------

#[test]
fn e21_a_pinned_file_deleted_remotely_loses_its_pin_row_with_the_item() {
    let dd = rig("1GB", small);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    dd.drive.trash_file("c");
    dd.poll();
    assert!(!dd.paths("gdrive").contains(&"c.md".to_string()));
    assert!(pins(&dd).is_empty(), "the pin went with the item");
    assert!(!dd.root().join("c.md").exists());
    dd.d.close();
}

#[test]
fn e21_a_file_deleted_inside_a_pinned_folder_drops_only_that_item() {
    let dd = rig("1GB", small);
    pin(&dd, "Trip Plans/");
    wait_state(&dd, "Trip Plans/a.md", "pinned");
    wait_state(&dd, "Trip Plans/b.md", "pinned");
    dd.drive.trash_file("a");
    dd.poll();
    let paths = dd.paths("gdrive");
    assert!(!paths.contains(&"Trip Plans/a.md".to_string()));
    assert!(paths.contains(&"Trip Plans/b.md".to_string()));
    assert_eq!(
        pins(&dd),
        [("Trip Plans".to_string(), true)],
        "the folder pin stays"
    );
    assert_eq!(dd.row("Trip Plans/b.md").state, "pinned");
    // A file added later is still covered.
    dd.drive
        .add_file("n", "n.md", "text/markdown", Some("trip"), b"# n\n");
    dd.poll();
    wait_state(&dd, "Trip Plans/n.md", "pinned");
    dd.d.close();
}

#[test]
fn e21_a_modified_copy_of_a_deleted_pinned_file_is_kept_as_needs_attention() {
    let dd = rig("1GB", small);
    set_cache(&dd, false);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    // The edit is saved and not yet sent (the upload is held at Drive).
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    let file = dd.root().join("c.md");
    save(&file, b"# c\nedited by Dan\n");
    wait_state(&dd, "c.md", "pending_upload");
    // The state precedence of PR-8: a pinned file with an unsent edit.
    let entry =
        dd.d.entries("gdrive")
            .into_iter()
            .find(|e| e["rel_path"] == json!("c.md"))
            .unwrap();
    assert_eq!(entry["state"], json!("pending_upload"), "{entry}");
    assert_eq!(pins(&dd), [("c.md".to_string(), false)]);

    dd.drive.trash_file("c");
    dd.poll();
    let row = dd.row("c.md");
    assert_eq!(row.state, "needs_attention", "{row:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"# c\nedited by Dan\n");
    // Idle for ever, cache off: the unsent-edit guard keeps it.
    dd.advance(Duration::from_secs(5_000));
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(dd.row("c.md").state, "needs_attention");
    assert_eq!(std::fs::read(&file).unwrap(), b"# c\nedited by Dan\n");
    dd.drive.release_uploads();
    dd.d.close();
}

#[test]
fn a_pinned_file_with_an_unsent_edit_is_pending_upload_and_pinned_again_once_sent() {
    let dd = rig("1GB", small);
    pin(&dd, "c.md");
    wait_state(&dd, "c.md", "pinned");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&dd.root().join("c.md"), b"# c\nnew\n");
    wait_state(&dd, "c.md", "pending_upload");
    dd.drive.release_uploads();
    wait_state(&dd, "c.md", "pinned");
    assert_eq!(pins(&dd), [("c.md".to_string(), false)]);
    dd.d.close();
}
