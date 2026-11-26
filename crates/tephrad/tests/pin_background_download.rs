//! PR-12 (G11, GP9, E2, E7, E16): a pin downloads in the background through
//! the `unit.fetch` path, a folder pin covers files added later, failures
//! go back to `remote` and are retried after the next sync, and pins and
//! half-done pins survive a restart. Real registry, RPC server and poller
//! against the PR-3 fake Drive on a fake clock: nothing sleeps.
#![allow(dead_code)]

mod remote_support;

use std::sync::Arc;
use std::time::Duration;

use remote_support::{cache_status, item_rows, DriveDaemon, MemStore, ProbeDisk, Tuning};
use serde_json::{json, Value};

const FILES: usize = 40;

fn body(n: usize) -> Vec<u8> {
    format!("# Trip {n}\n{}\n", "x".repeat(100 + n)).into_bytes()
}

fn trip_drive(drive: &tephra_remote::testing::FakeDrive) {
    drive.add_folder("fld", "Notes", None);
    drive.add_folder("trip", "Trip Plans", Some("fld"));
    drive.add_folder("other", "Other", Some("fld"));
    for n in 0..FILES {
        drive.add_file(
            &format!("t{n:02}"),
            &format!("f{n:02}.md"),
            "text/markdown",
            Some("trip"),
            &body(n),
        );
    }
    drive.add_file("o1", "o1.md", "text/markdown", Some("other"), b"# other\n");
}

fn total() -> u64 {
    (0..FILES).map(|n| body(n).len() as u64).sum()
}

fn start() -> DriveDaemon {
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1GB"),
            ..Tuning::default()
        },
        None,
        trip_drive,
    );
    dd.add_active("gdrive", "fld");
    dd
}

fn pin(dd: &DriveDaemon, unit: &str) -> Value {
    dd.d.rpc(
        "unit.pin",
        json!({ "unit": format!("@gdrive:{unit}"), "confirm": true }),
    )
}

fn states_under(dd: &DriveDaemon, prefix: &str) -> Vec<String> {
    item_rows(&dd.d.cache_path, "gdrive")
        .into_iter()
        .filter(|r| r.path.starts_with(prefix) && r.kind.as_deref() != Some("folder"))
        .map(|r| r.state)
        .collect()
}

fn wait_all(dd: &DriveDaemon, prefix: &str, state: &str, count: usize) {
    dd.d.wait_for(
        &format!("{count} files under {prefix} to be {state}"),
        || {
            let states = states_under(dd, prefix);
            (states.len() == count && states.iter().all(|s| s == state)).then_some(())
        },
    );
}

#[test]
fn gp9_a_folder_pin_downloads_the_files_and_a_later_file_is_fetched_on_the_next_sync() {
    let dd = start();
    let before = dd.media_reads();
    let reply = pin(&dd, "Trip Plans/");
    assert!(reply.get("error").is_none(), "{reply}");
    let result = &reply["result"];
    assert_eq!(result["status"], json!("pinned"), "{result}");
    assert_eq!(result["kind"], json!("folder"));
    assert_eq!(result["files"], json!(FILES));
    assert_eq!(result["total_bytes"], json!(total()));

    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    assert_eq!(dd.media_reads() - before, FILES, "one download per file");
    // `cache status` pinned bytes reach the total.
    let status = cache_status(&dd.d);
    assert_eq!(status["pinned_bytes"], json!(total()), "{status}");
    // `unit.list` shows them pinned.
    let listing = dd.d.entries("gdrive");
    let pinned = listing
        .iter()
        .filter(|e| e["state"] == json!("pinned"))
        .count();
    assert_eq!(pinned, FILES);
    // Nothing outside the pin was requested (F12).
    assert_eq!(states_under(&dd, "Other/"), ["remote"]);
    for n in 0..FILES {
        assert_eq!(
            std::fs::read(dd.root().join(format!("Trip Plans/f{n:02}.md"))).unwrap(),
            body(n)
        );
    }

    // A 41st file; the next sync fetches it with no new pin call.
    dd.drive.add_file(
        "t40",
        "f40.md",
        "text/markdown",
        Some("trip"),
        b"# the 41st\n",
    );
    dd.poll();
    dd.d.wait_for("the 41st file to be pinned", || {
        (dd.row("Trip Plans/f40.md").state == "pinned").then_some(())
    });
    // The new file is read by the index (the poll reads a new text file's
    // body, as the first index does) and by the pin; no other file is read
    // again.
    let reads = dd.drive.request_log().matching("alt=media").len() - before;
    assert_eq!(
        reads,
        FILES + 2,
        "40 pin downloads, then 2 for the new file"
    );
    assert_eq!(dd.drive.request_log().matching("/files/t40").len(), 2);
    let pins = tephra_cache::Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap();
    assert_eq!(pins.len(), 1, "no new row for the 41st");
    dd.d.close();
}

#[test]
fn pin_returns_before_the_files_are_down() {
    let dd = start();
    // Hold the content: the files cannot finish, and pin must still answer.
    dd.drive.hold_content_after(0);
    let reply = pin(&dd, "Trip Plans/");
    assert_eq!(reply["result"]["status"], json!("pinned"), "{reply}");
    let states = states_under(&dd, "Trip Plans/");
    assert!(states.iter().all(|s| s != "pinned"), "{states:?}");
    dd.d.wait_for("a file to be downloading", || {
        states_under(&dd, "Trip Plans/")
            .iter()
            .any(|s| s == "downloading")
            .then_some(())
    });
    dd.drive.release_content();
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    dd.d.close();
}

#[test]
fn a_failed_file_returns_to_remote_with_its_reason_and_the_next_sync_retries_it() {
    let dd = start();
    // The Drive refuses the first read; every other file comes down.
    dd.drive.fail_content_reads(1, 503);
    assert_eq!(pin(&dd, "Trip Plans/")["result"]["status"], json!("pinned"));
    dd.d.wait_for("39 files pinned", || {
        (states_under(&dd, "Trip Plans/")
            .iter()
            .filter(|s| *s == "pinned")
            .count()
            == FILES - 1)
            .then_some(())
    });
    let failed: Vec<_> = item_rows(&dd.d.cache_path, "gdrive")
        .into_iter()
        .filter(|r| r.path.starts_with("Trip Plans/") && r.state == "remote")
        .collect();
    assert_eq!(failed.len(), 1);
    let why = failed[0].reason.clone().unwrap_or_default();
    assert!(why.contains("server error (503)"), "{why}");
    let pins = tephra_cache::Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap();
    assert_eq!(pins.len(), 1, "the pin stays");

    dd.poll();
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    dd.d.close();
}

#[test]
fn disk_full_leaves_no_partial_file_and_the_other_files_still_download() {
    let probe = ProbeDisk::new();
    probe.fail_write_number(1);
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1GB"),
            ..Tuning::default()
        },
        Some(Arc::new(remote_support::ProbeHandle(probe.clone()))),
        trip_drive,
    );
    dd.add_active("gdrive", "fld");
    assert_eq!(pin(&dd, "Trip Plans/")["result"]["status"], json!("pinned"));
    dd.d.wait_for("39 files pinned", || {
        (states_under(&dd, "Trip Plans/")
            .iter()
            .filter(|s| *s == "pinned")
            .count()
            == FILES - 1)
            .then_some(())
    });
    let failed: Vec<_> = item_rows(&dd.d.cache_path, "gdrive")
        .into_iter()
        .filter(|r| r.path.starts_with("Trip Plans/") && r.state == "remote")
        .collect();
    assert_eq!(failed.len(), 1);
    let why = failed[0].reason.clone().unwrap_or_default();
    assert!(why.contains("the disk is full"), "{why}");
    let leftovers: Vec<_> = remote_support::files_under(&dd.root())
        .into_iter()
        .filter(|p| !p.to_string_lossy().contains("Trip Plans/f"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp or partial file: {leftovers:?}"
    );
    assert_eq!(
        remote_support::files_under(&dd.root()).len(),
        FILES - 1,
        "only finished files"
    );
    // The next sync retries it (the disk no longer fails).
    dd.poll();
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    dd.d.close();
}

#[test]
fn an_offline_source_stores_the_pin_and_downloads_start_on_the_next_good_sync() {
    let dd = start();
    let reads_before = dd.media_reads();
    // A rejected token: every read answers 401 until the user signs in.
    dd.drive.revoke_token();
    let reply = pin(&dd, "Trip Plans/");
    // The pin is stored and shows its total even though nothing can come.
    assert_eq!(reply["result"]["status"], json!("pinned"), "{reply}");
    assert_eq!(reply["result"]["total_bytes"], json!(total()));
    dd.d.wait_for("the first attempt to be refused", || {
        (item_rows(&dd.d.cache_path, "gdrive").iter().any(|r| {
            r.reason
                .as_deref()
                .unwrap_or("")
                .contains("tephractl auth google")
        }))
        .then_some(())
    });
    // One refusal stops the pass: the other files were not hammered.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(dd.media_reads() - reads_before, 1);
    assert!(states_under(&dd, "Trip Plans/")
        .iter()
        .all(|s| s == "remote"));
    // The pin shows its total while it waits.
    assert_eq!(
        tephra_cache::Cache::open_read_only(&dd.d.cache_path)
            .unwrap()
            .remote_pins("gdrive")
            .unwrap()
            .len(),
        1
    );

    dd.drive.accept_only_token("ya29.fresh");
    dd.sign_in_again("ya29.fresh");
    dd.poll(); // the poll that sees auth_required again
    dd.poll();
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    dd.d.close();
}

#[test]
fn a_pin_cut_off_by_a_kill_is_filled_in_after_a_restart() {
    let dd = start();
    assert_eq!(pin(&dd, "Trip Plans/")["result"]["status"], json!("pinned"));
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    let root = dd.root();
    // What a kill in the middle of three downloads leaves: the rows say
    // `downloading` and there is no final file.
    let dd = dd.restart_after(|path| {
        remote_support::exec_sql(
            path,
            "UPDATE remote_items SET state = 'downloading' \
             WHERE path IN ('Trip Plans/f01.md', 'Trip Plans/f02.md', 'Trip Plans/f03.md')",
        );
        for n in 1..=3 {
            std::fs::remove_file(root.join(format!("Trip Plans/f{n:02}.md"))).unwrap();
        }
    });
    let before = dd.media_reads();
    wait_all(&dd, "Trip Plans/", "pinned", FILES);
    assert_eq!(dd.media_reads() - before, 3, "only the three that were cut");
    let pins = tephra_cache::Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap();
    assert_eq!(pins.len(), 1, "the pin survived");
    for n in 1..=3 {
        assert_eq!(
            std::fs::read(dd.root().join(format!("Trip Plans/f{n:02}.md"))).unwrap(),
            body(n)
        );
    }
    dd.d.close();
}
