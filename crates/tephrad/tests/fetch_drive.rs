//! PR-7 (G10, E7, E10): `unit.fetch` of a binary or text file of a Google
//! Drive source, against item rows seeded into the remote tables (PR-10 adds
//! the real source) and the fake Drive server of PR-3. The file is read by
//! its Drive file id, checked against the listed size, and written to
//! `<data_dir>/remote/<source>/<mapped path>` (no `clone/`).
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::path::PathBuf;

use remote_support::{files_under, item_rows, seed_drive_source, FetchRig, GOOGLE_TOKEN};
use serde_json::{json, Value};
use tephra_remote::testing::FakeDrive;

/// A daemon in front of a fake GitHub (unused) and a fake Drive that holds
/// `files` (`id`, bytes); the Drive source `gdrive` has the rows `items`
/// (`path`, kind, file id, listed size).
struct DriveRig {
    rig: FetchRig,
    drive: FakeDrive,
    // The fake Drive's server runs on this runtime.
    _rt: tokio::runtime::Runtime,
}

impl DriveRig {
    fn start(files: &[(&str, &[u8])], items: &[(&str, &str, &str, i64)]) -> DriveRig {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let drive = rt.block_on(FakeDrive::start());
        for (id, bytes) in files {
            drive.add_file(id, id, "application/octet-stream", None, bytes);
        }
        let rig = FetchRig::start_with_drive(Some(drive.base_url()), |_| {});
        seed_drive_source(&rig.d.cache_path, "gdrive", items);
        DriveRig {
            rig,
            drive,
            _rt: rt,
        }
    }

    fn fetch(&self, path: &str) -> Value {
        self.rig
            .d
            .rpc("unit.fetch", json!({ "unit": format!("@gdrive:{path}") }))
    }

    fn root(&self) -> PathBuf {
        self.rig.d.data_dir.join("remote").join("gdrive")
    }

    fn downloads(&self) -> usize {
        self.drive.request_log().matching("alt=media").len()
    }

    fn row(&self, path: &str) -> tephra_cache::RemoteItemRow {
        item_rows(&self.rig.d.cache_path, "gdrive")
            .into_iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("{path} has a row"))
    }

    fn reason_of(reply: &Value) -> String {
        assert!(reply.get("error").is_some(), "expected a refusal: {reply}");
        reply["error"]["data"]["reason"]
            .as_str()
            .unwrap_or_else(|| panic!("no reason: {reply}"))
            .to_string()
    }

    fn assert_left_clean(&self, path: &str) {
        let row = self.row(path);
        assert_eq!(row.state, "remote", "{path}");
        assert!(
            files_under(&self.root()).is_empty(),
            "no final or temp file: {:?}",
            files_under(&self.root())
        );
    }
}

#[test]
fn a_drive_file_is_fetched_by_its_id_to_the_source_directory_without_clone() {
    let body = b"# Plan\n\nthe drive bytes\n";
    let t = DriveRig::start(
        &[("plan-1", body)],
        &[("Notes/Plan.md", "document", "plan-1", body.len() as i64)],
    );
    assert!(files_under(&t.root()).is_empty());
    let reply = t.fetch("Notes/Plan.md");
    assert!(reply.get("error").is_none(), "{reply}");
    let result = &reply["result"];
    let expected = t.root().join("Notes").join("Plan.md");
    assert_eq!(result["path"], json!(expected.display().to_string()));
    assert_eq!(result["state"], json!("cached"));
    assert_eq!(result["size"], json!(body.len()));
    assert_eq!(std::fs::read(&expected).unwrap(), body);
    assert!(
        !t.root().join("clone").exists(),
        "a Drive file has no clone/"
    );
    assert_eq!(t.row("Notes/Plan.md").state, "cached");
    assert_eq!(t.downloads(), 1);
    assert_eq!(
        t.drive.request_log().matching("/files/plan-1").len(),
        1,
        "one request, by the file id"
    );
    // The token went on the request, and nowhere else.
    let sent = t.drive.request_log().matching("alt=media")[0]
        .header("authorization")
        .map(str::to_string);
    assert_eq!(sent, Some(format!("Bearer {GOOGLE_TOKEN}")));
    assert!(!reply.to_string().contains(GOOGLE_TOKEN));

    // A second fetch is a no-op: no request, same file.
    let again = t.fetch("Notes/Plan.md");
    assert_eq!(again["result"]["path"], result["path"]);
    assert_eq!(t.downloads(), 1);
}

#[test]
fn drive_names_with_slashes_dots_and_300_bytes_land_inside_the_cache_root() {
    let long = format!("{}.md", "n".repeat(300));
    let names: Vec<String> = vec![
        "a/b/c.md".to_string(),
        "../escape.md".to_string(),
        "Notes/../../escape2.md".to_string(),
        "up/..".to_string(),
        "dot/./.".to_string(),
        "/abs/olute.md".to_string(),
        "back\\slash.md".to_string(),
        "a%2Fb.md".to_string(),
        ".git".to_string(),
        "Dir/.git/config".to_string(),
        long.clone(),
        "Readme.md".to_string(),
        "README.MD".to_string(),
    ];
    let bodies: Vec<Vec<u8>> = (0..names.len())
        .map(|i| format!("drive body {i}\n").into_bytes())
        .collect();
    let ids: Vec<String> = (0..names.len()).map(|i| format!("file-{i}")).collect();
    let files: Vec<(&str, &[u8])> = ids
        .iter()
        .zip(&bodies)
        .map(|(id, b)| (id.as_str(), b.as_slice()))
        .collect();
    let items: Vec<(&str, &str, &str, i64)> = names
        .iter()
        .zip(&ids)
        .zip(&bodies)
        .map(|((n, id), b)| (n.as_str(), "binary", id.as_str(), b.len() as i64))
        .collect();
    let t = DriveRig::start(&files, &items);
    let root = t.root();
    let mut written = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let reply = t.fetch(name);
        assert!(reply.get("error").is_none(), "{name:?}: {reply}");
        let path = PathBuf::from(reply["result"]["path"].as_str().unwrap());
        assert!(path.starts_with(&root), "{name:?} -> {path:?}");
        assert!(
            path.canonicalize()
                .unwrap()
                .starts_with(root.canonicalize().unwrap()),
            "{name:?} -> {path:?}"
        );
        assert!(
            !path
                .components()
                .any(|c| c.as_os_str() == ".." || c.as_os_str() == ".git"),
            "{name:?} -> {path:?}"
        );
        for part in path.strip_prefix(&root).unwrap().components() {
            assert!(
                part.as_os_str().len() <= 255,
                "{name:?}: a name over 255 bytes"
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), bodies[i], "{name:?}");
        written.push(path);
    }
    // Injective: no two names share a file, and nothing else was written.
    let mut unique = written.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{written:?}");
    assert_eq!(files_under(&root).len(), names.len());
    assert!(!root.join("clone").exists());
    // Nothing was written outside the source's directory.
    let remote = root.parent().unwrap();
    assert_eq!(files_under(remote).len(), names.len());
    assert_eq!(t.downloads(), names.len());
}

#[test]
fn a_drive_body_that_is_not_the_listed_size_is_refused_and_nothing_is_kept() {
    let body = b"twelve bytes";
    let t = DriveRig::start(
        &[("f-1", body)],
        &[
            ("longer.bin", "binary", "f-1", 10_000),
            ("shorter.bin", "binary", "f-1", 3),
        ],
    );
    for path in ["longer.bin", "shorter.bin"] {
        let reply = t.fetch(path);
        assert_eq!(DriveRig::reason_of(&reply), "corrupt_body", "{reply}");
        t.assert_left_clean(path);
        let stored = t.row(path).reason.unwrap_or_default();
        assert!(stored.starts_with("fetch failed: "), "{stored}");
    }
}

#[test]
fn a_drive_connection_that_drops_midway_leaves_nothing() {
    let t = DriveRig::start(
        &[("f-1", b"some bytes that get cut")],
        &[("cut.bin", "binary", "f-1", 23)],
    );
    t.drive.truncate_next_bodies(1);
    let reply = t.fetch("cut.bin");
    assert_eq!(DriveRig::reason_of(&reply), "connection_dropped", "{reply}");
    t.assert_left_clean("cut.bin");
    // The next fetch is a fresh download.
    let ok = t.fetch("cut.bin");
    assert!(ok.get("error").is_none(), "{ok}");
    assert_eq!(t.row("cut.bin").state, "cached");
}

#[test]
fn drive_errors_give_a_reason_that_names_drive_and_never_the_token() {
    for (name, status, reason) in [
        ("403", 403u16, "permission_denied"),
        ("404", 404, "remote_missing"),
        ("500", 500, "server_error"),
    ] {
        let t = DriveRig::start(&[("f-1", b"abc")], &[("a.bin", "binary", "f-1", 3)]);
        t.drive.echo_authorization_in_errors(true);
        t.drive.fail_next(20, status);
        let reply = t.fetch("a.bin");
        assert_eq!(DriveRig::reason_of(&reply), reason, "{name}: {reply}");
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(message.contains("Google Drive"), "{name}: {message}");
        assert!(
            !reply.to_string().contains(GOOGLE_TOKEN),
            "{name}: the token is in the answer"
        );
        t.assert_left_clean("a.bin");
        let stored = t.row("a.bin").reason.unwrap_or_default();
        assert!(!stored.contains(GOOGLE_TOKEN));
    }
}

#[test]
fn a_rejected_google_token_says_to_sign_in_to_google() {
    let t = DriveRig::start(&[("f-1", b"abc")], &[("a.bin", "binary", "f-1", 3)]);
    t.drive.revoke_token();
    let reply = t.fetch("a.bin");
    assert_eq!(DriveRig::reason_of(&reply), "auth_required", "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(message.contains("tephractl auth google"), "{message}");
    t.assert_left_clean("a.bin");
}

#[test]
fn a_drive_row_without_a_file_id_is_not_downloaded() {
    let t = DriveRig::start(&[("f-1", b"abc")], &[("a.bin", "binary", "", 3)]);
    let reply = t.fetch("a.bin");
    assert_eq!(DriveRig::reason_of(&reply), "no_listing", "{reply}");
    assert_eq!(t.downloads(), 0);
    t.assert_left_clean("a.bin");
}

/// A file Tephra did not write is at the cache path: a Drive listing has no
/// checksum, so the download is compared with it. The same bytes are adopted
/// (and never deleted); other bytes are left as they are and the item needs
/// attention.
#[test]
fn a_file_already_at_the_drive_path_is_adopted_if_the_same_and_kept_if_not() {
    let body = b"the drive bytes\n";
    let t = DriveRig::start(
        &[("f-1", body), ("f-2", body)],
        &[
            ("same.md", "document", "f-1", body.len() as i64),
            ("other.md", "document", "f-2", body.len() as i64),
        ],
    );
    std::fs::create_dir_all(t.root()).unwrap();
    std::fs::write(t.root().join("same.md"), body).unwrap();
    std::fs::write(t.root().join("other.md"), b"Dan's own words\n").unwrap();

    let same = t.fetch("same.md");
    assert!(same.get("error").is_none(), "{same}");
    assert_eq!(same["result"]["state"], json!("cached"));
    assert_eq!(std::fs::read(t.root().join("same.md")).unwrap(), body);
    assert_eq!(t.row("same.md").state, "cached");

    let other = t.fetch("other.md");
    assert!(other.get("error").is_none(), "{other}");
    assert_eq!(other["result"]["state"], json!("needs_attention"));
    assert_eq!(
        std::fs::read(t.root().join("other.md")).unwrap(),
        b"Dan's own words\n",
        "never overwritten"
    );
    assert_eq!(t.row("other.md").state, "needs_attention");
    // No temp file is left next to either.
    assert_eq!(files_under(&t.root()).len(), 2);
}

#[test]
fn a_symlink_at_a_drive_path_is_never_followed_or_written_through() {
    let t = DriveRig::start(&[("f-1", b"abc")], &[("link.md", "document", "f-1", 3)]);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("victim.txt");
    std::fs::write(&target, b"untouched").unwrap();
    std::fs::create_dir_all(t.root()).unwrap();
    std::os::unix::fs::symlink(&target, t.root().join("link.md")).unwrap();
    let reply = t.fetch("link.md");
    assert!(reply.get("error").is_some(), "{reply}");
    assert_eq!(t.downloads(), 0, "refused before any bytes were read");
    assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    assert_eq!(t.row("link.md").state, "remote");
}

/// A copy that was already at the path and was adopted is not Tephra's to
/// delete: when the listing changes while the fetch runs (so the fetch can
/// not mark the item cached) the file stays where it is, and the fetch
/// says the file changed.
#[test]
fn an_adopted_copy_is_not_deleted_when_the_listing_changes_during_the_fetch() {
    let body = b"the drive bytes\n";
    let t = DriveRig::start(
        &[("f-1", body)],
        &[("same.md", "document", "f-1", body.len() as i64)],
    );
    std::fs::create_dir_all(t.root()).unwrap();
    std::fs::write(t.root().join("same.md"), body).unwrap();
    t.drive.set_latency(std::time::Duration::from_millis(1200));

    let reply = std::thread::scope(|scope| {
        let fetch = scope.spawn(|| t.fetch("same.md"));
        t.rig.d.wait_for("the item is downloading", || {
            (t.row("same.md").state == "downloading").then_some(())
        });
        // A sync lists the file anew while its bytes come down.
        let cache = tephra_cache::Cache::open(&t.rig.d.cache_path).unwrap();
        cache
            .connection()
            .execute(
                "UPDATE remote_items SET size = size + 1 WHERE source = 'gdrive' AND path = 'same.md'",
                [],
            )
            .unwrap();
        fetch.join().unwrap()
    });
    assert_eq!(DriveRig::reason_of(&reply), "changed_meanwhile", "{reply}");
    assert_eq!(
        std::fs::read(t.root().join("same.md")).unwrap(),
        body,
        "a file this fetch did not write is not deleted"
    );
    assert_eq!(t.row("same.md").state, "remote");
}
