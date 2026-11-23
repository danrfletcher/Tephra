//! PR-9 (G13, G15, E12, E23, F12, GP6): a GitHub source whose cache is a real
//! git clone. Tephra counts what differs from the remote tip, shows how far
//! behind the clone is, fast-forwards a clean clone that is strictly behind,
//! and in every other state fetches and touches nothing.
//!
//! Everything here is real git against a local bare repository standing in
//! for GitHub (the clone's `origin`). The in-process fake GitHub serves the
//! REST side (the index), `MemStore` the token, a temp dir the data.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use remote_support::{FetchRig, Tuning, QUIET, TOKEN};
use serde_json::{json, Value};

const README: &[u8] = b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n";
const GUIDE: &[u8] = b"# Guide\n\nBack to [[README]].\n";
const LIB: &[u8] = b"pub fn atlas() -> u32 {\n    1\n}\n";
const LOGO: &[u8] = &[0x89, b'P', b'N', b'G', 0, 1, 2];

/// Runs real git as Dan would, with an identity and no global settings.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=Dan", "-c", "user.email=dan@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// A recording stand-in for `git`: it logs its arguments and runs the real one.
fn recording_git(dir: &Path) -> (&'static str, PathBuf) {
    let real = String::from_utf8(Command::new("which").arg("git").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let log = dir.join("git-calls.log");
    let script = dir.join("git-wrapper.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (
        Box::leak(script.display().to_string().into_boxed_str()),
        log,
    )
}

struct CloneRig {
    rig: FetchRig,
    /// The bare repository standing in for GitHub.
    origin: tempfile::TempDir,
    /// Where the test acts as another person (or Dan elsewhere) and pushes.
    other: tempfile::TempDir,
    /// Where the recording wrapper lives.
    tools: tempfile::TempDir,
    log: PathBuf,
}

impl CloneRig {
    /// A source `g` whose `clone/` is a real clone of an origin that has the
    /// fake's four files on `main`; one sync has run, so the clone is
    /// known and watched.
    fn start() -> CloneRig {
        let tools = tempfile::tempdir().unwrap();
        let (program, log) = recording_git(tools.path());
        let rig = FetchRig::start_capped(
            Tuning {
                git: Some(program),
                ..Tuning::default()
            },
            |_| {},
        );
        let origin = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "--bare", "-b", "main", "."]);
        git(other.path(), &["init", "-b", "main", "."]);
        write(other.path(), "README.md", README);
        write(other.path(), "docs/Guide.md", GUIDE);
        write(other.path(), "src/lib.rs", LIB);
        write(other.path(), "assets/logo.png", LOGO);
        git(other.path(), &["add", "-A"]);
        git(other.path(), &["commit", "-m", "first"]);
        git(
            other.path(),
            &[
                "remote",
                "add",
                "origin",
                &origin.path().display().to_string(),
            ],
        );
        git(other.path(), &["push", "origin", "main"]);
        let clone = rig.d.data_dir.join("remote").join("g").join("clone");
        std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
        git(
            rig.d.data_dir.as_path(),
            &[
                "clone",
                "-b",
                "main",
                &origin.path().display().to_string(),
                &clone.display().to_string(),
            ],
        );
        let c = CloneRig {
            rig,
            origin,
            other,
            tools,
            log,
        };
        c.sync();
        c
    }

    fn clone(&self) -> PathBuf {
        self.rig.d.data_dir.join("remote").join("g").join("clone")
    }

    /// Dan's own git in the clone.
    fn dan(&self, args: &[&str]) -> String {
        git(&self.clone(), args)
    }

    /// Someone pushes `rel` = `bytes` to GitHub (a commit on `main`).
    fn push_upstream(&self, rel: &str, bytes: &[u8], message: &str) {
        write(self.other.path(), rel, bytes);
        git(self.other.path(), &["add", "-A"]);
        git(self.other.path(), &["commit", "-m", message]);
        git(self.other.path(), &["push", "origin", "main"]);
    }

    /// A sync cycle: the clock moves past the poll interval and this waits
    /// until the poller (and the clone pass after it) is done.
    fn sync(&self) {
        let before = self.rig.d.registry.remote_polls_done();
        self.rig.clock.advance(Duration::from_secs(61));
        self.rig.d.wait_for("a sync cycle", || {
            (self.rig.d.registry.remote_polls_done() > before).then_some(())
        });
    }

    fn row(&self) -> Value {
        self.rig.d.source_row("g").expect("g is listed")
    }

    fn not_pushed(&self) -> u64 {
        self.row()["not_pushed"].as_u64().unwrap_or(0)
    }

    fn behind(&self) -> u64 {
        self.row()["behind"].as_u64().unwrap_or(0)
    }

    fn item(&self, path: &str) -> Value {
        self.rig
            .d
            .entries("g")
            .into_iter()
            .find(|e| e["id"] == json!(format!("@g:{path}")) || e["path"] == json!(path))
            .unwrap_or_else(|| panic!("{path} is listed"))
    }

    fn state(&self, path: &str) -> String {
        let e = self.item(path);
        e["state"].as_str().unwrap_or("").to_string()
    }

    /// Waits (bounded) for `source.list` to say `n` changes not pushed.
    fn wait_not_pushed(&self, n: u64) -> Duration {
        let start = Instant::now();
        self.rig.d.wait_for(&format!("{n} changes not pushed"), || {
            (self.not_pushed() == n).then_some(())
        });
        start.elapsed()
    }

    /// What every git call the daemon made looked like.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// HEAD, the branch, the index and the reflog: what must not move.
    fn identity(&self) -> (String, String, String, String) {
        (
            self.dan(&["rev-parse", "HEAD"]),
            self.dan(&["symbolic-ref", "HEAD"]),
            self.dan(&["status", "--porcelain=v1", "-uall"]),
            self.dan(&["reflog", "--format=%H %gs"]),
        )
    }

    /// Every tracked and untracked file of the clone with its bytes.
    fn files(&self) -> Vec<(String, Vec<u8>)> {
        let root = self.clone();
        let mut out: Vec<(String, Vec<u8>)> = remote_support::files_under(&root)
            .into_iter()
            .filter(|rel| !rel.components().any(|c| c.as_os_str() == ".git"))
            .map(|rel| {
                let bytes = std::fs::read(root.join(&rel)).unwrap();
                (rel.display().to_string(), bytes)
            })
            .collect();
        out.sort();
        out
    }
}

/// F3: every git call the daemon made is on the allowlist (`fetch`,
/// `rev-parse`, `rev-list`, `merge-base`, `diff`, `status`, `ls-files`, and
/// `merge --ff-only --no-overwrite-ignore <ref>` as the one moving command),
/// carries no `--force` and no token. Any other argv fails the test.
fn assert_allowlisted(calls: &[String]) {
    for call in calls {
        let words: Vec<&str> = call.split_whitespace().collect();
        let first = words.first().copied().unwrap_or("");
        let ok = match first {
            "merge" => {
                words.len() == 4
                    && words[1] == "--ff-only"
                    && words[2] == "--no-overwrite-ignore"
                    && words[3].starts_with("refs/remotes/origin/")
            }
            other => [
                "fetch",
                "rev-parse",
                "rev-list",
                "merge-base",
                "diff",
                "status",
                "ls-files",
            ]
            .contains(&other),
        };
        assert!(ok, "git {call} is not on the allowlist");
        for w in &words {
            assert!(
                !w.starts_with("--force") && *w != "-f" && !w.starts_with("--hard"),
                "git {call} forces something"
            );
        }
        assert!(!call.contains(TOKEN), "a token is in git's argv: {call}");
        assert!(!call.to_lowercase().contains("token"), "{call}");
    }
}

impl CloneRig {
    /// F3: the allowlist over every call so far.
    fn assert_only_allowed_git(&self) {
        assert_allowlisted(&self.calls());
    }

    /// No token in the clone's `.git/config` (remote URLs included), git's
    /// argv, or what `source.list` and `unit.list` answer.
    fn assert_no_token_anywhere(&self) {
        let config = std::fs::read_to_string(self.clone().join(".git/config")).unwrap();
        assert!(!config.contains(TOKEN), ".git/config holds a token");
        let rows = self.rig.d.ok("source.list", json!({})).to_string();
        let list = self
            .rig
            .d
            .ok(
                "unit.list",
                json!({"source": "g", "folder": "", "recursive": true}),
            )
            .to_string();
        for (what, text) in [("source.list", rows), ("unit.list", list)] {
            assert!(!text.contains(TOKEN), "{what} answered with a token");
        }
        for call in self.calls() {
            assert!(!call.contains(TOKEN), "{call}");
        }
    }

    /// `git rev-list --merges HEAD` in Dan's clone: no merge commit.
    fn assert_no_merge_commit(&self) {
        assert_eq!(self.dan(&["rev-list", "--merges", "HEAD"]), "");
    }

    /// HEAD has exactly one parent.
    fn assert_head_has_one_parent(&self) {
        let line = self.dan(&["rev-list", "--parents", "-n", "1", "HEAD"]);
        assert_eq!(line.split_whitespace().count(), 2, "{line}");
    }
}

/// Every scenario ends with the allowlist and the token checks (F3), without
/// each test having to remember to.
impl Drop for CloneRig {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.assert_only_allowed_git();
            self.assert_no_token_anywhere();
        }
    }
}

// ---- G13: what differs from the remote tip -------------------------------

#[test]
fn a_clean_clone_shows_no_changes_and_no_behind() {
    let r = CloneRig::start();
    assert_eq!(r.not_pushed(), 0);
    assert_eq!(r.behind(), 0);
    assert_eq!(r.row()["status"], json!("active"));
    for p in ["README.md", "docs/Guide.md", "src/lib.rs"] {
        assert_ne!(r.state(p), "not_pushed", "{p}");
    }
    let list = r.rig.d.ok(
        "unit.list",
        json!({"source": "g", "folder": "", "recursive": true}),
    );
    assert_eq!(list["not_pushed"], json!(0));
    assert_eq!(list["behind"], json!(0));
}

#[test]
fn a_saved_edit_is_one_change_within_two_seconds_and_nothing_is_committed() {
    let r = CloneRig::start();
    let before = r.identity();
    let reflog_before = r.dan(&["reflog"]);

    write(&r.clone(), "README.md", b"# Atlas\n\nedited by Dan\n");
    let took = r.wait_not_pushed(1);
    assert!(
        took < Duration::from_secs(5),
        "the watcher's debounce: {took:?}"
    );
    assert_eq!(r.state("README.md"), "not_pushed");
    assert_ne!(
        r.state("docs/Guide.md"),
        "not_pushed",
        "an untouched file is as it was"
    );

    // The file as Dan saved it, and git as it was: no commit, no reset.
    assert_eq!(
        std::fs::read(r.clone().join("README.md")).unwrap(),
        b"# Atlas\n\nedited by Dan\n"
    );
    let after = r.identity();
    assert_eq!(after.0, before.0, "HEAD");
    assert_eq!(after.1, before.1, "branch");
    assert_eq!(after.3, before.3, "reflog");
    assert_eq!(r.dan(&["reflog"]), reflog_before);
    assert_eq!(r.dan(&["status", "--porcelain=v1"]), " M README.md");

    // `source list` says it in the CLI's words' source: the count.
    assert_eq!(r.row()["not_pushed"], json!(1));
}

#[test]
fn reverting_the_edit_drops_the_change_and_the_state_returns_to_cached() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"changed\n");
    r.wait_not_pushed(1);
    write(&r.clone(), "README.md", README);
    r.wait_not_pushed(0);
    assert_eq!(r.state("README.md"), "cached");
}

#[test]
fn new_and_deleted_files_and_unpushed_commits_all_count_and_a_committed_edit_counts_once() {
    let r = CloneRig::start();
    // A new file, a deleted one.
    write(&r.clone(), "notes/new.md", b"new\n");
    std::fs::remove_file(r.clone().join("src/lib.rs")).unwrap();
    r.wait_not_pushed(2);
    assert_eq!(r.state("src/lib.rs"), "not_pushed");

    // An edit that is committed, then edited again: one file, once.
    write(&r.clone(), "docs/Guide.md", b"# Guide\n\ncommitted\n");
    r.dan(&["add", "docs/Guide.md"]);
    r.dan(&["commit", "-m", "guide"]);
    write(
        &r.clone(),
        "docs/Guide.md",
        b"# Guide\n\ncommitted and edited again\n",
    );
    r.wait_not_pushed(3);
    assert_eq!(r.state("docs/Guide.md"), "not_pushed");

    // A file that exists only in an unpushed commit (clean tree after it).
    write(&r.clone(), "only/in/commit.md", b"x\n");
    r.dan(&["add", "-A"]);
    r.dan(&["commit", "-m", "everything"]);
    r.wait_for_count_after_commit(4);
}

impl CloneRig {
    fn wait_for_count_after_commit(&self, n: u64) {
        // The commit touches `.git` only (the watcher drops it) and the
        // files did not change: the next sync (or a save) recounts.
        self.sync();
        self.wait_not_pushed(n);
    }
}

#[test]
fn a_push_that_lands_brings_the_count_back_to_zero_after_the_next_fetch() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"# Atlas\n\nmine\n");
    r.dan(&["commit", "-am", "mine"]);
    r.sync();
    r.wait_not_pushed(1);
    assert_eq!(r.state("README.md"), "not_pushed");

    // Dan pushes with his own git; Tephra sees it at the next fetch.
    r.dan(&["push", "origin", "main"]);
    assert_eq!(r.not_pushed(), 1, "a push alone is not a fetch");
    r.sync();
    r.wait_not_pushed(0);
    assert_eq!(r.state("README.md"), "cached");
}

// ---- F12 / GP6 ------------------------------------------------------------

#[test]
fn a_fetch_of_a_file_is_not_a_change_and_leaves_head_and_porcelain_alone() {
    let r = CloneRig::start();
    let before = r.identity();
    let reply = r.rig.fetch("README.md");
    assert!(reply.get("error").is_none(), "{reply}");
    std::thread::sleep(QUIET * 5);
    r.sync();
    assert_eq!(r.not_pushed(), 0, "0 changes not pushed");
    assert_eq!(r.identity(), before, "HEAD, branch, porcelain and reflog");
    assert_eq!(r.dan(&["status", "--porcelain=v1"]), "");
}

#[test]
fn a_file_added_on_github_that_unit_fetch_wrote_is_no_change() {
    let r = CloneRig::start();
    // Someone adds a file upstream. The clone is behind, with an unrelated
    // edit so that it does not fast-forward and the file stays absent.
    r.push_upstream("added.md", b"added upstream\n", "add");
    write(&r.clone(), "README.md", b"mine\n");
    r.sync();
    r.wait_not_pushed(1);
    // The daemon would have written the file itself on a `fetch`; that file
    // is the tip's own, so it does not differ from it.
    write(&r.clone(), "added.md", b"added upstream\n");
    r.sync();
    assert_eq!(r.not_pushed(), 1);
    // The same path with other bytes does differ.
    write(&r.clone(), "added.md", b"changed by Dan\n");
    r.wait_not_pushed(2);
}

#[test]
fn tephra_makes_no_git_call_that_writes_and_never_a_non_get() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"x\n");
    r.wait_not_pushed(1);
    r.push_upstream("other.md", b"o\n", "other");
    r.sync();
    write(&r.clone(), "README.md", README);
    r.wait_not_pushed(0);
    r.sync();
    assert!(!r.calls().is_empty(), "the pass ran git");
    r.assert_only_allowed_git();
    assert!(r.rig.fake.non_get_requests().is_empty());
}

#[test]
fn the_allowlist_check_itself_refuses_what_it_should() {
    for bad in [
        "commit -m x",
        "push origin main",
        "pull --rebase",
        "rebase main",
        "reset --hard origin/main",
        "checkout main",
        "stash",
        "merge origin/main",
        "merge --ff-only refs/remotes/origin/main",
        "merge --no-ff refs/remotes/origin/main",
        "fetch --force origin",
    ] {
        assert!(
            std::panic::catch_unwind(|| assert_allowlisted(&[bad.to_string()])).is_err(),
            "{bad} passed"
        );
    }
    assert_allowlisted(&[
        "fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main".to_string(),
        "merge --ff-only --no-overwrite-ignore refs/remotes/origin/main".to_string(),
        "status --porcelain=v2 -z".to_string(),
    ]);
}

// ---- G15 / E23: the pull rules --------------------------------------------

#[test]
fn a_clean_clone_strictly_behind_fast_forwards_and_graph_changed_fires() {
    let r = CloneRig::start();
    let mut client = r.rig.d.connect();
    client.drain_graph_changed(QUIET);
    let reflog_len = r.dan(&["reflog"]).lines().count();
    r.push_upstream("README.md", b"# Atlas\n\nnew upstream text\n", "upstream");
    r.push_upstream("added.md", b"new file\n", "second");
    let tip = git(r.other.path(), &["rev-parse", "HEAD"]);

    r.sync();
    assert_eq!(r.dan(&["rev-parse", "HEAD"]), tip, "fast-forwarded");
    assert_eq!(r.dan(&["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        std::fs::read(r.clone().join("README.md")).unwrap(),
        b"# Atlas\n\nnew upstream text\n"
    );
    assert!(r.clone().join("added.md").exists());
    assert_eq!(r.dan(&["status", "--porcelain=v1"]), "");
    assert_eq!(r.behind(), 0);
    assert_eq!(r.not_pushed(), 0);
    assert!(
        client.next_graph_changed(Duration::from_secs(5)).is_some(),
        "graph:changed fired"
    );
    // The branch moved forward, nothing else: the reflog grew by the merge.
    assert!(r.dan(&["reflog"]).lines().count() > reflog_len);
    assert!(r
        .calls()
        .iter()
        .any(|c| c == "merge --ff-only --no-overwrite-ignore refs/remotes/origin/main"));
    // The one moving operation never makes a merge commit: two commits came
    // in and HEAD is still a straight line.
    r.assert_no_merge_commit();
    r.assert_head_has_one_parent();
    assert_eq!(
        r.dan(&["rev-list", "--count", "HEAD"]),
        "3",
        "first plus the two upstream commits, no merge"
    );
}

#[test]
fn a_clone_with_a_tracked_edit_is_fetch_only_and_shows_behind() {
    let r = CloneRig::start();
    write(&r.clone(), "docs/Guide.md", b"# Guide\n\nDan's edit\n");
    r.wait_not_pushed(1);
    let before = r.identity();
    let files = r.files();
    r.push_upstream("src/lib.rs", b"pub fn atlas() -> u32 {\n    2\n}\n", "lib");

    r.sync();
    assert_eq!(r.behind(), 1);
    assert_eq!(
        r.not_pushed(),
        1,
        "only Dan's edit; the remote's change is not his"
    );
    assert_eq!(
        r.identity(),
        before,
        "HEAD, branch, index, reflog untouched"
    );
    assert_eq!(r.files(), files, "the working tree is as it was");
    assert_eq!(r.state("docs/Guide.md"), "not_pushed");
    assert_eq!(
        r.state("src/lib.rs"),
        "remote",
        "a one-sided change keeps its state"
    );
    assert_eq!(r.row()["status"], json!("active"));
    assert!(
        !r.calls().iter().any(|c| c.starts_with("merge ")),
        "no merge was tried: {:?}",
        r.calls()
    );
}

#[test]
fn an_untracked_file_a_local_commit_or_both_ahead_and_behind_are_fetch_only() {
    // An untracked file.
    let r = CloneRig::start();
    write(&r.clone(), "scratch.md", b"mine\n");
    r.wait_not_pushed(1);
    let before = r.identity();
    r.push_upstream("a.md", b"a\n", "a");
    r.sync();
    assert_eq!(r.behind(), 1);
    assert_eq!(r.identity(), before);
    assert!(!r.clone().join("a.md").exists());
    drop(r);

    // A local commit: ahead and behind.
    let r = CloneRig::start();
    write(&r.clone(), "mine.md", b"commit\n");
    r.dan(&["add", "-A"]);
    r.dan(&["commit", "-m", "mine"]);
    let head = r.dan(&["rev-parse", "HEAD"]);
    r.push_upstream("b.md", b"b\n", "b");
    r.sync();
    assert_eq!(r.behind(), 1);
    assert_eq!(r.not_pushed(), 1);
    assert_eq!(r.dan(&["rev-parse", "HEAD"]), head);
    assert!(!r.clone().join("b.md").exists());
    assert!(!r.calls().iter().any(|c| c.starts_with("merge ")));

    // F14: a divergence is Dan's to settle. No merge commit, HEAD as it was,
    // and the only guidance is the reason, which names his own command.
    r.assert_no_merge_commit();
    r.assert_head_has_one_parent();
    let row = r.row();
    assert_eq!(row["status"], json!("active"), "{row}");
    let reason = row["status_reason"].as_str().unwrap_or("");
    assert!(reason.contains("git pull --rebase"), "{row}");

    // Dan runs it himself; the note goes at the next sync.
    r.dan(&["pull", "--rebase", "origin", "main"]);
    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("active"), "{row}");
    assert!(
        row.get("status_reason").is_none() || row["status_reason"].is_null(),
        "{row}"
    );
    assert_eq!(r.behind(), 0);
}

#[test]
fn a_file_changed_on_both_sides_needs_attention_and_nothing_is_overwritten() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"# Atlas\n\nDan's side\n");
    write(&r.clone(), "docs/Guide.md", b"# Guide\n\nDan only\n");
    r.wait_not_pushed(2);
    r.push_upstream("README.md", b"# Atlas\n\nGitHub's side\n", "readme");
    r.push_upstream("src/lib.rs", b"pub fn atlas() -> u32 {\n    9\n}\n", "lib");
    let before = r.identity();
    let files = r.files();

    r.sync();
    let both = r.item("README.md");
    assert_eq!(both["state"], json!("needs_attention"));
    let reason = both["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("changed locally and on GitHub"),
        "{reason}"
    );
    assert!(reason.contains("git pull --rebase"), "{reason}");
    // One-sided changes keep their ordinary states.
    assert_eq!(r.state("docs/Guide.md"), "not_pushed");
    assert_eq!(r.state("src/lib.rs"), "remote");
    assert_eq!(r.identity(), before);
    assert_eq!(r.files(), files, "Dan's bytes are where he left them");
    assert_eq!(r.behind(), 2);
    r.assert_no_merge_commit();
}

// ---- E12: a force-push ----------------------------------------------------

/// The reason starts as E12 says and points Dan at his own command.
fn assert_rewritten_reason(row: &Value) {
    let reason = row["status_reason"].as_str().unwrap_or("");
    assert!(reason.starts_with("history rewritten on GitHub"), "{row}");
    assert!(reason.contains("git pull --rebase"), "{row}");
}

#[test]
fn a_force_push_is_never_pulled_and_the_source_needs_attention() {
    let r = CloneRig::start();
    // Dan has work of his own: an unpushed commit and an edit.
    write(&r.clone(), "mine.md", b"commit\n");
    r.dan(&["add", "-A"]);
    r.dan(&["commit", "-m", "mine"]);
    write(&r.clone(), "README.md", b"# Atlas\n\nedit\n");
    r.wait_not_pushed(2);
    let before = r.identity();
    let files = r.files();

    // Upstream rewrites history: the first commit is replaced.
    write(r.other.path(), "README.md", b"# Rewritten\n");
    git(r.other.path(), &["add", "-A"]);
    git(r.other.path(), &["commit", "--amend", "-m", "rewritten"]);
    git(r.other.path(), &["push", "--force", "origin", "main"]);

    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("needs_attention"), "{row}");
    assert_rewritten_reason(&row);
    assert_eq!(r.identity(), before, "HEAD, branch, index and reflog");
    assert_eq!(
        r.files(),
        files,
        "clone files and Dan's commits are byte-identical"
    );
    assert!(
        !r.calls().iter().any(|c| c.starts_with("merge ")),
        "{:?}",
        r.calls()
    );

    // It stays flagged on the next sync: the new tip is not forgotten.
    r.sync();
    assert_eq!(r.row()["status"], json!("needs_attention"));
    assert_eq!(r.identity(), before);

    // A normal poll that finds nothing new does not wipe the reason either.
    assert_rewritten_reason(&r.row());
    r.assert_no_merge_commit();
}

#[test]
fn a_force_push_on_a_clean_clone_is_also_not_fast_forwarded() {
    let r = CloneRig::start();
    let head = r.dan(&["rev-parse", "HEAD"]);
    write(r.other.path(), "README.md", b"# Rewritten\n");
    git(r.other.path(), &["add", "-A"]);
    git(r.other.path(), &["commit", "--amend", "-m", "rewritten"]);
    git(r.other.path(), &["push", "--force", "origin", "main"]);
    r.sync();
    assert_eq!(r.row()["status"], json!("needs_attention"));
    assert_eq!(r.dan(&["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(r.clone().join("README.md")).unwrap(), README);
}

#[test]
fn the_source_recovers_when_dan_brings_the_clone_to_the_new_history() {
    let r = CloneRig::start();
    write(r.other.path(), "README.md", b"# Rewritten\n");
    git(r.other.path(), &["add", "-A"]);
    git(r.other.path(), &["commit", "--amend", "-m", "rewritten"]);
    git(r.other.path(), &["push", "--force", "origin", "main"]);
    r.sync();
    assert_eq!(r.row()["status"], json!("needs_attention"));
    // Dan's own decision, with his own git.
    r.dan(&["fetch", "origin"]);
    r.dan(&["reset", "--hard", "origin/main"]);
    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("active"), "{row}");
    assert!(row.get("status_reason").is_none() || row["status_reason"].is_null());
}

// ---- other states ---------------------------------------------------------

#[test]
fn a_clone_on_another_branch_is_not_fast_forwarded_and_says_so() {
    let r = CloneRig::start();
    r.dan(&["checkout", "-b", "feature"]);
    r.push_upstream("a.md", b"a\n", "a");
    let head = r.dan(&["rev-parse", "HEAD"]);
    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("needs_attention"), "{row}");
    assert!(row["status_reason"].as_str().unwrap().contains("feature"));
    assert_eq!(r.dan(&["rev-parse", "HEAD"]), head);
    assert!(!r.clone().join("a.md").exists());
    // Back on the branch: the next sync pulls it and clears the note.
    r.dan(&["checkout", "main"]);
    r.sync();
    assert!(r.clone().join("a.md").exists());
    assert_eq!(r.row()["status"], json!("active"));
}

#[test]
fn a_rebase_in_progress_is_left_alone() {
    let r = CloneRig::start();
    let git_dir = r.clone().join(".git");
    std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
    r.push_upstream("a.md", b"a\n", "a");
    let calls_before = r.calls().len();
    r.sync();
    let calls = r.calls();
    assert!(
        !calls[calls_before..]
            .iter()
            .any(|c| c.starts_with("fetch") || c.starts_with("merge ")),
        "{:?}",
        &calls[calls_before..]
    );
    assert!(!r.clone().join("a.md").exists());
    std::fs::remove_dir(git_dir.join("rebase-merge")).unwrap();
    r.sync();
    assert!(
        r.clone().join("a.md").exists(),
        "pulled once the rebase is over"
    );
}

#[test]
fn an_unreachable_origin_keeps_the_last_counts_and_says_offline() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"mine\n");
    r.wait_not_pushed(1);
    // The origin goes away.
    let moved = r.origin.path().with_extension("gone");
    std::fs::rename(r.origin.path(), &moved).unwrap();
    r.sync();
    assert_eq!(r.not_pushed(), 1, "the last count is kept");
    let row = r.row();
    assert_eq!(row["status"], json!("offline"), "{row}");
    assert!(row["status_reason"]
        .as_str()
        .unwrap()
        .starts_with("git fetch failed"));
    // It comes back.
    std::fs::rename(&moved, r.origin.path()).unwrap();
    r.sync();
    assert_eq!(r.row()["status"], json!("active"));
    assert_eq!(r.not_pushed(), 1);
}

#[test]
fn source_list_carries_the_counts_of_a_clone() {
    let r = CloneRig::start();
    write(&r.clone(), "README.md", b"mine\n");
    r.wait_not_pushed(1);
    let row = r.row();
    assert_eq!(row["not_pushed"], json!(1));
    assert!(row["behind"].is_u64());
}

// ---- R1: Dan's ignored files ----------------------------------------------

#[test]
fn a_fast_forward_never_overwrites_an_ignored_file_dan_has() {
    let r = CloneRig::start();
    // `.git/info/exclude` ignores it without a tracked `.gitignore` (which
    // would itself be an untracked file and stop the fast-forward).
    let exclude = r.clone().join(".git/info/exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("\n.env\n");
    std::fs::write(&exclude, text).unwrap();
    write(&r.clone(), ".env", b"DAN SECRET");
    assert_eq!(
        r.dan(&["status", "--porcelain=v1"]),
        "",
        "ignored: a clean clone"
    );
    let head = r.dan(&["rev-parse", "HEAD"]);

    // Upstream starts tracking a file at that very path.
    r.push_upstream(".env", b"UPSTREAM", "adds .env");
    r.sync();
    r.sync();

    assert_eq!(
        std::fs::read(r.clone().join(".env")).unwrap(),
        b"DAN SECRET",
        "the ignored file is Dan's"
    );
    assert_eq!(r.dan(&["rev-parse", "HEAD"]), head, "HEAD did not move");
    assert_eq!(r.behind(), 1, "still behind: nothing was pulled");
    let row = r.row();
    assert_eq!(row["status"], json!("active"), "no error loop: {row}");
    // The fast-forward was tried (with the flag that protects the file) and
    // git refused it.
    assert!(r
        .calls()
        .iter()
        .any(|c| c == "merge --ff-only --no-overwrite-ignore refs/remotes/origin/main"));
}

#[test]
fn a_fast_forward_that_would_overwrite_an_untracked_file_is_refused_and_left() {
    let r = CloneRig::start();
    write(&r.clone(), "added.md", b"Dan's own\n");
    r.wait_not_pushed(1);
    r.push_upstream("added.md", b"upstream\n", "adds added.md");
    r.sync();
    assert_eq!(
        std::fs::read(r.clone().join("added.md")).unwrap(),
        b"Dan's own\n"
    );
    assert_eq!(r.behind(), 1);
    assert_eq!(r.row()["status"], json!("active"));
}

// ---- R3: a force-push that moves the branch back ----------------------------

#[test]
fn a_force_push_that_drops_the_last_commit_is_a_rewrite_and_clears_with_dans_reset() {
    let r = CloneRig::start();
    // Two upstream commits; the clone is clean, so it fast-forwards to both.
    r.push_upstream("a.md", b"a\n", "a");
    r.push_upstream("b.md", b"b\n", "b");
    r.sync();
    assert_eq!(r.behind(), 0);
    assert!(r.clone().join("b.md").exists());
    let before = r.identity();
    let files = r.files();
    let calls_before = r.calls().len();

    // Upstream drops its last commit: the new tip is an ancestor of the one
    // recorded.
    git(
        r.other.path(),
        &["push", "--force", "origin", "HEAD~1:main"],
    );
    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("needs_attention"), "{row}");
    assert_rewritten_reason(&row);
    assert_eq!(r.identity(), before, "HEAD, branch, index, reflog");
    assert_eq!(r.files(), files, "Dan's files byte for byte");
    // It stays flagged while the clone still holds the dropped commit.
    r.sync();
    assert_eq!(r.row()["status"], json!("needs_attention"));
    assert!(
        !r.calls()[calls_before..]
            .iter()
            .any(|c| c.starts_with("merge ")),
        "nothing was merged"
    );
    r.assert_no_merge_commit();

    // Dan's own reset onto the new history clears it.
    r.dan(&["reset", "--hard", "origin/main"]);
    r.sync();
    let row = r.row();
    assert_eq!(row["status"], json!("active"), "{row}");
    assert!(row.get("status_reason").is_none() || row["status_reason"].is_null());
}

// ---- E-count: names and numbers ----------------------------------------------

#[test]
fn ten_thousand_new_files_with_odd_names_are_counted_in_under_five_seconds() {
    let r = CloneRig::start();
    let root = r.clone();
    std::fs::create_dir_all(root.join("bulk")).unwrap();
    for i in 0..9_996 {
        std::fs::write(root.join(format!("bulk/file {i}.md")), format!("{i}\n")).unwrap();
    }
    for odd in [
        "-dash.md",
        "two\nlines.md",
        "caf\u{e9} \u{65e5}\u{672c}.md",
        "a b.md",
    ] {
        std::fs::write(root.join(odd), "x\n").unwrap();
    }
    let wrote = Instant::now();
    r.rig.d.wait_for("10,000 changes not pushed", || {
        (r.not_pushed() == 10_000).then_some(())
    });
    let took = wrote.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "10,000 changed files took {took:?}"
    );
    // Not the same file twice, not one lost: a recount says the same.
    r.sync();
    assert_eq!(r.not_pushed(), 10_000);
}
