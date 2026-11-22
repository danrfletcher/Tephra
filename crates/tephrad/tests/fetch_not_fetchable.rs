//! PR-7 (G10, E27, F5, F11): items whose bytes are never fetched answer
//! `not_fetchable`, with where to open them when there is a URL, and nothing
//! is downloaded, written, or left `downloading`.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use remote_support::{files_under, seed_drive_source, FetchRig, REPO};
use serde_json::{json, Value};
use tephra_remote::testing::FakeDrive;

fn error_data(reply: &Value) -> &Value {
    assert!(reply.get("error").is_some(), "expected a refusal: {reply}");
    &reply["error"]["data"]
}

/// E27, with the Drive server on the side: a Doc or a Sheet answers
/// `not_fetchable` with its URL, and the fake Drive server sees no download
/// and no export request.
#[test]
fn a_doc_or_a_sheet_answers_not_fetchable_with_its_url_and_no_drive_request_is_made() {
    let rig = FetchRig::start(|_| {});
    let rt = tokio::runtime::Runtime::new().unwrap();
    let drive = rt.block_on(FakeDrive::start());
    drive.add_doc("doc-1", "Plan", None, "# Plan\n");
    drive.add_file(
        "sheet-1",
        "Budget",
        "application/vnd.google-apps.spreadsheet",
        None,
        b"",
    );
    seed_drive_source(
        &rig.d.cache_path,
        "gdrive",
        &[
            ("Plan", "gdoc", "doc-1", 0),
            ("Budget", "gsheet", "sheet-1", 0),
            ("Deck", "gslides", "slides-1", 0),
            ("Survey", "gform", "form-1", 0),
            ("Sketch", "gdrawing", "drawing-1", 0),
            ("Link to Plan", "gshortcut", "shortcut-1", 0),
        ],
    );

    for (path, url) in [
        (
            "Plan",
            Some("https://docs.google.com/document/d/doc-1/edit"),
        ),
        (
            "Budget",
            Some("https://docs.google.com/spreadsheets/d/sheet-1/edit"),
        ),
        (
            "Deck",
            Some("https://docs.google.com/presentation/d/slides-1/edit"),
        ),
        (
            "Survey",
            Some("https://docs.google.com/forms/d/form-1/edit"),
        ),
        (
            "Sketch",
            Some("https://docs.google.com/drawings/d/drawing-1/edit"),
        ),
        ("Link to Plan", None),
    ] {
        let reply = rig
            .d
            .rpc("unit.fetch", json!({ "unit": format!("@gdrive:{path}") }));
        let data = error_data(&reply);
        assert_eq!(data["reason"], json!("not_fetchable"), "{path}: {reply}");
        match url {
            Some(url) => {
                assert_eq!(data["url"], json!(url), "{path}");
                assert!(
                    reply["error"]["message"].as_str().unwrap().contains(url),
                    "the message carries the URL too: {reply}"
                );
            }
            None => assert!(data.get("url").is_none(), "{path}: {reply}"),
        }
    }
    assert_eq!(
        drive.request_log().matching("alt=media").len()
            + drive.request_log().matching("/export").len(),
        0,
        "no download or export request"
    );
    assert_eq!(drive.request_log().count(), 0);
    assert!(
        files_under(&rig.d.data_dir.join("remote").join("gdrive")).is_empty(),
        "nothing was written"
    );
    for item in remote_support::item_rows(&rig.d.cache_path, "gdrive") {
        assert_eq!(item.state, "remote", "{}", item.path);
    }
}

/// F5, F11: a symlink, a submodule and a Git LFS pointer of a GitHub source
/// are the same: listed by name, never fetched.
#[test]
fn a_symlink_a_submodule_and_an_lfs_pointer_answer_not_fetchable_and_ask_nothing_of_the_server() {
    let rig = FetchRig::start(|fake| {
        fake.gh.put_symlink(REPO, "main", "link", "README.md");
        fake.gh.put_submodule(
            REPO,
            "main",
            "ext/sub",
            "0123456789abcdef0123456789abcdef01234567",
        );
        fake.gh.put_file(
            REPO,
            "main",
            "big.bin.txt",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aa\nsize 99\n",
        );
    });
    let requests = rig.fake.requests();
    for path in ["link", "ext/sub", "big.bin.txt"] {
        let reply = rig.fetch(path);
        assert_eq!(
            error_data(&reply)["reason"],
            json!("not_fetchable"),
            "{path}: {reply}"
        );
        assert!(rig.row(path).state == "remote", "{path}");
    }
    assert_eq!(rig.fake.requests(), requests, "no request at all");
    assert!(rig.cache_files().is_empty());
}

/// A folder is not a file, and a file that is no longer there is unknown.
#[test]
fn a_folder_and_a_missing_file_are_unknown_units() {
    let rig = FetchRig::start(|_| {});
    for unit in ["@g:docs", "@g:docs/Missing.md", "@g:"] {
        let reply = rig.d.rpc("unit.fetch", json!({ "unit": unit }));
        assert!(reply.get("error").is_some(), "{unit}: {reply}");
    }
    assert!(rig.cache_files().is_empty());
}
