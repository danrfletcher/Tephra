//! PR-8 (G12, GP8, E4, E5): the cap on the remote content cache, end to end
//! against the fake GitHub server, a temp data dir and an injected clock.
//!
//! The idle window is the product's 600 s; the fake clock moves past it.
//! The eviction timer ticks every 20 ms of real time and reads the fake
//! clock, so a test advances the clock and waits for the result.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{
    big_file, cache_status, exec_sql, files_under, DriveRig, FetchRig, Tuning, REPO,
};
use serde_json::{json, Value};

/// A little under 1 MB, so that two of three files plus a few small
/// protected ones fit under a 1 MB cap and the third does not.
const BIG: usize = 999_900;

fn capped(limit: &'static str) -> Tuning {
    Tuning {
        limit: Some(limit),
        idle_secs: 600,
        ..Tuning::default()
    }
}

fn three_big(fake: &remote_support::Fake) {
    for n in 0..3 {
        let (path, bytes) = big_file(n, BIG);
        fake.gh.put_file(REPO, "main", &path, &bytes);
    }
}

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

fn state(rig: &FetchRig, path: &str) -> String {
    rig.row(path).state
}

/// Fetches `path`, then moves the clock on so the next file is younger.
fn fetch_and_age(rig: &FetchRig, path: &str) {
    ok(&rig.fetch(path));
    rig.clock.advance(Duration::from_secs(5));
}

fn past_idle(rig: &FetchRig) {
    rig.clock.advance(Duration::from_secs(601));
}

fn entry_paths(rig: &FetchRig) -> Vec<String> {
    let mut v: Vec<String> = rig
        .d
        .entries("g")
        .iter()
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

/// Seeds GP8's protected files: `docs/` pinned (as PR-12 will) and
/// `README.md` with an edit waiting to be sent (as PR-9 will).
fn protect(rig: &FetchRig) {
    ok(&rig.fetch("docs/Guide.md"));
    ok(&rig.fetch("README.md"));
    exec_sql(
        &rig.d.cache_path,
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('g', 'docs', 1, 1);
         INSERT INTO upload_queue (source, path, base_revision, attempts, created_at)
            VALUES ('g', 'README.md', 'base', 0, 1795132000);
         UPDATE remote_items SET state = 'pending_upload' WHERE source = 'g' AND path = 'README.md';",
    );
}

/// GP8: a 1 MB cap, 3 MB fetched; over the cap before the idle time, the
/// least recently used files go back to `remote` after it, and nothing
/// else changes.
#[test]
fn over_the_cap_the_oldest_files_go_back_to_remote_once_idle_and_the_listing_is_unchanged() {
    let rig = FetchRig::start_capped(capped("1MB"), three_big);
    protect(&rig);
    let listing = entry_paths(&rig);
    for n in 0..3 {
        fetch_and_age(&rig, &format!("big/{n}.bin"));
    }

    // Before the idle time: over the limit, and nothing dropped.
    let status = cache_status(&rig.d);
    assert_eq!(status["over_limit"], json!(true), "{status}");
    assert_eq!(status["limit_bytes"], json!(1_000_000));
    assert!(
        status["cache_bytes"].as_u64().unwrap() > 2_999_000,
        "{status}"
    );
    std::thread::sleep(Duration::from_millis(200)); // several ticks
    for n in 0..3 {
        assert_eq!(state(&rig, &format!("big/{n}.bin")), "cached");
    }

    past_idle(&rig);
    // The row goes first and the file right after, so wait for both.
    rig.d.wait_for("the cache is at or under the limit", || {
        (cache_status(&rig.d)["over_limit"] == json!(false)
            && !rig.cached_path("big/0.bin").exists()
            && !rig.cached_path("big/1.bin").exists())
        .then_some(())
    });

    // Oldest first: 0 and 1 are gone, 2 (the newest) stays.
    assert_eq!(state(&rig, "big/0.bin"), "remote");
    assert_eq!(state(&rig, "big/1.bin"), "remote");
    assert_eq!(state(&rig, "big/2.bin"), "cached");
    assert!(
        !rig.cached_path("big/0.bin").exists(),
        "no path, no placeholder"
    );
    assert!(!rig.cached_path("big/1.bin").exists());
    assert!(rig.cached_path("big/2.bin").exists());
    // The protected ones stay, and on disk.
    assert_eq!(state(&rig, "README.md"), "pending_upload");
    assert!(rig.cached_path("README.md").exists());
    assert!(rig.cached_path("docs/Guide.md").exists());
    // The listing is as it was, with the sizes; backlinks are untouched.
    assert_eq!(entry_paths(&rig), listing);
    let status = cache_status(&rig.d);
    assert!(
        status["cache_bytes"].as_u64().unwrap() <= 1_000_000,
        "{status}"
    );
    assert_eq!(status["over_limit"], json!(false));
    let backlinks = rig
        .d
        .ok("graph.get_backlinks", json!({"id": "@g:docs/Guide.md"}));
    assert!(backlinks.is_array(), "{backlinks}");
}

/// GP8 / E5: the dropped items stay listed as `remote` with their size, and
/// one fetch gives the file back, with nothing failing.
#[test]
fn a_dropped_file_is_listed_as_remote_and_can_be_fetched_again() {
    let rig = FetchRig::start_capped(capped("1MB"), three_big);
    for n in 0..3 {
        fetch_and_age(&rig, &format!("big/{n}.bin"));
    }
    past_idle(&rig);
    rig.d.wait_for("dropped", || {
        (state(&rig, "big/0.bin") == "remote").then_some(())
    });
    let entry = rig
        .d
        .entries("g")
        .into_iter()
        .find(|e| e["rel_path"] == json!("big/0.bin"))
        .expect("still listed");
    assert_eq!(entry["state"], json!("remote"));
    assert_eq!(entry["cache_bytes"], json!(0));
    assert_eq!(entry["size"], json!(BIG));

    let reply = rig.fetch("big/0.bin");
    assert_eq!(ok(&reply)["state"], json!("cached"));
    assert_eq!(
        std::fs::read(rig.cached_path("big/0.bin")).unwrap().len(),
        BIG
    );
}

/// E5: a fetch is never refused or slowed by the cap, even over it, and a
/// file used in the last 10 minutes is not dropped; once the window ends
/// the drop happens by itself.
#[test]
fn a_recently_used_file_is_kept_a_fetch_never_fails_over_the_cap_and_the_drop_needs_no_new_fetch() {
    let rig = FetchRig::start_capped(capped("1MB"), three_big);
    for n in 0..3 {
        ok(&rig.fetch(&format!("big/{n}.bin")));
    }
    // Over the cap by a wide margin; one more fetch still works.
    ok(&rig.fetch("README.md"));
    assert_eq!(cache_status(&rig.d)["over_limit"], json!(true));
    // 599 s: inside the window.
    rig.clock.advance(Duration::from_secs(599));
    std::thread::sleep(Duration::from_millis(200));
    for n in 0..3 {
        assert_eq!(state(&rig, &format!("big/{n}.bin")), "cached");
    }
    // One more second ends it; nobody fetches anything.
    rig.clock.advance(Duration::from_secs(2));
    rig.d.wait_for("drops without a fetch", || {
        (cache_status(&rig.d)["over_limit"] == json!(false)).then_some(())
    });
    let remote = (0..3)
        .filter(|n| state(&rig, &format!("big/{n}.bin")) == "remote")
        .count();
    assert!(remote >= 2, "at least two of three go: {remote}");
}

/// A fetch of a file the pass has put back touches `last_used`, so the file
/// is a young one again and is the last to go.
#[test]
fn a_fetch_makes_a_file_young_again() {
    let rig = FetchRig::start_capped(capped("1MB"), three_big);
    for n in 0..3 {
        fetch_and_age(&rig, &format!("big/{n}.bin"));
    }
    // Used again just before the window ends for the others.
    rig.clock.advance(Duration::from_secs(500));
    ok(&rig.fetch("big/0.bin"));
    rig.clock.advance(Duration::from_secs(120));
    rig.d.wait_for("dropped to the cap", || {
        (cache_status(&rig.d)["over_limit"] == json!(false)).then_some(())
    });
    assert_eq!(state(&rig, "big/0.bin"), "cached", "used 120 s ago");
    assert_eq!(state(&rig, "big/1.bin"), "remote");
    assert_eq!(state(&rig, "big/2.bin"), "remote");
}

/// E4: pins alone take more than the cap: `cache status` warns, nothing
/// pinned goes, no error, and the unpinned go first.
#[test]
fn pins_over_the_cap_warn_keep_the_pinned_and_drop_the_unpinned_first() {
    let rig = FetchRig::start_capped(capped("1MB"), |fake| {
        three_big(fake);
        for n in 3..5 {
            let (path, bytes) = big_file(n, BIG);
            fake.gh
                .put_file(REPO, "main", &format!("pinned/{n}.bin"), &bytes);
            let _ = path;
        }
    });
    ok(&rig.fetch("pinned/3.bin"));
    ok(&rig.fetch("pinned/4.bin"));
    exec_sql(
        &rig.d.cache_path,
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('g', 'pinned', 1, 1);",
    );
    fetch_and_age(&rig, "big/0.bin");
    past_idle(&rig);
    rig.d.wait_for("the unpinned file goes", || {
        (state(&rig, "big/0.bin") == "remote").then_some(())
    });
    let status = cache_status(&rig.d);
    assert_eq!(
        status["over_limit"],
        json!(true),
        "pins alone are over: {status}"
    );
    assert!(
        status["warning"].as_str().unwrap().contains("pinned"),
        "{status}"
    );
    assert_eq!(state(&rig, "pinned/3.bin"), "cached");
    assert_eq!(state(&rig, "pinned/4.bin"), "cached");
    assert!(rig.cached_path("pinned/3.bin").exists());
    assert!(status.get("error").is_none());
}

/// E4, the other way in: `set-limit` below the pinned total evicts the
/// unpinned files, no error.
#[test]
fn set_limit_below_the_pinned_total_drops_the_unpinned_and_keeps_the_pinned() {
    let rig = FetchRig::start(|fake| {
        three_big(fake);
    });
    for n in 0..3 {
        fetch_and_age(&rig, &format!("big/{n}.bin"));
    }
    exec_sql(
        &rig.d.cache_path,
        "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('g', 'big/0.bin', 0, 1);",
    );
    past_idle(&rig);
    // 10 GB: nothing to do.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(state(&rig, "big/1.bin"), "cached");

    let reply = rig.d.ok("cache.set_limit", json!({"limit": "0.5 MB"}));
    assert_eq!(reply["dropped"], json!(2), "{reply}");
    assert_eq!(reply["over_limit"], json!(true));
    assert!(reply["warning"].is_string(), "{reply}");
    assert_eq!(state(&rig, "big/0.bin"), "cached");
    assert_eq!(state(&rig, "big/1.bin"), "remote");
    assert_eq!(state(&rig, "big/2.bin"), "remote");
}

/// `0` is accepted and evicts everything eligible; the file used now is
/// still inside the window.
#[test]
fn a_zero_limit_evicts_everything_eligible() {
    let rig = FetchRig::start(three_big);
    protect(&rig);
    for n in 0..3 {
        ok(&rig.fetch(&format!("big/{n}.bin")));
    }
    past_idle(&rig);
    let reply = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(reply["limit_bytes"], json!(0), "{reply}");
    assert_eq!(reply["previous"], json!("10 GB"));
    assert_eq!(reply["dropped"], json!(3), "{reply}");
    for n in 0..3 {
        assert_eq!(state(&rig, &format!("big/{n}.bin")), "remote");
    }
    // Pinned and unsent files stay.
    assert_eq!(state(&rig, "README.md"), "pending_upload");
    assert!(rig.cached_path("docs/Guide.md").exists());
    assert!(rig.cached_path("README.md").exists());
}

/// A limit that does not parse is refused with a reason and the old limit
/// stays; every shape the spec lists.
#[test]
fn a_limit_that_does_not_parse_is_refused_and_the_old_one_is_kept() {
    let rig = FetchRig::start(|_| {});
    let huge = format!("{} GB", u64::MAX);
    for bad in [
        "abc",
        "-5MB",
        "1.5.2GB",
        "",
        "   ",
        "500",
        "1 TB",
        huge.as_str(),
    ] {
        let reply = rig.d.rpc("cache.set_limit", json!({ "limit": bad }));
        let error = &reply["error"];
        assert_eq!(error["code"], json!(-32011), "{bad:?}: {reply}");
        assert_eq!(
            error["data"]["reason"],
            json!("bad_limit"),
            "{bad:?}: {reply}"
        );
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("10 GB"), "names the kept limit: {message}");
        let status = cache_status(&rig.d);
        assert_eq!(status["limit"], json!("10 GB"), "{bad:?}");
        assert_eq!(status["limit_bytes"], json!(10_000_000_000u64));
    }
    // A missing or wrongly typed value is a parameter error, not a change.
    let reply = rig.d.rpc("cache.set_limit", json!({}));
    assert_eq!(reply["error"]["code"], json!(-32602), "{reply}");
    let reply = rig.d.rpc("cache.set_limit", json!({"limit": 5}));
    assert_eq!(reply["error"]["code"], json!(-32602), "{reply}");
}

/// The units: MB and GB are 1000-based, MiB and GiB 1024-based.
#[test]
fn units_are_the_ones_the_config_file_takes() {
    let rig = FetchRig::start(|_| {});
    for (text, bytes) in [
        ("1MB", 1_000_000u64),
        ("2 GB", 2_000_000_000),
        ("1MiB", 1_048_576),
        ("1 GiB", 1_073_741_824),
        ("1.5 GB", 1_500_000_000),
    ] {
        let reply = rig.d.ok("cache.set_limit", json!({ "limit": text }));
        assert_eq!(reply["limit_bytes"], json!(bytes), "{text}");
        assert_eq!(reply["limit"], json!(text));
    }
}

/// The limit lives through a restart: the saver is what `config.toml`
/// is for the daemon.
#[test]
fn set_limit_survives_a_restart() {
    let rig = FetchRig::start(|_| {});
    rig.d.ok("cache.set_limit", json!({"limit": "250 MB"}));
    let rig = rig.restart(Tuning::default());
    let status = cache_status(&rig.d);
    assert_eq!(status["limit"], json!("250 MB"), "{status}");
    assert_eq!(status["limit_bytes"], json!(250_000_000));
}

/// `cache status` puts `.git` bytes on a line of its own, not counted, and
/// a source with git data in its clone is not evicted from.
#[test]
fn git_bytes_are_reported_apart_and_not_counted() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    let before = cache_status(&rig.d);
    let git = rig.d.data_dir.join("remote/g/.git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("pack"), vec![0u8; 4096]).unwrap();
    let after = cache_status(&rig.d);
    assert_eq!(after["git_bytes"], json!(4096), "{after}");
    assert_eq!(
        after["cache_bytes"], before["cache_bytes"],
        ".git is not counted"
    );
    assert_eq!(after["sources"][0]["git_bytes"], json!(4096));
    let row = rig.d.source_row("g").unwrap();
    assert_eq!(row["git_bytes"], json!(4096));
}

/// "The cache under the limit and no changes writes nothing to cache.db":
/// idle passes write nothing, byte for byte.
#[test]
fn passes_with_nothing_to_drop_write_nothing_to_the_index() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    rig.clock.advance(Duration::from_secs(3600));
    let before = remote_support::index_snapshot(&rig.d.cache_path);
    let mtime = std::fs::metadata(&rig.d.cache_path)
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300)); // many ticks
    assert_eq!(remote_support::index_snapshot(&rig.d.cache_path), before);
    assert_eq!(
        std::fs::metadata(&rig.d.cache_path)
            .unwrap()
            .modified()
            .unwrap(),
        mtime,
        "the database file was not written"
    );
}

/// A file with no row-visible edit but different bytes on disk is an edit
/// Tephra has not been told of (the watcher has not reported it): it is
/// kept, however idle and however over the cap.
#[test]
fn a_cached_file_whose_bytes_changed_is_kept() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    std::fs::write(rig.cached_path("README.md"), b"# my own edit\n").unwrap();
    past_idle(&rig);
    let reply = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(reply["dropped"], json!(0), "{reply}");
    assert_eq!(state(&rig, "README.md"), "cached");
    assert_eq!(
        std::fs::read(rig.cached_path("README.md")).unwrap(),
        b"# my own edit\n"
    );
}

/// Eviction works on a Drive source too (its copies sit straight under the
/// source's folder). A Drive copy is judged by the hash noted when it was
/// downloaded, so these two are fetched for real.
#[test]
fn a_drive_copy_is_dropped_by_the_same_rules() {
    let t = DriveRig::start(
        &[("id-a", b"hello"), ("id-b", b"abc")],
        &[
            ("notes/a.txt", "file", "id-a", 5),
            ("b.txt", "file", "id-b", 3),
        ],
    );
    ok(&t.fetch("notes/a.txt"));
    ok(&t.fetch("b.txt"));
    let dir = t.root();
    assert!(dir.join("notes/a.txt").exists());
    t.rig.clock.advance(Duration::from_secs(700));
    let reply = t.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(reply["dropped"], json!(2), "{reply}");
    let left: Vec<_> = files_under(&dir);
    assert!(left.is_empty(), "no copy and no placeholder: {left:?}");
    assert!(!dir.join("notes").exists(), "the empty folder went");
    assert!(dir.exists(), "the cache root stays");
    assert_eq!(t.state("b.txt"), "remote");
}

/// A Drive copy with no hash noted (nothing says what Tephra wrote) is
/// never dropped: nothing is deleted on a doubt.
#[test]
fn a_drive_copy_with_no_noted_hash_is_kept() {
    let t = DriveRig::start(&[("id-a", b"hello")], &[("a.txt", "file", "id-a", 5)]);
    ok(&t.fetch("a.txt"));
    exec_sql(&t.rig.d.cache_path, "DELETE FROM remote_copy_stamps;");
    t.rig.clock.advance(Duration::from_secs(700));
    let reply = t.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(reply["dropped"], json!(0), "{reply}");
    assert_eq!(std::fs::read(t.root().join("a.txt")).unwrap(), b"hello");
    assert_eq!(t.state("a.txt"), "cached");
}
