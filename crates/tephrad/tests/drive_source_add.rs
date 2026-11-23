//! PR-10 (G4, G8, F4, F7): `source.add_drive`. A folder by path, id or URL;
//! the credential and the folder are checked before anything is stored; a
//! Shared Drive folder and the whole Drive are refused with a reason; the
//! listing is stored with no content read. Real registry and RPC server
//! against the PR-3 fake Drive.
#![allow(dead_code)]

mod remote_support;

use remote_support::{DriveDaemon, MemStore};
use serde_json::{json, Value};

fn notes(drive: &tephra_remote::testing::FakeDrive) {
    drive.add_folder("f-notes", "Notes", None);
    drive.add_file(
        "m1",
        "Plan.md",
        "text/markdown",
        Some("f-notes"),
        b"# Plan\n",
    );
    drive.add_file("m2", "todo.txt", "text/plain", Some("f-notes"), b"todo\n");
    drive.add_folder("f-sub", "Sub", Some("f-notes"));
    drive.add_file("m3", "deep.md", "text/markdown", Some("f-sub"), b"# Deep\n");
    // Outside the picked folder.
    drive.add_folder("f-other", "Other", None);
    drive.add_file(
        "o1",
        "elsewhere.md",
        "text/markdown",
        Some("f-other"),
        b"# E\n",
    );
}

fn reason(reply: &Value) -> String {
    reply["error"]["data"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal with a reason: {reply}"))
        .to_string()
}

#[test]
fn a_folder_by_name_path_id_or_url_is_the_root_and_nothing_else_is_listed() {
    for (n, folder) in [
        "Notes",
        "/Notes",
        "f-notes",
        "https://drive.google.com/drive/folders/f-notes?usp=sharing",
    ]
    .into_iter()
    .enumerate()
    {
        let rig = DriveDaemon::start(notes);
        let name = format!("gdrive{n}");
        let reply = rig.add(&name, folder);
        assert_eq!(reply["files"], json!(3), "{folder}: {reply}");
        assert_eq!(reply["folder"], json!("f-notes"));
        assert_eq!(
            rig.paths(&name),
            vec!["Plan.md", "Sub/deep.md", "todo.txt"],
            "{folder}"
        );
    }
}

#[test]
fn a_nested_path_names_the_folder() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("w", "Work", None);
        d.add_folder("n", "Notes", Some("w"));
        d.add_file("a", "a.md", "text/markdown", Some("n"), b"# a\n");
        d.add_folder("n2", "Notes", None);
        d.add_file("b", "b.md", "text/markdown", Some("n2"), b"# b\n");
        d.add_folder("n3", "Notes", None);
    });
    rig.add("g", "Work/Notes");
    assert_eq!(rig.paths("g"), vec!["a.md"]);
    // Two folders called Notes in My Drive: the name alone is ambiguous.
    let reply = rig.add_raw("h", "Notes");
    assert_eq!(reason(&reply), "ambiguous_folder", "{reply}");
}

#[test]
fn listing_reads_no_content() {
    let rig = DriveDaemon::start(notes);
    // Hold every content read, so the add itself is what is measured.
    rig.drive.hold_content_after(0);
    let reply = rig.add("gdrive", "Notes");
    assert_eq!(reply["status"], json!("indexing"));
    assert_eq!(reply["index_done"], json!(0));
    let row = rig.d.source_row("gdrive").expect("listed");
    assert_eq!(row["status"], json!("indexing"));
    // The add returned with the listing; the one read the background index
    // has started is held, and no content has reached the index.
    assert!(rig.media_reads() + rig.exports() <= 1);
    assert_eq!(rig.d.source_row("gdrive").unwrap()["index_done"], json!(0));
    let entries = rig.d.entries("gdrive");
    let files: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"] != json!("folder"))
        .collect();
    assert_eq!(files.len(), 3, "{entries:?}");
    for e in &files {
        assert_eq!(e["state"], json!("remote"), "{e}");
    }
    rig.drive.release_content();
    rig.d.wait_active("gdrive");
}

#[test]
fn a_missing_credential_is_auth_required_and_registers_nothing() {
    let rig = DriveDaemon::start_with(MemStore::signed_out(), notes);
    let reply = rig.add_raw("gdrive", "Notes");
    assert_eq!(reason(&reply), "auth_required", "{reply}");
    assert!(rig.d.source_row("gdrive").is_none());
    assert_eq!(rig.drive.request_log().count(), 0, "no request was made");
}

#[test]
fn a_rejected_token_is_auth_required_and_registers_nothing() {
    let rig = DriveDaemon::start(notes);
    rig.drive.revoke_token();
    let reply = rig.add_raw("gdrive", "Notes");
    assert_eq!(reason(&reply), "auth_required", "{reply}");
    assert!(rig.d.source_row("gdrive").is_none());
}

#[test]
fn an_unknown_folder_is_refused_clearly_and_registers_nothing() {
    let rig = DriveDaemon::start(notes);
    for folder in ["Nope", "Notes/Nope", "no-such-id"] {
        let reply = rig.add_raw("gdrive", folder);
        assert_eq!(reason(&reply), "not_found", "{folder}: {reply}");
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(folder.rsplit('/').next().unwrap()),
            "{message}"
        );
    }
    assert!(rig.d.source_row("gdrive").is_none());
    let listed = rig
        .d
        .rpc("unit.list", json!({"source": "gdrive", "folder": ""}));
    assert!(
        listed.get("error").is_some(),
        "no source, so nothing to list"
    );
}

#[test]
fn a_file_is_not_a_folder() {
    let rig = DriveDaemon::start(notes);
    let reply = rig.add_raw("gdrive", "m1");
    assert_eq!(reason(&reply), "not_a_folder", "{reply}");
}

#[test]
fn a_shared_drive_folder_is_refused_with_a_reason_and_no_request_scopes_to_one() {
    let rig = DriveDaemon::start(|d| {
        notes(d);
        d.set_drive_id("f-notes", "shared-drive-1");
    });
    let reply = rig.add_raw("gdrive", "f-notes");
    assert_eq!(reason(&reply), "shared_drive", "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Shared Drive"),
        "{reply}"
    );
    assert!(rig.d.source_row("gdrive").is_none());
    for request in rig.drive.request_log().all() {
        let target = request.target();
        for banned in [
            "supportsAllDrives",
            "includeItemsFromAllDrives",
            "corpora",
            "driveId=",
        ] {
            assert!(!target.contains(banned), "{banned} in {target}");
        }
    }
}

#[test]
fn the_whole_drive_cannot_be_added() {
    let rig = DriveDaemon::start(notes);
    for folder in ["", "/", "root"] {
        let reply = rig.add_raw("gdrive", folder);
        assert!(reply.get("error").is_some(), "{folder:?}: {reply}");
    }
    assert!(rig.d.source_row("gdrive").is_none());
}

#[test]
fn the_name_rules_are_the_local_ones() {
    let rig = DriveDaemon::start(notes);
    for bad in ["", "a/b", "with space", "a@b", ".."] {
        let reply = rig.add_raw(bad, "Notes");
        assert!(reply.get("error").is_some(), "{bad:?}: {reply}");
    }
    rig.add("gdrive", "Notes");
    let again = rig.add_raw("GDrive", "Notes");
    assert_eq!(reason(&again), "name_taken", "{again}");
    assert_eq!(
        rig.d
            .ok("source.list", json!({}))
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == json!("drive"))
            .count(),
        1
    );
}

#[test]
fn the_start_token_is_taken_before_the_first_listing() {
    let rig = DriveDaemon::start(notes);
    // By id, so nothing is listed to find the folder: the first listing
    // there is is the tree's.
    rig.add("gdrive", "f-notes");
    let targets: Vec<String> = rig
        .drive
        .request_log()
        .all()
        .iter()
        .map(|r| r.target())
        .collect();
    let token_at = targets
        .iter()
        .position(|t| t.contains("changes/startPageToken"))
        .expect("a start token was asked for");
    let first_list = targets
        .iter()
        .position(|t| t.contains("parents"))
        .expect("a listing was made");
    assert!(token_at < first_list, "{targets:?}");
}
