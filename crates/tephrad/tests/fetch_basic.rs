//! PR-7 (G10, GP5): `unit.fetch` puts one remote file in Tephra's content
//! cache, once, at the same relative path, and says what it did.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::os::unix::fs::MetadataExt;
use std::sync::atomic::Ordering;
use std::time::Duration;

use remote_support::{seed_cached_copy, FetchRig, QUIET, REPO};
use serde_json::{json, Value};
use tephra_remote::testing::FakeGithub;

const README: &[u8] = b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n";

fn entry(rig: &FetchRig, path: &str) -> Value {
    rig.d
        .entries("g")
        .into_iter()
        .find(|e| e["rel_path"] == json!(path))
        .unwrap_or_else(|| panic!("{path} listed"))
}

fn result(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

/// GP5: nothing is on disk before; `downloading` during a slow fetch;
/// `cached` after, with the path as the answer and the remote's bytes there.
#[test]
fn a_fetch_goes_remote_then_downloading_then_cached_and_writes_the_remote_bytes() {
    let rig = FetchRig::start(|_| {});
    assert_eq!(rig.row("README.md").state, "remote");
    assert!(rig.cache_files().is_empty(), "nothing is on disk before");
    assert_eq!(entry(&rig, "README.md")["state"], json!("remote"));
    assert_eq!(entry(&rig, "README.md")["cache_bytes"], json!(0));

    rig.fake.gh.set_latency(Duration::from_millis(900));
    let reads_before = rig.reads();
    let reply = std::thread::scope(|scope| {
        let call = scope.spawn(|| rig.fetch("README.md"));
        // While the slow call runs, the row says so, and `unit.list` shows it
        // without starting a download of its own.
        rig.d.wait_for("README.md is downloading", || {
            (rig.row("README.md").state == "downloading").then_some(())
        });
        assert_eq!(entry(&rig, "README.md")["state"], json!("downloading"));
        assert!(rig.cache_files().is_empty(), "no final file while it runs");
        call.join().unwrap()
    });
    let result = result(&reply);

    let expected = rig.cached_path("README.md");
    assert_eq!(result["path"], json!(expected.display().to_string()));
    assert_eq!(result["state"], json!("cached"));
    assert_eq!(result["size"], json!(README.len()));
    assert_eq!(result["unit"], json!("@g:README.md"));
    assert_eq!(std::fs::read(&expected).unwrap(), README);
    assert_eq!(
        FakeGithub::blob_sha(&std::fs::read(&expected).unwrap()),
        FakeGithub::blob_sha(README)
    );
    assert_eq!(rig.row("README.md").state, "cached");
    assert_eq!(
        rig.cache_files(),
        vec![std::path::PathBuf::from("clone/README.md")],
        "the file and nothing else: no temporary file is left"
    );
    assert_eq!(rig.reads() - reads_before, 1, "one content request");
}

/// `unit.list` and `source.list` show the live state and the bytes taken.
#[test]
fn the_lists_show_the_state_and_cache_bytes_rise_by_the_file_size() {
    let rig = FetchRig::start(|_| {});
    let source_bytes = |rig: &FetchRig| rig.d.source_row("g").unwrap()["cache_bytes"].clone();
    assert_eq!(source_bytes(&rig), json!(0));

    let reads = rig.reads();
    // Listing never downloads.
    for _ in 0..3 {
        let _ = rig.d.entries("g");
        let _ = rig.d.source_row("g");
    }
    assert_eq!(
        rig.reads(),
        reads,
        "unit.list and source.list read no content"
    );
    assert!(rig.cache_files().is_empty());

    result(&rig.fetch("README.md"));
    let e = entry(&rig, "README.md");
    assert_eq!(e["state"], json!("cached"));
    assert_eq!(e["cache_bytes"], json!(README.len()));
    assert_eq!(source_bytes(&rig), json!(README.len()));
    // Another file is still remote and takes nothing.
    assert_eq!(entry(&rig, "docs/Guide.md")["state"], json!("remote"));
    assert_eq!(entry(&rig, "docs/Guide.md")["cache_bytes"], json!(0));

    result(&rig.fetch("docs/Guide.md"));
    assert_eq!(source_bytes(&rig), json!(README.len() + 29));
    assert_eq!(
        std::fs::read(rig.cached_path("docs/Guide.md")).unwrap(),
        b"# Guide\n\nBack to [[README]].\n"
    );
}

/// GP5: a second fetch changes nothing on disk, asks the server nothing and
/// still touches `last_used`.
#[test]
fn a_second_fetch_is_a_no_op_that_only_moves_last_used() {
    let rig = FetchRig::start(|_| {});
    rig.clock.advance(Duration::from_secs(10));
    result(&rig.fetch("README.md"));
    let first_use = rig
        .row("README.md")
        .last_used
        .expect("a fetch notes its use");
    let path = rig.cached_path("README.md");
    let before = std::fs::metadata(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    // The clock moves 90 s, past the 60 s poll interval, so the poller may
    // ask the server for the branch head in this window. That is not the
    // fetch: what must not happen is a request for the file's content.
    let reads = rig.fake.reads();
    let requests = rig.fake.requests();

    rig.clock.advance(Duration::from_secs(90));
    let reply = rig.fetch("README.md");
    let again = result(&reply);
    assert_eq!(again["path"], json!(path.display().to_string()));
    assert_eq!(again["state"], json!("cached"));
    assert_eq!(again["size"], json!(README.len()));

    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.ino(), after.ino(), "same inode");
    assert_eq!(before.mtime(), after.mtime());
    assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    assert_eq!(bytes, std::fs::read(&path).unwrap());
    assert_eq!(rig.fake.reads(), reads, "no content request to the server");
    let all = rig.fake.gh.request_log().all();
    for r in &all[requests..] {
        assert!(
            r.target().contains("/commits/"),
            "only the poller's head check may reach the server, not {}",
            r.target()
        );
    }
    assert_eq!(rig.row("README.md").state, "cached");
    assert_eq!(rig.row("README.md").last_used, Some(first_use + 90));
}

/// A body larger than the copy buffer arrives in pieces and is written in
/// pieces: no write is the whole file.
#[test]
fn a_five_megabyte_file_is_streamed_in_pieces_and_arrives_intact() {
    let big: Vec<u8> = (0..5 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let rig = FetchRig::start(|fake| {
        fake.gh.put_file(REPO, "main", "data/big.bin", &big);
    });
    let reply = rig.fetch("data/big.bin");
    let result = result(&reply);
    assert_eq!(result["size"], json!(big.len()));
    assert_eq!(result["state"], json!("cached"));
    assert!(std::fs::read(rig.cached_path("data/big.bin")).unwrap() == big);
    assert!(
        rig.probe.writes.load(Ordering::SeqCst) > 1,
        "the body was written in more than one piece"
    );
    assert!(
        rig.probe.largest_write.load(Ordering::SeqCst) < big.len() / 2,
        "no single write held most of the file"
    );
    assert_eq!(
        rig.d.source_row("g").unwrap()["cache_bytes"],
        json!(big.len())
    );
}

/// An empty file is a file: fetched, size 0.
#[test]
fn an_empty_file_is_fetched_as_an_empty_file() {
    let rig = FetchRig::start(|fake| {
        fake.gh.put_file(REPO, "main", "empty.txt", b"");
    });
    let reply = rig.fetch("empty.txt");
    assert_eq!(result(&reply)["size"], json!(0));
    assert_eq!(std::fs::read(rig.cached_path("empty.txt")).unwrap(), b"");
    assert_eq!(rig.row("empty.txt").state, "cached");
}

/// A unit id that is not a remote item is refused with a reason.
#[test]
fn a_local_unit_is_not_remote_and_a_missing_one_is_unknown() {
    let rig = FetchRig::start(|_| {});
    let vault = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("note.md"), "# Note\n").unwrap();
    rig.d.ok(
        "source.register",
        json!({"name": "vault", "root": vault.path().display().to_string()}),
    );

    let local = rig.d.rpc("unit.fetch", json!({"unit": "@vault:note.md"}));
    assert_eq!(
        local["error"]["data"]["reason"],
        json!("not_remote"),
        "{local}"
    );

    for unit in [
        "@vault:nope.md",
        "@g:nope.md",
        "@nowhere:README.md",
        "@g:docs",
    ] {
        let reply = rig.d.rpc("unit.fetch", json!({ "unit": unit }));
        assert_eq!(
            reply["error"]["data"]["reason"],
            json!("unknown_unit"),
            "{unit}: {reply}"
        );
    }
    assert!(rig.cache_files().is_empty());
    assert_eq!(
        rig.d.rpc("unit.fetch", json!({"unit": "README.md"}))["error"]["code"],
        json!(-32602)
    );
    assert_eq!(
        rig.d.rpc("unit.fetch", json!({}))["error"]["code"],
        json!(-32602)
    );
}

/// A fetch of a file with unsent or unpushed edits never overwrites it.
#[test]
fn a_file_with_edits_is_returned_as_it_is_and_never_overwritten() {
    let rig = FetchRig::start(|_| {});
    let edited = b"# Atlas\n\nMy own edit.\n";
    for (path, state) in [
        ("README.md", "pending_upload"),
        ("docs/Guide.md", "not_pushed"),
        ("src/lib.rs", "needs_attention"),
    ] {
        let file = seed_cached_copy(&rig.d.data_dir, "g", path, edited);
        rusqlite_set_state(&rig, path, state);
        let requests = rig.fake.requests();
        let reply = rig.fetch(path);
        let result = result(&reply);
        assert_eq!(result["path"], json!(file.display().to_string()), "{path}");
        assert_eq!(result["state"], json!(state), "{path}");
        assert_eq!(result["size"], json!(edited.len()), "{path}");
        assert_eq!(std::fs::read(&file).unwrap(), edited, "{path} untouched");
        assert_eq!(rig.row(path).state, state);
        assert_eq!(
            rig.fake.requests(),
            requests,
            "{path}: nothing asked of the server"
        );
    }
}

/// A file Tephra did not write is never replaced, even when the item says
/// `remote`: the same bytes are adopted, other bytes are left alone.
#[test]
fn a_file_already_at_the_path_is_adopted_if_it_is_the_remotes_and_kept_if_not() {
    let rig = FetchRig::start(|_| {});
    // The remote's own bytes: what a killed fetch or a restored backup leaves.
    let same = seed_cached_copy(&rig.d.data_dir, "g", "README.md", README);
    let requests = rig.fake.requests();
    let reply = rig.fetch("README.md");
    assert_eq!(result(&reply)["state"], json!("cached"));
    assert_eq!(std::fs::read(&same).unwrap(), README);
    assert_eq!(
        rig.fake.requests(),
        requests,
        "the bytes were already there"
    );

    // Dan's own file at the path of a file never fetched.
    let mine = seed_cached_copy(&rig.d.data_dir, "g", "docs/Guide.md", b"mine\n");
    let reply = rig.fetch("docs/Guide.md");
    let result = result(&reply);
    assert_eq!(result["state"], json!("needs_attention"));
    assert_eq!(result["path"], json!(mine.display().to_string()));
    assert_eq!(std::fs::read(&mine).unwrap(), b"mine\n", "never replaced");
    assert_eq!(rig.row("docs/Guide.md").state, "needs_attention");
    assert_eq!(rig.fake.requests(), requests);
}

/// A copy that went missing from the disk is downloaded again.
#[test]
fn a_cached_file_removed_from_the_disk_is_fetched_again() {
    let rig = FetchRig::start(|_| {});
    result(&rig.fetch("README.md"));
    std::fs::remove_file(rig.cached_path("README.md")).unwrap();
    let reads = rig.reads();
    let reply = rig.fetch("README.md");
    assert_eq!(result(&reply)["state"], json!("cached"));
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), README);
    assert_eq!(rig.reads() - reads, 1);
}

/// Nothing is left running behind a fetch: after it, a quiet window shows no
/// new request and no new file.
#[test]
fn a_fetch_leaves_nothing_running_behind_it() {
    let rig = FetchRig::start(|_| {});
    result(&rig.fetch("README.md"));
    let requests = rig.fake.requests();
    let files = rig.cache_files();
    std::thread::sleep(QUIET);
    assert_eq!(rig.fake.requests(), requests);
    assert_eq!(rig.cache_files(), files);
}

fn rusqlite_set_state(rig: &FetchRig, path: &str, state: &str) {
    let cache = tephra_cache::Cache::open(&rig.d.cache_path).unwrap();
    cache
        .connection()
        .execute(
            "UPDATE remote_items SET state = ?3 WHERE source = ?1 AND path = ?2",
            ["g", path, state],
        )
        .unwrap();
}
