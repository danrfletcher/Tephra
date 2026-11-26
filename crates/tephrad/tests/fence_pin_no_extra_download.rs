//! PR-12 fence (F2, F3, F5, F6, F11, F12), read from the fake servers'
//! request logs. A pin asks for the bytes of the files it covers and
//! nothing else: not a native Google file, not Git LFS content or a
//! submodule, not a file in a sibling folder. Pin and unpin write nothing
//! on the remote and run no git command.
#![allow(dead_code)]

mod remote_support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use remote_support::{DriveDaemon, FetchRig};
use serde_json::json;

fn drive(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("fld", "Notes", None);
    d.add_folder("trip", "Trip Plans", Some("fld"));
    d.add_folder("deep", "Deep", Some("trip"));
    d.add_folder("other", "Other", Some("fld"));
    d.add_folder("oldtrip", "Trip Plans-old", Some("fld"));
    d.add_file("a", "a.md", "text/markdown", Some("trip"), b"# a\n");
    d.add_file("b", "b.pdf", "application/pdf", Some("deep"), b"%PDF-1.4 b");
    d.add_doc("gdoc1", "Plan", Some("trip"), "# plan\n");
    d.add_native(
        "gsheet1",
        "Budget",
        "application/vnd.google-apps.spreadsheet",
        Some("trip"),
    );
    d.add_native(
        "gslide1",
        "Deck",
        "application/vnd.google-apps.presentation",
        Some("trip"),
    );
    d.add_native(
        "gform1",
        "Survey",
        "application/vnd.google-apps.form",
        Some("trip"),
    );
    d.add_file("o1", "o1.md", "text/markdown", Some("other"), b"# o1\n");
    d.add_file("z1", "z1.md", "text/markdown", Some("oldtrip"), b"# z1\n");
    d.add_file("c", "c.md", "text/markdown", Some("fld"), b"# c\n");
}

/// The Drive file ids named by the requests since `from`, as the set of
/// `/files/<id>` segments.
fn ids_since(dd: &DriveDaemon, from: usize) -> BTreeSet<String> {
    dd.drive
        .request_log()
        .all()
        .into_iter()
        .skip(from)
        .filter_map(|r| {
            let rest = r.path.split("/files/").nth(1)?;
            let id = rest.split(['/', '?']).next()?.to_string();
            // A listing or the changes feed has no id of its own.
            (!id.is_empty() && id != "generateIds").then_some(id)
        })
        .collect()
}

fn settled(dd: &DriveDaemon, paths: &[&str]) {
    for path in paths {
        dd.wait_state(path, "pinned");
    }
}

#[test]
fn f12_a_folder_pin_requests_only_the_files_it_covers_and_never_a_native_file() {
    let dd = DriveDaemon::start(drive);
    dd.add_active("gdrive", "fld");
    let all_before = dd.drive.request_log().count();
    let media_before = dd.media_reads();
    let exports_before = dd.exports();

    let reply = dd.d.rpc(
        "unit.pin",
        json!({ "unit": "@gdrive:Trip Plans/", "confirm": true }),
    );
    // Four natives (Doc, Sheet, Slides, Form) are counted as skipped, and
    // the Doc is read by the index as a markdown file, so it is not native
    // here: the pin still never exports it.
    assert_eq!(reply["result"]["status"], json!("pinned"), "{reply}");
    let skipped = reply["result"]["skipped"].as_object().unwrap();
    for kind in ["gsheet", "gslides", "gform"] {
        assert_eq!(skipped[kind], json!(1), "{reply}");
    }
    settled(&dd, &["Trip Plans/a.md", "Trip Plans/Deep/b.pdf"]);
    // Let any stray request show.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let ids = ids_since(&dd, all_before);
    assert_eq!(
        ids,
        BTreeSet::from(["a".to_string(), "b".to_string()]),
        "only the pinned, fetchable files were asked for: {ids:?}"
    );
    assert_eq!(dd.media_reads() - media_before, 2);
    assert_eq!(dd.exports(), exports_before, "no export for a Doc");

    // The natives are still listed by name; nothing of theirs was fetched.
    let paths = dd.paths("gdrive");
    for name in ["Trip Plans/Budget", "Trip Plans/Deck", "Trip Plans/Survey"] {
        assert!(paths.contains(&name.to_string()), "{name} is listed");
        assert_eq!(dd.row(name).state, "remote");
    }
    assert!(!dd.root().join("Trip Plans/Budget").exists());
    // The sibling folders, one of which shares the pinned name's prefix,
    // were never touched.
    assert_eq!(dd.row("Other/o1.md").state, "remote");
    assert_eq!(dd.row("Trip Plans-old/z1.md").state, "remote");
    assert_eq!(dd.row("c.md").state, "remote");
    dd.d.close();
}

#[test]
fn f2_f12_pin_and_unpin_write_nothing_remote_and_unpin_asks_for_nothing() {
    let dd = DriveDaemon::start(drive);
    dd.add_active("gdrive", "fld");
    let all_before = dd.drive.request_log().count();
    assert!(dd.writes().is_empty());

    dd.d.ok(
        "unit.pin",
        json!({ "unit": "@gdrive:Trip Plans/", "confirm": true }),
    );
    settled(&dd, &["Trip Plans/a.md", "Trip Plans/Deep/b.pdf"]);
    dd.d.ok("unit.pin", json!({ "unit": "@gdrive:c.md" }));
    dd.wait_state("c.md", "pinned");
    let after_pin = dd.drive.request_log().count();
    dd.d.ok("unit.unpin", json!({ "unit": "@gdrive:Trip Plans/" }));
    dd.d.ok("unit.unpin", json!({ "unit": "@gdrive:c.md" }));
    std::thread::sleep(std::time::Duration::from_millis(300));

    // F2: no write, delete, move or rename on the Drive.
    assert_eq!(dd.writes(), Vec::<String>::new());
    // Unpin made no request at all.
    assert_eq!(dd.drive.request_log().count(), after_pin);
    // Every request of the pin was a read of a file it covers.
    let methods: BTreeSet<String> = dd
        .drive
        .request_log()
        .all()
        .into_iter()
        .skip(all_before)
        .map(|r| r.method)
        .collect();
    assert_eq!(methods, BTreeSet::from(["GET".to_string()]));
    dd.d.close();
}

#[test]
fn f3_f5_f11_a_github_pin_never_reads_lfs_content_a_submodule_or_a_symlink_and_never_writes() {
    let rig = FetchRig::start(|fake| {
        fake.gh.put_file(
            remote_support::REPO,
            "main",
            "pinned/real.txt",
            b"real bytes\n",
        );
        fake.gh.put_file(
            remote_support::REPO,
            "main",
            "pinned/huge.txt",
            b"version https://git-lfs.github.com/spec/v1\noid sha256:aa\nsize 99\n",
        );
        fake.gh.put_submodule(
            remote_support::REPO,
            "main",
            "pinned/vendor",
            "0123456789abcdef0123456789abcdef01234567",
        );
        fake.gh
            .put_symlink(remote_support::REPO, "main", "pinned/link", "real.txt");
        fake.gh.put_file(
            remote_support::REPO,
            "main",
            "sibling/other.txt",
            b"never asked for\n",
        );
    });
    let reads_before = rig.fake.reads().len();
    let all_before = rig.fake.requests();

    let reply = rig
        .d
        .rpc("unit.pin", json!({ "unit": "@g:pinned/", "confirm": true }));
    assert_eq!(reply["result"]["status"], json!("pinned"), "{reply}");
    assert_eq!(reply["result"]["files"], json!(1), "{reply}");
    let skipped = reply["result"]["skipped"].as_object().unwrap();
    assert_eq!(skipped["lfs_pointer"], json!(1), "{reply}");
    assert_eq!(skipped["submodule"], json!(1), "{reply}");
    assert_eq!(skipped["symlink"], json!(1), "{reply}");
    rig.d.wait_for("the real file to be pinned", || {
        (rig.row("pinned/real.txt").state == "pinned").then_some(())
    });
    std::thread::sleep(std::time::Duration::from_millis(300));

    let reads = rig.fake.reads();
    assert_eq!(reads.len() - reads_before, 1, "one read: {reads:?}");
    assert!(!reads.iter().any(|r| r.contains("sibling")), "{reads:?}");
    for name in ["pinned/huge.txt", "pinned/vendor", "pinned/link"] {
        assert_eq!(rig.row(name).state, "remote", "{name}");
        assert!(!rig.cached_path(name).exists(), "{name}");
    }
    assert_eq!(rig.row("sibling/other.txt").state, "remote");

    // F3: no non-GET request, so no push, and unpin asks for nothing.
    let after_pin = rig.fake.requests();
    assert!(after_pin > all_before);
    rig.d.ok("unit.unpin", json!({ "unit": "@g:pinned/" }));
    assert_eq!(rig.fake.requests(), after_pin);
    assert!(rig.fake.non_get_requests().is_empty());
    rig.d.close();
}

/// F3, the static half: the pin module names no git command and has no
/// path that writes to a remote.
#[test]
fn the_pin_module_runs_no_git_and_calls_no_write_api() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/remote/pin.rs");
    let source = std::fs::read_to_string(path).expect("pin.rs exists");
    let code = source.split("#[cfg(test)]").next().unwrap();
    for forbidden in [
        "Command::new",
        "std::process",
        "git_program",
        "upload",
        "push",
        "commit",
        "rebase",
        "merge",
        "remove_file",
        "rename(",
    ] {
        let hit = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .find(|l| l.contains(forbidden));
        assert_eq!(hit, None, "pin.rs mentions `{forbidden}`");
    }
}
