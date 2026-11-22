//! PR-5 (G4): `source.add_github` registers one picked repository on one
//! branch, takes its whole listing at one commit, and refuses everything
//! else with a plain reason and no change. Real registry, real RPC server,
//! the PR-3 fake GitHub; no network and no Keychain.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::Duration;

use remote_support::{files_under, Daemon, Fake, REPO};
use serde_json::{json, Value};

fn error_of(reply: &Value) -> (&str, &str) {
    let error = reply
        .get("error")
        .unwrap_or_else(|| panic!("expected a refusal: {reply}"));
    (
        error["data"]["reason"].as_str().unwrap_or(""),
        error["message"].as_str().unwrap_or(""),
    )
}

fn names(d: &Daemon) -> Vec<String> {
    let mut n: Vec<String> = d
        .ok("source.list", json!({}))
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    n.sort();
    n
}

// ---- the happy path ------------------------------------------------------

#[test]
fn adding_a_repo_lists_the_whole_tree_as_remote_with_nothing_cached_then_goes_active() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let head = fake.gh.head_sha(REPO, "main");

    let reply = d.add_github("github-atlas", REPO);
    assert!(reply.get("error").is_none(), "{reply}");
    let added = &reply["result"];
    assert_eq!(added["name"], json!("github-atlas"));
    assert_eq!(added["kind"], json!("github"));
    assert_eq!(added["repo"], json!(REPO));
    assert_eq!(added["branch"], json!("main"), "the default branch");
    assert_eq!(added["commit"], json!(head), "the listing is at one commit");
    assert_eq!(added["files"], json!(4));

    // The listing is there at once: every file `remote`, 0 bytes cached.
    let entries = d.entries("github-atlas");
    let files: Vec<&Value> = entries.iter().filter(|e| e["kind"] != "folder").collect();
    let mut paths: Vec<&str> = files
        .iter()
        .map(|e| e["rel_path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "README.md",
            "assets/logo.png",
            "docs/Guide.md",
            "src/lib.rs"
        ]
    );
    for e in &files {
        assert_eq!(e["state"], json!("remote"), "{e}");
        assert_eq!(e["cache_bytes"], json!(0), "{e}");
    }

    let row = d.wait_active("github-atlas");
    assert_eq!(row["kind"], json!("github"));
    assert_eq!(row["branch"], json!("main"));
    assert_eq!(row["cache_bytes"], json!(0));
    assert_eq!(row["git_bytes"], json!(0));
    assert_eq!(row["cache"], json!(true));
    // Nothing is a file on disk: no regular file under <data_dir>/remote.
    assert!(files_under(&d.data_dir.join("remote")).is_empty());
    d.close();
}

#[test]
fn the_branch_defaults_to_the_repo_default_and_an_explicit_one_is_stored() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh
        .put_file(REPO, "release", "README.md", b"# Release\n");
    let d = Daemon::start(&fake);

    d.ok("source.add_github", json!({"name": "def", "repo": REPO}));
    d.ok(
        "source.add_github",
        json!({"name": "rel", "repo": REPO, "branch": "release"}),
    );
    assert_eq!(d.source_row("def").unwrap()["branch"], json!("main"));
    assert_eq!(d.source_row("rel").unwrap()["branch"], json!("release"));
    d.wait_active("rel");
    let rel: Vec<String> = d
        .entries("rel")
        .iter()
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(rel, ["README.md"], "only that branch's tree");
    d.close();
}

#[test]
fn sources_changed_added_is_pushed_once() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let mut client = d.connect();

    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    let first = client
        .next_sources_changed(Duration::from_secs(20))
        .expect("a sources_changed push");
    assert_eq!(first, json!([{ "kind": "added", "name": "github-atlas" }]));
    d.wait_active("github-atlas");

    // Indexing may push ordinary frames, but never a second `added`.
    let again = client.next_sources_changed(Duration::from_millis(400));
    assert!(again.is_none(), "a second sources_changed: {again:?}");
    d.close();
}

#[test]
fn names_with_unicode_spaces_and_hash_round_trip_into_unit_ids() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    let awkward = [
        "docs/Résumé ünï.md",
        "notes/a b c.md",
        "notes/C# tips.md",
        "日本語/メモ.md",
        "weird/100%.md",
    ];
    for p in awkward {
        fake.gh.put_file(REPO, "main", p, b"# t\n");
    }
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    let entries = d.entries("g");
    for p in awkward {
        let entry = entries
            .iter()
            .find(|e| e["rel_path"] == json!(p))
            .unwrap_or_else(|| panic!("{p} listed"));
        assert_eq!(entry["id"], json!(format!("@g:{p}")), "id keeps the bytes");
    }
    // And an id resolves back to the file. (A `#` ends the path in link
    // syntax, so a link to `C# tips.md` cannot be written; the listing's
    // id above is what keeps it exact.)
    for p in ["docs/Résumé ünï.md", "notes/a b c.md", "日本語/メモ.md"] {
        let resolved = d.ok_when(
            "unit.resolve_link",
            json!({"link": format!("[[@g:{p}]]")}),
            |_| true,
        );
        assert_eq!(resolved["unit"]["id"], json!(format!("@g:{p}")));
    }
    d.close();
}

// ---- refusals change nothing ---------------------------------------------

#[test]
fn a_refused_add_changes_nothing_sends_nothing_and_asks_github_nothing() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let mut client = d.connect();
    let local = fixtures::empty_source();
    local.write("note.md", "# n\n");
    d.ok(
        "source.register",
        json!({"name": "Notes", "root": local.root().to_string_lossy()}),
    );
    client
        .next_sources_changed(Duration::from_secs(20))
        .expect("the local add's push");
    let before = fake.requests();

    let too_long = "x".repeat(65);
    let bad_names = [
        "a/b",
        "a b",
        "a@b",
        "a:b",
        "a#b",
        "..",
        ".",
        too_long.as_str(),
        // Collisions are case-insensitive, with a local source too.
        "notes",
        "NOTES",
    ];
    for name in bad_names {
        let reply = d.add_github(name, REPO);
        let (reason, message) = error_of(&reply);
        assert!(
            reason == "invalid_name" || reason == "name_taken",
            "{name:?}: {reason}: {message}"
        );
    }
    assert_eq!(
        fake.requests(),
        before,
        "a refused name asks GitHub nothing"
    );
    assert_eq!(names(&d), ["Notes"], "nothing was registered");

    // A remote source's name collides too.
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    let reply = d.add_github("GitHub-Atlas", REPO);
    assert_eq!(error_of(&reply).0, "name_taken");
    assert_eq!(names(&d), ["Notes", "github-atlas"]);

    // Of the pushes since: the one `added` for github-atlas, none for any
    // refusal.
    let push = client
        .next_sources_changed(Duration::from_secs(20))
        .expect("the add's push");
    assert_eq!(push, json!([{ "kind": "added", "name": "github-atlas" }]));
    d.wait_active("github-atlas");
    assert!(client
        .next_sources_changed(Duration::from_millis(400))
        .is_none());
    d.close();
}

#[test]
fn no_token_is_auth_required_and_nothing_is_registered() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start_signed_out(&fake);
    let reply = d.add_github("github-atlas", REPO);
    let (reason, message) = error_of(&reply);
    assert_eq!(reason, "auth_required");
    assert!(message.contains("tephractl auth github"), "{message}");
    assert_eq!(fake.requests(), 0, "no token, no request");
    assert!(names(&d).is_empty());
    d.close();
}

#[test]
fn a_rejected_token_is_auth_required_with_a_plain_message() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.revoke_token();
    let d = Daemon::start(&fake);
    let reply = d.add_github("github-atlas", REPO);
    let (reason, message) = error_of(&reply);
    assert_eq!(reason, "auth_required");
    assert!(message.contains("rejected"), "{message}");
    assert!(names(&d).is_empty());
    d.close();
}

#[test]
fn a_repo_that_is_missing_or_hidden_says_no_access_looks_the_same() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.add_repo("someone/private", "main");
    fake.gh.set_hidden("someone/private", true);
    let d = Daemon::start(&fake);
    for repo in ["danrfletcher/nope", "someone/private"] {
        let reply = d.add_github("g", repo);
        let (reason, message) = error_of(&reply);
        assert_eq!(reason, "not_found", "{repo}");
        assert!(
            message.contains("does not exist") && message.contains("no access"),
            "{message}"
        );
    }
    assert!(names(&d).is_empty());
    d.close();
}

#[test]
fn an_unknown_branch_is_refused_and_nothing_is_registered() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let reply = d.rpc(
        "source.add_github",
        json!({"name": "g", "repo": REPO, "branch": "no-such-branch"}),
    );
    let (reason, message) = error_of(&reply);
    assert_eq!(reason, "unknown_branch");
    assert!(message.contains("no-such-branch"), "{message}");
    assert!(names(&d).is_empty());
    d.close();
}

#[test]
fn a_repo_that_moved_is_refused_with_where_it_went() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh
        .move_repo(REPO, "https://api.github.com/repositories/1234");
    let d = Daemon::start(&fake);
    let reply = d.add_github("g", REPO);
    let (reason, message) = error_of(&reply);
    assert_eq!(reason, "repo_moved");
    assert!(message.contains("moved"), "{message}");
    assert!(names(&d).is_empty());
    d.close();
}

#[test]
fn a_github_failure_is_not_a_refusal_and_registers_nothing() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.fail_next(8, 503);
    let d = Daemon::start(&fake);
    let reply = d.add_github("g", REPO);
    assert!(reply.get("error").is_some(), "{reply}");
    assert!(names(&d).is_empty());
    d.close();
}

// ---- repo shapes ---------------------------------------------------------

#[test]
fn an_empty_repo_registers_with_no_files_and_goes_active() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    fake.gh.set_empty(REPO, true);
    let d = Daemon::start(&fake);
    let added = d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    assert_eq!(added["files"], json!(0));
    let row = d.wait_active("g");
    assert_eq!(row["index_total"], json!(0));
    assert!(d.entries("g").is_empty());
    d.close();
}

#[test]
fn a_truncated_tree_is_completed_with_no_file_lost_or_listed_twice() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    let mut want = Vec::new();
    for dir in 0..6 {
        for file in 0..5 {
            let p = format!("d{dir}/sub{file}/f{file}.md");
            fake.gh.put_file(REPO, "main", &p, b"# x\n");
            want.push(p);
        }
    }
    fake.gh.set_tree_limit(Some(7));
    let d = Daemon::start(&fake);
    let added = d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    assert_eq!(added["files"], json!(want.len()));
    d.wait_active("g");
    let mut got: Vec<String> = d
        .entries("g")
        .iter()
        .filter(|e| e["kind"] != "folder")
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    d.close();
}

// ---- restart -------------------------------------------------------------

#[test]
fn a_restart_answers_from_the_cache_with_no_adapter_call() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");
    let listed = d.entries("github-atlas");
    let kept = d.close_keeping_data();

    let before = fake.requests();
    let d2 = Daemon::restart(&fake, kept.path());
    let row = d2.source_row("github-atlas").expect("still registered");
    assert_eq!(row["status"], json!("active"));
    assert_eq!(d2.entries("github-atlas"), listed);
    assert_eq!(
        fake.requests(),
        before,
        "a restart of a finished source calls no adapter"
    );
    d2.close();
}

#[test]
fn a_restart_indexes_an_interrupted_index_again_from_scratch() {
    let fake = Fake::start();
    fake.atlas();
    // A slow fake keeps the first index unfinished while the daemon stops.
    fake.gh.set_latency(Duration::from_millis(1500));
    let d = Daemon::start(&fake);
    // The listing itself is slow too; it still completes inside the call.
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    let row = d.source_row("github-atlas").unwrap();
    assert_eq!(row["status"], json!("indexing"));
    let kept = d.close_keeping_data();
    fake.gh.set_latency(Duration::ZERO);
    fake.gh.request_log().clear();

    let d2 = Daemon::restart(&fake, kept.path());
    d2.wait_active("github-atlas");
    // The index read the text files again, and only those.
    let reads = fake.reads();
    assert!(reads.iter().any(|t| t.contains("/git/blobs/")), "{reads:?}");
    assert!(
        !fake
            .gh
            .request_log()
            .all()
            .iter()
            .any(|r| r.target().contains("/git/trees/")),
        "the listing is not taken again"
    );
    d2.close();
}
