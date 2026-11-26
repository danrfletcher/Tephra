//! PR-12 review R1: a pin follows what it names when Google Drive renames or
//! moves it. A folder pin follows its renamed folder (and so covers the
//! files that come later), a file pin follows its renamed file, the states
//! and `unit.list` agree with the pin rows, unpinning by the new name works,
//! and a pin never follows a file that only left its folder. Real poller,
//! registry and RPC server against the PR-3 fake Drive on a fake clock.
#![allow(dead_code)]

mod remote_support;

use remote_support::{cache_status, item_rows, DriveDaemon, MemStore, Tuning};
use serde_json::{json, Value};
use tephra_remote::testing::FakeDrive;

fn tree(d: &FakeDrive) {
    d.add_folder("fld", "Notes", None);
    d.add_folder("trip", "Trip Plans", Some("fld"));
    d.add_folder("other", "Other", Some("fld"));
    d.add_file("a", "a.md", "text/markdown", Some("trip"), b"# a\n");
    d.add_file("b", "b.md", "text/markdown", Some("trip"), b"# bb\n");
    d.add_file("o1", "o1.md", "text/markdown", Some("other"), b"# o1\n");
    d.add_file("o2", "o2.md", "text/markdown", Some("other"), b"# o2\n");
}

fn start() -> DriveDaemon {
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1GB"),
            ..Tuning::default()
        },
        None,
        tree,
    );
    dd.add_active("gdrive", "fld");
    dd
}

fn call(dd: &DriveDaemon, method: &str, unit: &str) -> Value {
    let reply =
        dd.d.rpc(method, json!({ "unit": format!("@gdrive:{unit}") }));
    assert!(reply.get("error").is_none(), "{reply}");
    reply["result"].clone()
}

fn pins(dd: &DriveDaemon) -> Vec<(String, bool)> {
    tephra_cache::Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap()
        .into_iter()
        .map(|p| (p.path, p.is_folder))
        .collect()
}

fn states(dd: &DriveDaemon) -> Vec<(String, String)> {
    item_rows(&dd.d.cache_path, "gdrive")
        .into_iter()
        .map(|r| (r.path, r.state))
        .collect()
}

fn state_of(dd: &DriveDaemon, path: &str) -> Option<String> {
    states(dd)
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, s)| s)
}

fn wait_state(dd: &DriveDaemon, path: &str, state: &str) {
    dd.d.wait_for(&format!("{path} to be {state}"), || {
        (state_of(dd, path).as_deref() == Some(state)).then_some(())
    });
}

fn listed_pinned(dd: &DriveDaemon, rel_path: &str) -> Option<bool> {
    dd.d.entries("gdrive")
        .iter()
        .find(|e| e["rel_path"] == json!(rel_path))
        .map(|e| e.get("pinned") == Some(&json!(true)))
}

#[test]
fn a_renamed_pinned_folder_and_file_keep_their_pins_and_the_folder_covers_later_files() {
    let dd = start();
    call(&dd, "unit.pin", "Trip Plans/");
    call(&dd, "unit.pin", "Other/o1.md");
    for p in ["Trip Plans/a.md", "Trip Plans/b.md", "Other/o1.md"] {
        wait_state(&dd, p, "pinned");
    }
    assert_eq!(
        pins(&dd),
        [
            ("Other/o1.md".to_string(), false),
            ("Trip Plans".to_string(), true)
        ]
    );

    dd.drive.rename_file("trip", "Trips");
    dd.drive.rename_file("o1", "one.md");
    dd.poll();

    // The pin rows say the new names, and only those.
    assert_eq!(
        pins(&dd),
        [
            ("Other/one.md".to_string(), false),
            ("Trips".to_string(), true)
        ]
    );
    // Every moved file is fetched again under its new name and is pinned;
    // nothing is left under the old names.
    for p in ["Trips/a.md", "Trips/b.md", "Other/one.md"] {
        wait_state(&dd, p, "pinned");
    }
    assert!(state_of(&dd, "Trip Plans/a.md").is_none());
    assert!(state_of(&dd, "Other/o1.md").is_none());
    // The file next to the renamed one was never pinned.
    assert_eq!(state_of(&dd, "Other/o2.md").as_deref(), Some("remote"));
    // unit.list and cache status agree with the pins.
    assert_eq!(listed_pinned(&dd, "Trips"), Some(true));
    assert_eq!(listed_pinned(&dd, "Trips/a.md"), Some(true));
    assert_eq!(listed_pinned(&dd, "Other/one.md"), Some(true));
    assert_eq!(listed_pinned(&dd, "Other/o2.md"), Some(false));
    let status = cache_status(&dd.d);
    assert_eq!(
        status["pinned_bytes"],
        json!(b"# a\n".len() + b"# bb\n".len() + b"# o1\n".len()),
        "{status}"
    );

    // A file added later under the renamed folder is fetched (G11).
    dd.drive
        .add_file("n", "new.md", "text/markdown", Some("trip"), b"# new\n");
    dd.poll();
    wait_state(&dd, "Trips/new.md", "pinned");

    // A file that now takes the old folder's name is not pinned.
    dd.drive.add_folder("again", "Trip Plans", Some("fld"));
    dd.drive
        .add_file("t", "t.md", "text/markdown", Some("again"), b"# t\n");
    dd.poll();
    wait_state(&dd, "Trip Plans/t.md", "remote");
    assert_eq!(listed_pinned(&dd, "Trip Plans/t.md"), Some(false));

    // Unpinning by the new name works and frees the copies for eviction.
    assert_eq!(
        call(&dd, "unit.unpin", "Trips/")["status"],
        json!("unpinned")
    );
    assert_eq!(
        call(&dd, "unit.unpin", "Other/one.md")["status"],
        json!("unpinned")
    );
    assert!(pins(&dd).is_empty());
    for p in ["Trips/a.md", "Trips/b.md", "Trips/new.md", "Other/one.md"] {
        assert_eq!(state_of(&dd, p).as_deref(), Some("cached"), "{p}");
    }
    dd.d.close();
}

#[test]
fn a_file_moved_out_of_a_pinned_folder_does_not_take_the_pin_along() {
    let dd = start();
    call(&dd, "unit.pin", "Trip Plans/");
    wait_state(&dd, "Trip Plans/a.md", "pinned");
    wait_state(&dd, "Trip Plans/b.md", "pinned");

    dd.drive.move_file("a", "other");
    dd.poll();

    // The pin stays on its folder; the moved file is no longer covered.
    assert_eq!(pins(&dd), [("Trip Plans".to_string(), true)]);
    assert_eq!(state_of(&dd, "Trip Plans/b.md").as_deref(), Some("pinned"));
    assert!(state_of(&dd, "Trip Plans/a.md").is_none());
    assert_eq!(state_of(&dd, "Other/a.md").as_deref(), Some("remote"));
    assert_eq!(listed_pinned(&dd, "Other/a.md"), Some(false));
    dd.d.close();
}

#[test]
fn two_pinned_files_that_swap_names_swap_their_pins() {
    let dd = start();
    call(&dd, "unit.pin", "Other/o1.md");
    wait_state(&dd, "Other/o1.md", "pinned");

    // Swapped names: the pin stays with the file, not with the name.
    dd.drive.rename_file("o1", "o2.md");
    dd.drive.rename_file("o2", "o1.md");
    dd.poll();

    assert_eq!(pins(&dd), [("Other/o2.md".to_string(), false)]);
    wait_state(&dd, "Other/o2.md", "pinned");
    assert_eq!(state_of(&dd, "Other/o1.md").as_deref(), Some("remote"));
    assert_eq!(
        std::fs::read(dd.root().join("Other/o2.md")).unwrap(),
        b"# o1\n"
    );
    dd.d.close();
}
