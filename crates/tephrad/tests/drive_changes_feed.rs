//! PR-10 (G5, E8, E11, S5): a Drive source kept current from the changes
//! feed. Every kind of change is applied once; a page that comes twice
//! changes nothing; every page is drained before the position is kept; a
//! position Drive no longer knows starts the index over; a finished index
//! resumes from its position after a restart. Real poller, registry and
//! RPC server against the PR-3 fake Drive on a fake clock.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::{exec_sql, files_under, item_rows, DriveDaemon};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_remote::testing::FakeDrive;

const QUIET: Duration = Duration::from_millis(400);

fn tree(d: &FakeDrive) {
    d.add_folder("f", "Notes", None);
    d.add_file("a", "a.md", "text/markdown", Some("f"), b"# a\n");
    d.add_file("b", "b.md", "text/markdown", Some("f"), b"# b\n");
    d.add_folder("sub", "Sub", Some("f"));
    d.add_file("c", "c.md", "text/markdown", Some("sub"), b"# c\n");
    d.add_file(
        "ref",
        "ref.md",
        "text/markdown",
        Some("f"),
        b"[[a]] [[x]]\n",
    );
    d.add_folder("out", "Outside", None);
    d.add_file("o", "o.md", "text/markdown", Some("out"), b"# o\n");
}

fn started() -> DriveDaemon {
    let rig = DriveDaemon::start(tree);
    rig.add_active("g", "Notes");
    rig
}

fn cursor(rig: &DriveDaemon) -> Option<String> {
    Cache::open_read_only(&rig.d.cache_path)
        .unwrap()
        .remote_poll_state("g")
        .unwrap()
        .and_then(|s| s.commit)
}

fn file_id(rig: &DriveDaemon, path: &str) -> Option<String> {
    item_rows(&rig.d.cache_path, "g")
        .into_iter()
        .find(|r| r.path == path)
        .and_then(|r| r.file_id)
}

fn resolved(rig: &DriveDaemon, from: &str, to_raw: &str) -> bool {
    let forward = rig.d.ok("graph.get_forward_links", json!({ "id": from }));
    forward
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["to_raw"] == json!(to_raw))
        .is_some_and(|e| e["to"].get("resolved").is_some())
}

/// One change of the tree and what it must leave behind.
struct Case {
    name: &'static str,
    change: fn(&FakeDrive),
    paths: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "added",
        change: |d| d.add_file("n", "new.md", "text/markdown", Some("f"), b"# new\n"),
        paths: &["Sub/c.md", "a.md", "b.md", "new.md", "ref.md"],
    },
    Case {
        name: "added in a subfolder",
        change: |d| d.add_file("n", "new.md", "text/markdown", Some("sub"), b"# new\n"),
        paths: &["Sub/c.md", "Sub/new.md", "a.md", "b.md", "ref.md"],
    },
    Case {
        name: "edited",
        change: |d| d.update_file("b", b"# b changed\n"),
        paths: &["Sub/c.md", "a.md", "b.md", "ref.md"],
    },
    Case {
        name: "renamed",
        change: |d| d.rename_file("b", "renamed.md"),
        paths: &["Sub/c.md", "a.md", "ref.md", "renamed.md"],
    },
    Case {
        name: "moved to another parent",
        change: |d| d.move_file("b", "sub"),
        paths: &["Sub/b.md", "Sub/c.md", "a.md", "ref.md"],
    },
    Case {
        name: "moved out of the folder",
        change: |d| d.move_file("b", "out"),
        paths: &["Sub/c.md", "a.md", "ref.md"],
    },
    Case {
        name: "moved in from outside",
        change: |d| d.move_file("o", "f"),
        paths: &["Sub/c.md", "a.md", "b.md", "o.md", "ref.md"],
    },
    Case {
        name: "trashed",
        change: |d| d.trash_file("b"),
        paths: &["Sub/c.md", "a.md", "ref.md"],
    },
    Case {
        name: "deleted",
        change: |d| d.remove_file("b"),
        paths: &["Sub/c.md", "a.md", "ref.md"],
    },
    Case {
        name: "a folder renamed",
        change: |d| d.rename_file("sub", "Renamed"),
        paths: &["Renamed/c.md", "a.md", "b.md", "ref.md"],
    },
    Case {
        name: "a folder trashed",
        change: |d| d.trash_file("sub"),
        paths: &["a.md", "b.md", "ref.md"],
    },
    Case {
        name: "a folder moved out",
        change: |d| d.move_file("sub", "out"),
        paths: &["a.md", "b.md", "ref.md"],
    },
    Case {
        name: "added then removed before the poll",
        change: |d| {
            d.add_file("n", "new.md", "text/markdown", Some("f"), b"# new\n");
            d.remove_file("n");
        },
        paths: &["Sub/c.md", "a.md", "b.md", "ref.md"],
    },
    Case {
        name: "changed outside the folder",
        change: |d| d.add_file("n", "elsewhere.md", "text/markdown", Some("out"), b"# n\n"),
        paths: &["Sub/c.md", "a.md", "b.md", "ref.md"],
    },
];

#[test]
fn every_kind_of_change_is_applied_once_within_one_poll() {
    for case in CASES {
        let rig = started();
        let mut client = rig.d.connect();
        (case.change)(&rig.drive);
        rig.poll();
        assert_eq!(rig.paths("g"), case.paths, "{}", case.name);
        let pushes = client.drain_graph_changed(QUIET);
        let changed = case.paths != ["Sub/c.md", "a.md", "b.md", "ref.md"] || case.name == "edited";
        if changed && case.name != "added then removed before the poll" {
            assert!(pushes >= 1, "{}: a graph:changed was pushed", case.name);
        }
        let row = rig.d.source_row("g").unwrap();
        assert_eq!(row["status"], json!("active"), "{}", case.name);
        rig.d.close();
    }
}

#[test]
fn a_new_file_is_read_once_and_an_edit_replaces_what_was_indexed() {
    let rig = started();
    let reads = rig.media_reads();
    rig.drive.add_file(
        "n",
        "new.md",
        "text/markdown",
        Some("f"),
        b"# new\n\n[[b]]\n",
    );
    rig.poll_applied();
    assert_eq!(rig.media_reads(), reads + 1, "the new file, once");
    assert!(resolved(&rig, "@g:new.md", "b"));

    // Edited: the links it holds are the new ones.
    rig.drive.update_file("n", b"# new\n\n[[a]]\n");
    rig.poll_applied();
    assert_eq!(rig.media_reads(), reads + 2);
    let forward = rig
        .d
        .ok("graph.get_forward_links", json!({"id": "@g:new.md"}));
    let raws: Vec<&str> = forward
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["to_raw"].as_str().unwrap())
        .collect();
    assert_eq!(raws, ["a"]);
    // A poll with nothing in the feed reads nothing and pushes nothing.
    let mut client = rig.d.connect();
    rig.poll();
    assert_eq!(rig.media_reads(), reads + 2);
    assert_eq!(client.drain_graph_changed(QUIET), 0);
    rig.d.close();
}

#[test]
fn a_rename_keeps_the_file_id_updates_the_path_and_is_recorded() {
    let rig = started();
    let id = file_id(&rig, "b.md");
    assert_eq!(id.as_deref(), Some("b"));
    let reads = rig.media_reads();
    rig.drive.rename_file("b", "Beta.md");
    rig.poll();
    assert_eq!(
        rig.media_reads(),
        reads,
        "a rename reads nothing: the parsed file moves with its row"
    );
    assert_eq!(file_id(&rig, "Beta.md"), id);
    assert_eq!(file_id(&rig, "b.md"), None);
    let fixes = rig.d.ok("fix.list", json!({}));
    let entry = &fixes.as_array().unwrap()[0];
    assert_eq!(entry["kind"], json!("rename"), "{fixes}");
    assert_eq!(entry["old"], json!("b.md"));
    assert_eq!(entry["new"], json!("Beta.md"));
    assert_eq!(entry["status"], json!("applied"));
    assert_eq!(entry["files_changed"], json!(0));
    rig.d.close();
}

#[test]
fn a_trashed_or_deleted_file_is_removed_and_its_links_are_unresolved() {
    for how in ["trash", "delete"] {
        let rig = started();
        assert!(resolved(&rig, "@g:ref.md", "a"));
        match how {
            "trash" => rig.drive.trash_file("a"),
            _ => rig.drive.remove_file("a"),
        }
        rig.poll();
        let forward = rig
            .d
            .ok_when("graph.get_forward_links", json!({"id": "@g:ref.md"}), |r| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["to_raw"] == json!("a") && e["to"].get("resolved").is_none())
            });
        assert!(!forward.as_array().unwrap().is_empty());
        assert!(
            rig.d
                .entries("g")
                .iter()
                .all(|e| e["rel_path"] != json!("a.md")),
            "{how}"
        );
        rig.d.close();
    }
}

#[test]
fn a_page_that_comes_twice_changes_nothing() {
    let rig = started();
    let before = cursor(&rig).expect("a cursor");
    rig.drive
        .add_file("n", "new.md", "text/markdown", Some("f"), b"# new\n");
    rig.drive.rename_file("b", "Beta.md");
    rig.drive.remove_file("a");
    rig.poll_applied();
    let after = cursor(&rig).unwrap();
    assert_ne!(after, before, "the position moved");
    let paths = rig.paths("g");
    let items = item_rows(&rig.d.cache_path, "g");
    let fixes = rig.d.ok("fix.list", json!({}));

    // Put the old position back: the same page comes again.
    exec_sql(
        &rig.d.cache_path,
        &format!("UPDATE remote_cursors SET cursor = '{before}'"),
    );
    let mut client = rig.d.connect();
    let reads = rig.media_reads() + rig.exports();
    rig.poll();
    assert_eq!(rig.paths("g"), paths);
    assert_eq!(
        item_rows(&rig.d.cache_path, "g"),
        items,
        "the index rows are the same"
    );
    assert_eq!(
        rig.d.ok("fix.list", json!({})),
        fixes,
        "no second rename record"
    );
    assert_eq!(
        rig.media_reads() + rig.exports(),
        reads,
        "nothing is read twice"
    );
    assert_eq!(
        client.drain_graph_changed(QUIET),
        0,
        "nothing changed, nothing pushed"
    );
    assert_eq!(cursor(&rig).unwrap(), after);
    rig.d.close();
}

#[test]
fn every_page_is_drained_before_the_position_is_kept() {
    let rig = started();
    rig.drive.set_page_size(2);
    let before = cursor(&rig).unwrap();
    for n in 0..7 {
        rig.drive.add_file(
            &format!("n{n}"),
            &format!("n{n}.md"),
            "text/markdown",
            Some("f"),
            b"# n\n",
        );
    }
    rig.poll();
    assert_eq!(rig.paths("g").len(), 11);
    let kept = cursor(&rig).unwrap();
    assert_ne!(kept, before);
    // The kept position is the feed's new start, not a page token: the
    // next poll finds nothing left.
    let reads = rig.media_reads();
    rig.poll();
    assert_eq!(rig.paths("g").len(), 11);
    assert_eq!(rig.media_reads(), reads);
    let pages = rig.drive.request_log().matching("changes?").len();
    assert!(pages >= 4, "7 changes at 2 a page is 4 pages ({pages})");
    rig.d.close();
}

#[test]
fn a_failure_in_the_middle_of_the_feed_keeps_the_old_position_and_the_page_replays() {
    let rig = started();
    rig.drive.set_page_size(2);
    let before = cursor(&rig).unwrap();
    for n in 0..5 {
        rig.drive.add_file(
            &format!("n{n}"),
            &format!("n{n}.md"),
            "text/markdown",
            Some("f"),
            b"# n\n",
        );
    }
    // Two pages arrive, the third fails.
    rig.drive.fail_changes_after(2, 500);
    let paths = rig.paths("g");
    rig.poll();
    assert_eq!(cursor(&rig).unwrap(), before, "the old position is kept");
    assert_eq!(
        rig.paths("g"),
        paths,
        "nothing of the first pages was applied"
    );
    let row = rig.d.source_row("g").unwrap();
    assert_eq!(row["status"], json!("offline"), "{row}");
    // The next poll replays the whole feed and finishes it.
    rig.poll();
    assert_eq!(rig.paths("g").len(), 9);
    assert_ne!(cursor(&rig).unwrap(), before);
    assert_eq!(rig.d.source_row("g").unwrap()["status"], json!("active"));
    rig.d.close();
}

#[test]
fn a_lost_position_starts_the_index_over_from_scratch() {
    let rig = started();
    rig.drive
        .add_file("n", "new.md", "text/markdown", Some("f"), b"# new\n");
    rig.drive.remove_file("b");
    rig.drive.forget_old_tokens();
    let tokens = rig.drive.request_log().matching("startPageToken").len();
    let reads = rig.media_reads();
    rig.poll();
    rig.d.wait_active("g");
    assert!(
        rig.drive.request_log().matching("startPageToken").len() > tokens,
        "a fresh position"
    );
    assert_eq!(rig.paths("g"), vec!["Sub/c.md", "a.md", "new.md", "ref.md"]);
    // Every text file was read again.
    rig.d.wait_for("the reads", || {
        (rig.media_reads() >= reads + 4).then_some(())
    });
    let row = rig.d.source_row("g").unwrap();
    assert_eq!(row["index_done"], row["index_total"], "{row}");
    assert!(files_under(&rig.d.data_dir.join("remote")).is_empty());
    rig.d.close();
}

#[test]
fn a_finished_index_resumes_from_its_position_after_a_restart() {
    let rig = started();
    let kept = cursor(&rig).unwrap();
    rig.drive
        .add_file("n", "new.md", "text/markdown", Some("f"), b"# new\n");
    let rig = rig.restart();
    rig.d.wait_active("g");
    let tokens = rig.drive.request_log().matching("startPageToken").len();
    let listings = rig.drive.request_log().matching("in%20parents").len();
    assert_eq!(cursor(&rig).unwrap(), kept, "the restart did not move it");
    rig.poll();
    assert!(
        rig.paths("g").contains(&"new.md".to_string()),
        "found in the feed"
    );
    assert_eq!(
        rig.drive.request_log().matching("startPageToken").len(),
        tokens
    );
    assert_eq!(
        rig.drive.request_log().matching("in%20parents").len(),
        listings,
        "no re-listing"
    );
    rig.d.close();
}

#[test]
fn a_changed_name_that_collides_gets_a_stable_name_2() {
    let rig = started();
    rig.drive.rename_file("b", "a.md");
    rig.poll();
    assert_eq!(
        rig.paths("g"),
        vec!["Sub/c.md", "a (2).md", "a.md", "ref.md"]
    );
    assert_eq!(file_id(&rig, "a.md").as_deref(), Some("a"));
    assert_eq!(file_id(&rig, "a (2).md").as_deref(), Some("b"));
    rig.d.close();
}

#[test]
fn a_native_doc_edit_is_exported_again_and_a_sheet_edit_reads_nothing() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_doc("g1", "Doc", Some("f"), "# one\n");
        d.add_native(
            "s",
            "Sheet",
            "application/vnd.google-apps.spreadsheet",
            Some("f"),
        );
    });
    rig.add_active("g", "Notes");
    assert_eq!(rig.exports(), 1);
    rig.drive.update_file("g1", b"# two\n\n[[Sheet]]\n");
    rig.drive.update_file("s", b"x");
    rig.poll();
    assert_eq!(rig.exports(), 2, "the Doc only");
    assert_eq!(rig.media_reads(), 0);
    rig.d.close();
}

#[test]
fn the_feed_is_polled_about_every_minute_and_only_reads_are_sent() {
    let rig = started();
    let before = rig.drive.request_log().matching("changes?").len();
    rig.clock.advance(Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        rig.drive.request_log().matching("changes?").len(),
        before,
        "not due yet"
    );
    rig.clock.advance(Duration::from_secs(40));
    rig.d.wait_for("a poll", || {
        (rig.drive.request_log().matching("changes?").len() > before).then_some(())
    });
    for r in rig.drive.request_log().all() {
        assert_eq!(r.method, "GET", "{}", r.target());
    }
    let _: Value = json!(null);
    rig.d.close();
}

#[test]
fn a_renamed_file_keeps_its_links_and_two_files_can_swap_names() {
    let rig = started();
    // ref.md links [[a]]; rename a: the parsed links of a itself move too.
    rig.drive.update_file("b", b"# b\n\n[[a]]\n");
    rig.poll_applied();
    rig.drive.rename_file("b", "Beta.md");
    rig.poll_applied();
    let forward = rig
        .d
        .ok("graph.get_forward_links", json!({"id": "@g:Beta.md"}));
    assert_eq!(forward.as_array().unwrap().len(), 1, "{forward}");
    assert_eq!(forward[0]["to"], json!({"resolved": "@g:a.md"}));
    // Swap: a becomes b.md and b becomes a.md in one poll.
    rig.drive.rename_file("a", "tmp.md");
    rig.drive.rename_file("b", "a.md");
    rig.drive.rename_file("a", "Beta.md");
    rig.poll();
    assert_eq!(file_id(&rig, "Beta.md").as_deref(), Some("a"));
    assert_eq!(file_id(&rig, "a.md").as_deref(), Some("b"));
    rig.d.close();
}

#[test]
fn a_file_with_edits_to_send_is_kept_when_drive_removes_it() {
    let rig = started();
    exec_sql(
        &rig.d.cache_path,
        "UPDATE remote_items SET state = 'pending_upload' WHERE source = 'g' AND path = 'b.md'",
    );
    rig.drive.remove_file("b");
    rig.drive.remove_file("a");
    rig.poll();
    let rows = item_rows(&rig.d.cache_path, "g");
    let b = rows
        .iter()
        .find(|r| r.path == "b.md")
        .expect("the item with edits is kept");
    assert_eq!(b.state, "needs_attention");
    assert!(
        rows.iter().all(|r| r.path != "a.md"),
        "a clean one is dropped"
    );
    assert!(
        rig.d
            .entries("g")
            .iter()
            .all(|e| e["rel_path"] != json!("b.md")),
        "out of the index"
    );
    rig.d.close();
}
