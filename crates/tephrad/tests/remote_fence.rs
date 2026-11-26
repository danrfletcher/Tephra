//! PR-5 fence regression checks, as scanners over the request log, the data
//! dir, the cache and the logs:
//!   F1/F12  no regular file under `<data_dir>/remote`, and the only reads
//!           are the one first-index read per text file
//!   F3/F13  GET only; no clone, no push, no LFS, no mount
//!   F4      only one repo is ever added; no list-all-repos request
//!   F9      the token is in no RPC answer, no data-dir file, no log line
//!   F11     LFS pointers and submodules are listed by name, never fetched
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use remote_support::{files_under, Daemon, Fake, REPO, TOKEN};
use serde_json::{json, Value};

// ---- log capture ---------------------------------------------------------

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

/// Everything the daemon and its dependencies log at the most verbose level,
/// for the whole test binary.
fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(Sink(buf.clone()))
                .finish(),
        );
        buf
    })
    .clone()
}

fn logged() -> String {
    String::from_utf8_lossy(&logs().lock().unwrap()).into_owned()
}

// ---- scanners ------------------------------------------------------------

/// Whether `needle` is anywhere in any file under `dir` (bytes, so a SQLite
/// file or its WAL is searched too).
fn tree_contains(dir: &std::path::Path, needle: &str) -> Vec<String> {
    files_under(dir)
        .into_iter()
        .filter(|rel| {
            std::fs::read(dir.join(rel))
                .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
        .collect()
}

fn assert_only_allowed_requests(fake: &Fake) {
    for r in fake.gh.request_log().all() {
        assert_eq!(r.method, "GET", "F3: only reads: {} {}", r.method, r.path);
        let p = r.path.to_ascii_lowercase();
        // F13/F3: no git transport, no LFS, no archive download.
        for banned in [
            "git-upload-pack",
            "git-receive-pack",
            ".git/",
            "/info/lfs",
            "/lfs/",
            "/tarball",
            "/zipball",
            "/archive/",
        ] {
            assert!(!p.contains(banned), "{banned} in {}", r.path);
        }
        // F4: nothing that lists many repos.
        assert!(
            !(p == "/user/repos"
                || p.starts_with("/orgs/")
                || (p.starts_with("/users/") && p.ends_with("/repos"))
                || p.starts_with("/search/")
                || p == "/repositories"
                || p.starts_with("/installation")),
            "a list-all request: {}",
            r.path
        );
        // Only the one repo, or the user check.
        assert!(
            p == "/user" || p.starts_with(&format!("/repos/{}", REPO.to_ascii_lowercase())),
            "a request outside the one picked repo: {}",
            r.path
        );
    }
}

// ---- F4 ------------------------------------------------------------------

#[test]
fn f4_only_one_picked_repo_is_ever_added_and_no_list_all_request_is_sent() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    for bad in [
        "danrfletcher",
        "danrfletcher/*",
        "*/*",
        "*/atlas",
        "https://github.com/danrfletcher",
        "https://github.com/orgs/some-org",
        "https://github.com/danrfletcher/atlas",
        "danrfletcher/atlas/extra",
        "danrfletcher/atlas/",
        "/atlas",
        "danrfletcher/",
        "a b/c",
        "danrfletcher/atlas?tab=repos",
        "danrfletcher/at\nlas",
    ] {
        let reply = d.add_github("g", bad);
        assert_eq!(
            reply["error"]["data"]["reason"],
            json!("pick_one_repo"),
            "{bad:?}: {reply}"
        );
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("pick one repo"),
            "{bad:?}"
        );
    }
    // An empty string never reaches the registry: invalid params.
    assert!(d.add_github("g", "").get("error").is_some());
    assert_eq!(fake.requests(), 0, "refused before any request");
    assert!(d
        .ok("source.list", json!({}))
        .as_array()
        .unwrap()
        .is_empty());

    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    assert_only_allowed_requests(&fake);
    d.close();
}

// ---- F1 / F12 / F13 ------------------------------------------------------

#[test]
fn f1_f12_nothing_is_written_under_the_data_dir_remote_and_the_only_reads_are_the_index_reads() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");

    assert!(
        files_under(&d.data_dir.join("remote")).is_empty(),
        "F1: a regular file under <data_dir>/remote"
    );
    // No clone and no .git directory anywhere in the data dir.
    for rel in files_under(&d.data_dir) {
        assert!(
            !rel.components().any(|c| c.as_os_str() == ".git"),
            "F13/F3: a git dir: {}",
            rel.display()
        );
    }
    // F12: the reads are the first-index reads, one blob per text file.
    let reads = fake.reads();
    assert_eq!(reads.len(), 3, "{reads:?}");
    assert!(reads.iter().all(|t| t.contains("/git/blobs/")), "{reads:?}");
    let mut unique = reads.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), reads.len(), "no blob read twice");
    assert_only_allowed_requests(&fake);

    // F2: no file operation of any kind on a remote file exists in this
    // daemon surface: `source.add_github` is the only remote verb.
    d.close();
}

#[test]
fn f2_remote_sources_cannot_be_renamed_but_can_be_removed_since_pr_13() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    let rename = d.rpc("source.rename", json!({"name": "g", "new_name": "h"}));
    assert!(rename.get("error").is_some(), "{rename}");
    assert!(
        d.source_row("g").is_some(),
        "a refused rename changes nothing"
    );
    // Removal is PR-13: a source with no unsent work goes (see
    // `remote_source_remove.rs` for everything else about it).
    let remove = d.rpc("source.unregister", json!({"name": "g"}));
    assert!(remove.get("error").is_none(), "{remove}");
    assert!(d.source_row("g").is_none(), "removed");
    d.close();
}

#[test]
fn f2_no_refactor_or_fix_ever_writes_a_remote_file_or_a_path_made_from_its_marker() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let vault = fixtures::empty_source();
    vault.write("note.md", "# Note\n\n[[@github-atlas:README.md]]\n");
    d.ok(
        "source.register",
        json!({"name": "vault", "root": vault.root().to_string_lossy()}),
    );
    d.ok(
        "source.add_github",
        json!({"name": "github-atlas", "repo": REPO}),
    );
    d.wait_active("github-atlas");

    // Renaming a remote file is refused at the preview; with no preview,
    // apply has nothing to write.
    let rename = json!({"source": "github-atlas", "old_path": "README.md", "new_path": "ABOUT.md"});
    let preview = d.rpc("link.preview_refactor", rename.clone());
    let msg = preview["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("remote source"), "{preview}");
    assert!(d.rpc("link.apply_refactor", rename).get("error").is_some());
    // Renaming a vault file that the README links to: the README is not
    // rewritten; it is reported as skipped.
    let preview = d.ok(
        "link.preview_refactor",
        json!({"source": "vault", "old_path": "note.md", "new_path": "moved.md"}),
    );
    assert!(preview.to_string().contains("remote file"), "{preview}");

    // Nothing was created from the marker path, relative to anywhere.
    assert!(!std::path::Path::new("remote:github:github-atlas").exists());
    assert!(files_under(&d.data_dir.join("remote")).is_empty());
    d.close();
}

// ---- F11 -----------------------------------------------------------------

#[test]
fn f11_lfs_pointers_and_submodules_are_listed_by_name_and_never_fetched() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    fake.gh.put_file(
        REPO,
        "main",
        "big.bin.txt",
        b"version https://git-lfs.github.com/spec/v1\noid sha256:aa\nsize 99\n",
    );
    fake.gh
        .put_submodule(REPO, "main", "ext/sub", "https://github.com/other/sub.git");
    fake.gh.put_symlink(REPO, "main", "link", "big.bin.txt");
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    let entries = d.entries("g");
    for p in ["big.bin.txt", "ext/sub", "link"] {
        assert!(
            entries.iter().any(|e| e["rel_path"] == json!(p)),
            "{p} listed"
        );
    }
    assert_only_allowed_requests(&fake);
    for r in fake.gh.request_log().all() {
        assert!(!r.path.to_ascii_lowercase().contains("lfs"), "{}", r.path);
        assert!(!r.path.contains("other/sub"), "{}", r.path);
    }
    d.close();
}

// ---- F9 ------------------------------------------------------------------

/// A token-bearing error body: the fake echoes the Authorization it was
/// sent. Whatever the daemon does with the error, the token goes nowhere.
fn assert_token_nowhere(d: &Daemon, answers: &[Value]) {
    for a in answers {
        assert!(
            !a.to_string().contains(TOKEN),
            "token in an RPC answer: {a}"
        );
    }
    let rows = d.ok("source.list", json!({}));
    assert!(!rows.to_string().contains(TOKEN));
    assert!(
        tree_contains(&d.data_dir, TOKEN).is_empty(),
        "F9: the token is in {:?}",
        tree_contains(&d.data_dir, TOKEN)
    );
    assert!(!logged().contains(TOKEN), "F9: the token is in the logs");
}

#[test]
fn f9_a_401_that_echoes_the_token_leaves_it_out_of_rpc_cache_data_dir_and_logs() {
    let _ = logs();
    let fake = Fake::start();
    fake.atlas();
    fake.gh.echo_authorization_in_errors(true);
    fake.gh.revoke_token();
    let d = Daemon::start(&fake);
    let reply = d.add_github("g", REPO);
    assert!(reply.get("error").is_some(), "{reply}");
    assert_token_nowhere(&d, &[reply]);
    d.close();
}

#[test]
fn f9_a_5xx_that_echoes_the_token_leaves_it_out_too() {
    let _ = logs();
    let fake = Fake::start();
    fake.atlas();
    fake.gh.echo_authorization_in_errors(true);
    fake.gh.fail_next(20, 500);
    let d = Daemon::start(&fake);
    let reply = d.add_github("g", REPO);
    assert!(reply.get("error").is_some(), "{reply}");
    assert_token_nowhere(&d, &[reply]);
    d.close();
}

#[test]
fn f9_a_token_revoked_during_the_index_leaves_it_out_too() {
    let _ = logs();
    let fake = Fake::start();
    fake.atlas();
    fake.gh.echo_authorization_in_errors(true);
    fake.gh.set_latency(std::time::Duration::from_millis(700));
    let d = Daemon::start(&fake);
    let added = d.rpc("source.add_github", json!({"name": "g", "repo": REPO}));
    assert!(added.get("error").is_none(), "{added}");
    fake.gh.revoke_token();
    let row = d.wait_for("auth_required", || {
        d.source_row("g")
            .filter(|r| r["status"] == json!("auth_required"))
    });
    assert_token_nowhere(&d, &[added, row]);
    d.close();
}

#[test]
fn f9_the_token_is_in_no_file_even_when_everything_works() {
    let _ = logs();
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let added = d.rpc("source.add_github", json!({"name": "g", "repo": REPO}));
    let row = d.wait_active("g");
    assert_token_nowhere(&d, &[added, row]);
    // The daemon did send it, to GitHub: that is the point of the store.
    assert!(fake
        .gh
        .request_log()
        .all()
        .iter()
        .all(|r| r.header("authorization").is_some()));
    d.close();
}
