//! PR-13 (G7, E18, F2, F3, F9, F12): `source.unregister` of a GitHub or Drive
//! source. Without `force` the daemon refuses while the source holds work the
//! remote does not have, and says what; with `force` it discards that work
//! and everything Tephra keeps for the source. A removal never makes a
//! request to GitHub or Drive.
//!
//! Real registry, RPC server and watcher; real git against a local bare
//! repository for the GitHub cases (a recording wrapper stands in for `git`);
//! the PR-3 fake GitHub and Drive; a real temp data dir.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use remote_support::{exec_sql, item_rows, save, DriveDaemon, FetchRig, Tuning, QUIET};
use serde_json::{json, Value};
use tephra_remote::testing::UploadHold;

const SOURCE_ERROR: i64 = -32011;
const INVALID_PARAMS: i64 = -32602;

// ---- GitHub: a real clone -------------------------------------------------------------

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

/// A stand-in for `git` that logs its arguments and runs the real one.
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

struct GitRig {
    rig: FetchRig,
    origin: tempfile::TempDir,
    other: tempfile::TempDir,
    tools: tempfile::TempDir,
    log: PathBuf,
}

impl GitRig {
    /// The source `g` whose `clone/` is a real clone of an origin holding
    /// `README.md`; one sync has run, so the clone is known and watched.
    fn start() -> GitRig {
        Self::start_with(false)
    }

    /// [`GitRig::start`] with the fake GitHub behind a switch, so
    /// `network_down` works.
    fn start_with(gated: bool) -> GitRig {
        let tools = tempfile::tempdir().unwrap();
        let (program, log) = recording_git(tools.path());
        let rig = FetchRig::start_capped(
            Tuning {
                git: Some(program),
                gated,
                ..Tuning::default()
            },
            |_| {},
        );
        let origin = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "--bare", "-b", "main", "."]);
        git(other.path(), &["init", "-b", "main", "."]);
        write(other.path(), "README.md", b"# Atlas\n");
        write(other.path(), "docs/Guide.md", b"# Guide\n");
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
        let g = GitRig {
            rig,
            origin,
            other,
            tools,
            log,
        };
        g.sync();
        g
    }

    fn clone(&self) -> PathBuf {
        self.rig.d.data_dir.join("remote").join("g").join("clone")
    }

    fn dir(&self) -> PathBuf {
        self.rig.d.data_dir.join("remote").join("g")
    }

    fn dan(&self, args: &[&str]) -> String {
        git(&self.clone(), args)
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

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn requests(&self) -> usize {
        self.rig.fake.requests()
    }
}

fn remove(d: &remote_support::Daemon, name: &str, force: Option<Value>) -> Value {
    let mut params = json!({ "name": name });
    if let Some(force) = force {
        params["force"] = force;
    }
    d.rpc("source.unregister", params)
}

fn listed(d: &remote_support::Daemon, name: &str) -> bool {
    d.source_row(name).is_some()
}

fn refusal(reply: &Value) -> &Value {
    assert_eq!(reply["error"]["code"], json!(SOURCE_ERROR), "{reply}");
    assert_eq!(
        reply["error"]["data"]["reason"],
        json!("unsent_work"),
        "{reply}"
    );
    &reply["error"]["data"]
}

fn assert_removed_everywhere(d: &remote_support::Daemon, name: &str) {
    assert!(!listed(d, name), "{name} is still listed");
    assert!(
        !d.data_dir.join("remote").join(name).exists(),
        "remote/{name}/ is still there"
    );
    assert!(item_rows(&d.cache_path, name).is_empty());
    let cache = tephra_cache::Cache::open_read_only(&d.cache_path).unwrap();
    for (table, column) in tephra_cache::REMOTE_SOURCE_TABLES {
        let n: i64 = cache
            .connection()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                [name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{table} still has rows of {name}");
    }
}

#[test]
fn a_clean_github_source_is_removed_with_its_directory_and_one_event() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);
    let before = g.requests();

    let reply = remove(d, "g", None);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"], Value::Null);

    assert_removed_everywhere(d, "g");
    assert_eq!(
        client.next_sources_changed(Duration::from_secs(5)),
        Some(json!([{ "kind": "removed", "name": "g" }]))
    );
    assert!(
        client.next_sources_changed(QUIET).is_none(),
        "exactly one removed event"
    );
    assert_eq!(g.requests(), before, "a removal makes no request to GitHub");
}

#[test]
fn an_edited_file_in_the_clone_refuses_and_nothing_changes() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    let before = g.requests();
    let calls_before = g.calls().len();

    // No wait for the watcher: the removal looks at the clone itself.
    let reply = remove(d, "g", None);
    let data = refusal(&reply);
    assert_eq!(data["not_pushed"], json!(1), "{reply}");
    assert_eq!(data["pending_upload"], json!(0));
    assert_eq!(data["needs_attention"], json!(0));
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(message.contains("1 not pushed"), "{message}");
    assert!(message.contains("force"), "{message}");

    assert!(listed(d, "g"));
    assert_eq!(
        std::fs::read(g.clone().join("README.md")).unwrap(),
        b"# Atlas\nmy edit\n",
        "the edit is intact"
    );
    assert!(g.clone().join(".git").is_dir());
    assert_eq!(g.requests(), before, "a refusal makes no request");
    assert!(
        client.next_sources_changed(QUIET).is_none(),
        "no event for a refusal"
    );
    // Only reading git was done by the removal.
    for call in &g.calls()[calls_before..] {
        let first = call.split_whitespace().next().unwrap_or("");
        assert!(
            !["push", "commit", "rebase", "merge", "reset", "checkout", "stash"].contains(&first),
            "git {call}"
        );
    }

    // The source keeps polling.
    let polls = d.registry.remote_polls_done();
    g.sync();
    assert!(d.registry.remote_polls_done() > polls);
    assert_eq!(
        std::fs::read(g.clone().join("README.md")).unwrap(),
        b"# Atlas\nmy edit\n"
    );
}

#[test]
fn an_unpushed_commit_alone_refuses() {
    let g = GitRig::start();
    let d = &g.rig.d;
    write(&g.clone(), "docs/New.md", b"# New\n");
    g.dan(&["add", "-A"]);
    g.dan(&["commit", "-m", "local only"]);
    let head = g.dan(&["rev-parse", "HEAD"]);

    let reply = remove(d, "g", None);
    let data = refusal(&reply);
    assert!(data["not_pushed"].as_u64().unwrap() >= 1, "{reply}");

    assert!(listed(d, "g"));
    assert_eq!(g.dan(&["rev-parse", "HEAD"]), head, "the commit is intact");
    assert!(g.clone().join("docs/New.md").is_file());
}

#[test]
fn the_refusal_counts_what_source_list_shows() {
    let g = GitRig::start();
    let d = &g.rig.d;
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    d.wait_for("1 change not pushed", || {
        (d.source_row("g")?["not_pushed"] == json!(1)).then_some(())
    });
    let reply = remove(d, "g", None);
    assert_eq!(
        refusal(&reply)["not_pushed"],
        d.source_row("g").unwrap()["not_pushed"]
    );
}

#[test]
fn work_that_reaches_the_remote_makes_the_removal_succeed_without_force() {
    let g = GitRig::start();
    let d = &g.rig.d;
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    assert!(remove(d, "g", None).get("error").is_some());

    g.dan(&["add", "-A"]);
    g.dan(&["commit", "-m", "mine"]);
    g.dan(&["push", "origin", "main"]);
    g.sync();
    d.wait_for("0 changes not pushed", || {
        (d.source_row("g")?
            .get("not_pushed")
            .is_none_or(|n| n == &json!(0)))
        .then_some(())
    });

    let reply = remove(d, "g", None);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");
}

#[test]
fn force_discards_the_edit_the_clone_and_everything_and_runs_no_writing_git() {
    let g = GitRig::start();
    let d = &g.rig.d;
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    write(&g.clone(), "docs/New.md", b"# New\n");
    g.dan(&["add", "-A"]);
    g.dan(&["commit", "-m", "local only"]);
    let before = g.requests();
    let calls_before = g.calls().len();

    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");

    assert_removed_everywhere(d, "g");
    assert_eq!(g.requests(), before, "a forced removal makes no request");
    assert!(
        g.origin.path().join("HEAD").is_file(),
        "the origin is not touched"
    );
    for call in &g.calls()[calls_before..] {
        let first = call.split_whitespace().next().unwrap_or("");
        assert!(
            !["push", "commit", "rebase", "merge", "reset", "checkout", "stash", "fetch"]
                .contains(&first),
            "git {call}"
        );
    }
}

#[test]
fn an_edit_saved_between_the_check_and_the_delete_is_refused_and_survives() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let clone = g.clone();
    // The first look finds nothing; the edit lands once the workers are
    // paused; the second look finds it.
    d.registry.set_removal_pause_hook(move || {
        write(&clone, "README.md", b"# Atlas\nsaved in the gap\n");
    });
    let reply = remove(d, "g", None);
    assert_eq!(refusal(&reply)["not_pushed"], json!(1), "{reply}");
    assert!(listed(d, "g"));
    assert_eq!(
        std::fs::read(g.clone().join("README.md")).unwrap(),
        b"# Atlas\nsaved in the gap\n"
    );
    // The workers run again: a sync still works.
    let polls = d.registry.remote_polls_done();
    g.sync();
    assert!(d.registry.remote_polls_done() > polls);
}

#[test]
fn force_is_the_json_boolean_true_and_nothing_else() {
    let g = GitRig::start();
    let d = &g.rig.d;
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    for bad in [
        json!("true"),
        json!(1),
        json!(null),
        json!([true]),
        json!({}),
    ] {
        let reply = remove(d, "g", Some(bad.clone()));
        assert_eq!(
            reply["error"]["code"],
            json!(INVALID_PARAMS),
            "{bad}: {reply}"
        );
        assert!(listed(d, "g"), "{bad}: nothing was removed");
        assert!(g.dir().is_dir());
    }
    // false is the same as absent.
    let reply = remove(d, "g", Some(json!(false)));
    refusal(&reply);
    assert!(listed(d, "g"));
    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");
}

#[test]
fn a_link_inside_the_cache_is_unlinked_not_followed() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("precious.md"), b"keep me").unwrap();
    std::os::unix::fs::symlink(outside.path(), g.dir().join("escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("precious.md"),
        g.clone().join("link.md"),
    )
    .unwrap();

    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");
    assert_eq!(
        std::fs::read(outside.path().join("precious.md")).unwrap(),
        b"keep me",
        "what the link pointed at is untouched"
    );
}

#[test]
fn a_name_that_could_escape_the_remote_root_deletes_nothing() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let victim = d.data_dir.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("a.md"), b"a").unwrap();
    for name in ["../victim", "..", ".", "g/..", "/etc", "a\\b"] {
        let reply = remove(d, name, Some(json!(true)));
        // Not a source: nothing to remove, nothing deleted.
        assert!(
            reply.get("error").is_none() || reply["error"]["code"] == json!(SOURCE_ERROR),
            "{name}: {reply}"
        );
    }
    assert!(victim.join("a.md").is_file());
    assert!(listed(d, "g"));
    assert!(g.dir().is_dir());
}

#[test]
fn a_directory_that_cannot_be_deleted_leaves_the_source_whole_and_a_retry_completes() {
    use std::os::unix::fs::PermissionsExt;
    let g = GitRig::start();
    let d = &g.rig.d;
    let locked = g.dir().join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("x"), b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A process that may write there anyway (root) cannot show the failure.
    if std::fs::write(locked.join("probe"), b"p").is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: this user can write to a read-only directory");
        return;
    }
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);

    let reply = remove(d, "g", Some(json!(true)));
    assert_eq!(reply["error"]["code"], json!(SOURCE_ERROR), "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(message.contains("permission denied"), "{message}");
    assert!(
        !message.contains(&d.data_dir.display().to_string()),
        "no path in the message: {message}"
    );
    assert!(listed(d, "g"), "the source row is not half-deleted");
    assert!(!item_rows(&d.cache_path, "g").is_empty(), "nor its rows");
    assert!(client.next_sources_changed(QUIET).is_none(), "no event");

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");
}

#[test]
fn two_concurrent_removals_remove_once_and_the_other_says_not_found() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);
    let replies: Vec<Value> = std::thread::scope(|s| {
        let a = s.spawn(|| remove(d, "g", None));
        let b = s.spawn(|| remove(d, "g", None));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    let ok = replies.iter().filter(|r| r.get("error").is_none()).count();
    // The second either lost the race (not found) or came after it (an
    // idempotent success): never two removals.
    assert!(ok >= 1, "{replies:?}");
    for r in &replies {
        if let Some(e) = r.get("error") {
            assert_eq!(e["code"], json!(SOURCE_ERROR));
            assert!(e["message"].as_str().unwrap().contains("no source named"));
        }
    }
    assert_removed_everywhere(d, "g");
    assert!(client
        .next_sources_changed(Duration::from_secs(5))
        .is_some());
    assert!(
        client.next_sources_changed(QUIET).is_none(),
        "exactly one removed event"
    );
}

#[test]
fn a_source_added_again_under_the_name_begins_empty_and_is_served_by_a_new_poller() {
    let g = GitRig::start();
    let d = &g.rig.d;
    d.ok("unit.fetch", json!({ "unit": "@g:README.md" }));
    assert!(item_rows(&d.cache_path, "g").len() > 1);
    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");

    d.add_github("g", remote_support::REPO);
    d.wait_active("g");
    let row = d.source_row("g").unwrap();
    assert_eq!(row["cache_bytes"].as_u64().unwrap_or(0), 0, "{row}");
    assert!(!item_rows(&d.cache_path, "g").is_empty(), "the full index");
    assert!(
        item_rows(&d.cache_path, "g")
            .iter()
            .all(|r| r.state != "pinned" && r.state != "cached"),
        "no old copy, no old pin"
    );
    // The new source is polled.
    let polls = d.registry.remote_polls_done();
    g.sync();
    assert!(d.registry.remote_polls_done() > polls);
}

#[test]
fn a_directory_left_by_a_cut_short_removal_is_deleted_at_the_next_start() {
    let g = GitRig::start();
    let orphan = g.rig.d.data_dir.join("remote").join("ghost");
    std::fs::create_dir_all(orphan.join("clone")).unwrap();
    std::fs::write(orphan.join("old.md"), b"old").unwrap();
    let rig = g.rig.restart(Tuning::default());
    assert!(!orphan.exists(), "the orphan is gone");
    assert!(
        rig.d.data_dir.join("remote").join("g").is_dir(),
        "a source's directory is not an orphan"
    );
    rig.d.close();
}

#[test]
fn removing_a_local_source_is_unchanged_and_force_does_nothing_extra() {
    let g = GitRig::start();
    let d = &g.rig.d;
    let vault = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("a.md"), "# A\n").unwrap();
    d.ok(
        "source.register",
        json!({ "name": "notes", "root": vault.path().display().to_string() }),
    );
    assert!(listed(d, "notes"));
    let reply = remove(d, "notes", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(!listed(d, "notes"));
    assert!(
        vault.path().join("a.md").is_file(),
        "the folder is not touched"
    );
    assert!(listed(d, "g"), "another source is not touched");
    // An unknown name is still an idempotent success on the wire (the CLI
    // says "not found").
    let reply = remove(d, "nope", None);
    assert!(reply.get("error").is_none(), "{reply}");
}

// ---- Drive ----------------------------------------------------------------------------

fn drive_requests(dd: &DriveDaemon) -> usize {
    dd.drive.request_log().all().len()
}

// ---- the unsent-work rule, state by state ---------------------------------------------
//
// One row of an item in each of the eight states, for each kind: which block a
// removal without `force`. The rule is the cache's count, so a row set in the
// cache is the input; the real flows that make these states (an edit saved, a
// conflict held, an upload in backoff) are the tests above.

/// Drive: only an item waiting to be sent (`pending_upload`) or holding Dan's
/// version (`needs_attention`) blocks. Every other state, `not_pushed` (a
/// GitHub word) included, is a file Drive has or has nothing of.
const DRIVE_BLOCKS: [(&str, Option<&str>); 8] = [
    ("remote", None),
    ("downloading", None),
    ("cached", None),
    ("pinned", None),
    ("pending_upload", Some("pending_upload")),
    ("not_pushed", None),
    ("needs_attention", Some("needs_attention")),
    ("missing", None),
];

#[test]
fn the_state_tables_name_every_item_state_in_the_precedence_list() {
    // A state added to the list without a line here fails this, so the
    // rule is decided for it.
    let drive: Vec<&str> = DRIVE_BLOCKS.iter().map(|(s, _)| *s).collect();
    assert_eq!(drive, tephra_cache::REMOTE_ITEM_STATES);
    let mut github: Vec<&str> = GITHUB_CASES.iter().map(|c| c.state).collect();
    github.dedup();
    assert_eq!(github, tephra_cache::REMOTE_ITEM_STATES);
}

#[test]
fn a_drive_removal_is_refused_for_exactly_the_states_that_hold_unsent_work() {
    for (state, blocks) in DRIVE_BLOCKS {
        let dd = DriveDaemon::start_edits();
        dd.fetch_file("Plan.md");
        exec_sql(
            &dd.d.cache_path,
            &format!(
                "UPDATE remote_items SET state = '{state}' WHERE source = 'gdrive' AND path = 'Plan.md'"
            ),
        );
        let before = drive_requests(&dd);

        let reply = remove(&dd.d, "gdrive", None);
        match blocks {
            Some(field) => {
                let data = refusal(&reply);
                for kind in ["not_pushed", "pending_upload", "needs_attention"] {
                    assert_eq!(
                        data[kind],
                        json!(u64::from(kind == field)),
                        "{state}: {kind}: {reply}"
                    );
                }
                assert!(listed(&dd.d, "gdrive"), "{state}: still listed");
                assert!(dd.root().is_dir(), "{state}: the directory stays");
                assert_eq!(dd.row("Plan.md").state, state, "{state}: the row stays");
                let forced = remove(&dd.d, "gdrive", Some(json!(true)));
                assert!(forced.get("error").is_none(), "{state}: {forced}");
            }
            None => assert!(reply.get("error").is_none(), "{state}: {reply}"),
        }
        assert_removed_everywhere(&dd.d, "gdrive");
        assert_eq!(drive_requests(&dd), before, "{state}: no request");
        dd.d.close();
    }
}

/// One case of the GitHub table.
struct GithubCase {
    state: &'static str,
    /// Dan has edited a file in the clone (the real thing the state names).
    edit: bool,
    blocks: bool,
}

/// GitHub: the one thing that blocks is the clone's own count of changes not
/// pushed (what `source list` shows). The state of an item row is not the
/// rule: a row left in `pending_upload`, `needs_attention` or `not_pushed` on
/// a clone with nothing to push does not block.
const GITHUB_CASES: [GithubCase; 9] = [
    GithubCase {
        state: "remote",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "downloading",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "cached",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "pinned",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "pending_upload",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "not_pushed",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "not_pushed",
        edit: true,
        blocks: true,
    },
    GithubCase {
        state: "needs_attention",
        edit: false,
        blocks: false,
    },
    GithubCase {
        state: "missing",
        edit: false,
        blocks: false,
    },
];

#[test]
fn a_github_removal_is_refused_only_for_changes_not_pushed() {
    for case in &GITHUB_CASES {
        let g = GitRig::start();
        let d = &g.rig.d;
        let state = case.state;
        exec_sql(
            &d.cache_path,
            &format!(
                "UPDATE remote_items SET state = '{state}' WHERE source = 'g' AND path = 'README.md'"
            ),
        );
        if case.edit {
            write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
        }
        let before = g.requests();
        let label = format!("{state} (edit: {})", case.edit);

        let reply = remove(d, "g", None);
        if case.blocks {
            let data = refusal(&reply);
            assert_eq!(data["not_pushed"], json!(1), "{label}: {reply}");
            assert_eq!(data["pending_upload"], json!(0), "{label}");
            assert_eq!(data["needs_attention"], json!(0), "{label}");
            assert!(listed(d, "g"), "{label}: still listed");
            assert!(g.dir().is_dir(), "{label}: the directory stays");
            let forced = remove(d, "g", Some(json!(true)));
            assert!(forced.get("error").is_none(), "{label}: {forced}");
        } else {
            assert!(reply.get("error").is_none(), "{label}: {reply}");
        }
        assert_removed_everywhere(d, "g");
        assert_eq!(g.requests(), before, "{label}: no request");
    }
}

#[test]
fn a_clean_drive_source_with_cached_and_pinned_files_is_removed() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.d.ok(
        "unit.pin",
        json!({ "unit": "@gdrive:Other.md", "confirm": true }),
    );
    dd.d.wait_for("the pin to download", || {
        (dd.row("Other.md").state == "pinned").then_some(())
    });
    assert!(file.is_file());
    let before = drive_requests(&dd);
    let mut client = dd.d.connect();
    client.drain_graph_changed(QUIET);

    let reply = remove(&dd.d, "gdrive", None);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(&dd.d, "gdrive");
    assert_eq!(
        drive_requests(&dd),
        before,
        "a removal makes no request to Drive"
    );
    assert_eq!(
        client.next_sources_changed(Duration::from_secs(5)),
        Some(json!([{ "kind": "removed", "name": "gdrive" }]))
    );
    // The provider's credential survives.
    use tephra_remote::credentials::{CredentialStore, Provider};
    assert!(
        dd.store.get(Provider::Google).unwrap().is_some(),
        "the Google credential survives a removal"
    );
    dd.d.close();
}

#[test]
fn a_pending_upload_refuses_and_the_buffer_is_untouched() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"edited\n");
    dd.wait_state("Plan.md", "pending_upload");
    dd.d.wait_for("the upload to reach Drive", || {
        (dd.drive.uploads_held() == 1).then_some(())
    });
    let queue = dd.queue();
    let before = drive_requests(&dd);

    let reply = remove(&dd.d, "gdrive", None);
    let data = refusal(&reply);
    assert_eq!(data["pending_upload"], json!(1), "{reply}");
    assert_eq!(data["needs_attention"], json!(0));
    assert_eq!(data["not_pushed"], json!(0));
    assert_eq!(dd.queue(), queue, "the upload buffer is untouched");
    assert_eq!(std::fs::read(&file).unwrap(), b"edited\n");
    assert_eq!(drive_requests(&dd), before, "a refusal makes no request");

    // The upload in flight finishes: the work reached Drive, the removal
    // goes through without force.
    dd.drive.release_uploads();
    dd.wait_state("Plan.md", "cached");
    let reply = remove(&dd.d, "gdrive", None);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(&dd.d, "gdrive");
    dd.d.close();
}

#[test]
fn an_edit_saved_a_moment_ago_is_found_by_the_removal_itself() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    // Held at Drive so the watcher's scan and the upload are not the
    // subject: the save and the removal come back to back.
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"edited just now\n");
    let reply = remove(&dd.d, "gdrive", None);
    let data = refusal(&reply);
    assert_eq!(data["pending_upload"], json!(1), "{reply}");
    assert_eq!(std::fs::read(&file).unwrap(), b"edited just now\n");
    dd.drive.release_uploads();
    dd.d.close();
}

#[test]
fn a_conflict_that_holds_dans_version_refuses() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    // Drive moves on under Dan's edit.
    dd.drive.update_file("p1", b"# Plan\nsomeone else\n");
    save(&file, b"# Plan\nmine\n");
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert!(
        held.reason
            .as_deref()
            .unwrap_or("")
            .contains("changed on Drive"),
        "{held:?}"
    );

    let reply = remove(&dd.d, "gdrive", None);
    let data = refusal(&reply);
    assert_eq!(data["needs_attention"], json!(1), "{reply}");
    assert_eq!(std::fs::read(&file).unwrap(), b"# Plan\nmine\n");
    assert_eq!(dd.row("Plan.md").state, "needs_attention");

    let reply = remove(&dd.d, "gdrive", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(&dd.d, "gdrive");
    dd.d.close();
}

#[test]
fn an_upload_waiting_in_backoff_refuses_and_force_drops_it_with_no_request() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1, 503);
    save(&file, b"edited\n");
    let row = dd.wait_attempts("Plan.md", 1);
    let queue = dd.queue();
    assert_eq!(queue.len(), 1);
    let before = drive_requests(&dd);

    let reply = remove(&dd.d, "gdrive", None);
    assert_eq!(refusal(&reply)["pending_upload"], json!(1), "{reply}");
    assert_eq!(dd.queue(), queue, "the row and its backoff are untouched");
    assert_eq!(dd.queue()[0].id, row.id);

    let reply = remove(&dd.d, "gdrive", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(&dd.d, "gdrive");
    assert_eq!(drive_requests(&dd), before, "no request, forced or not");
    assert!(
        dd.writes().iter().all(|w| w.contains("PATCH")),
        "{:?}",
        dd.writes()
    );
    assert_eq!(dd.drive.upload_count("p1"), 0, "nothing was uploaded");
    // The clock moving past the backoff sends nothing for a removed source.
    dd.advance(Duration::from_secs(600));
    std::thread::sleep(QUIET);
    assert_eq!(drive_requests(&dd), before);
    dd.d.close();
}

#[test]
fn a_drive_source_added_again_begins_with_no_queue_and_no_old_files() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    save(&file, b"edited\n");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    dd.wait_state("Plan.md", "pending_upload");
    let reply = remove(&dd.d, "gdrive", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    dd.drive.release_uploads();
    assert_removed_everywhere(&dd.d, "gdrive");

    dd.add_active("gdrive", "fld");
    assert!(dd.queue().is_empty());
    assert_eq!(dd.row("Plan.md").state, "remote");
    assert!(!dd.root().join("Plan.md").exists(), "no old file");
    dd.d.close();
}

#[test]
fn a_download_in_flight_is_cancelled_and_leaves_nothing_under_the_removed_directory() {
    let dd = DriveDaemon::start_edits();
    dd.drive.hold_content_after(0);
    let d = &dd.d;
    let fetch = std::thread::scope(|s| {
        let fetching = s.spawn(|| d.rpc("unit.fetch", json!({ "unit": "@gdrive:Plan.md" })));
        d.wait_for("the download to begin", || {
            (dd.row("Plan.md").state == "downloading").then_some(())
        });
        let reply = remove(d, "gdrive", Some(json!(true)));
        assert!(reply.get("error").is_none(), "{reply}");
        dd.drive.release_content();
        fetching.join().unwrap()
    });
    assert!(
        fetch.get("error").is_some(),
        "the fetch did not finish: {fetch}"
    );
    assert_removed_everywhere(d, "gdrive");
    std::thread::sleep(QUIET);
    assert!(
        !d.data_dir.join("remote").join("gdrive").exists(),
        "no temp file or directory came back"
    );
    dd.d.close();
}

// ---- two Drive sources on one account -------------------------------------------------

/// `gdrive` (the upload tests' folder `fld`) and `gdrive2` (a folder `fld2`
/// with `More.md`), both active, one Google account for both.
fn two_drive_sources() -> DriveDaemon {
    let dd = DriveDaemon::start(|drive| {
        remote_support::edits_drive(drive);
        drive.add_folder("fld2", "More", None);
        drive.add_file("m1", "More.md", "text/markdown", Some("fld2"), b"# More\n");
    });
    dd.add_active("gdrive", "fld");
    dd.add_active("gdrive2", "fld2");
    dd
}

fn removed(name: &str) -> Option<Value> {
    Some(json!([{ "kind": "removed", "name": name }]))
}

#[test]
fn a_removal_over_http_and_one_over_websocket_each_send_one_removed_event() {
    let dd = two_drive_sources();
    let d = &dd.d;
    let mut watcher = d.connect();
    let mut asker = d.connect();
    watcher.drain_graph_changed(QUIET);
    asker.drain_graph_changed(QUIET);

    // Over HTTP: every connected client is told, once.
    let reply = remove(d, "gdrive", None);
    assert!(reply.get("error").is_none(), "{reply}");
    for client in [&mut watcher, &mut asker] {
        assert_eq!(
            client.next_sources_changed(Duration::from_secs(5)),
            removed("gdrive")
        );
        assert!(client.next_sources_changed(QUIET).is_none(), "only one");
    }

    // Over the WebSocket, a refusal first: the same error as over HTTP and
    // no event.
    exec_sql(
        &d.cache_path,
        "UPDATE remote_items SET state = 'pending_upload' WHERE source = 'gdrive2' AND path = 'More.md'",
    );
    let refused = asker.reply("source.unregister", json!({ "name": "gdrive2" }));
    assert_eq!(refusal(&refused)["pending_upload"], json!(1), "{refused}");
    assert_eq!(refused, {
        let mut over_http = remove(d, "gdrive2", None);
        over_http["id"] = refused["id"].clone();
        over_http
    });
    assert!(listed(d, "gdrive2"));
    for client in [&mut watcher, &mut asker] {
        assert!(client.next_sources_changed(QUIET).is_none(), "no event");
    }

    // The work is gone; the same call over the WebSocket removes it.
    exec_sql(
        &d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'gdrive2' AND path = 'More.md'",
    );
    let reply = asker.reply("source.unregister", json!({ "name": "gdrive2" }));
    assert!(reply.get("error").is_none(), "{reply}");
    for client in [&mut watcher, &mut asker] {
        assert_eq!(
            client.next_sources_changed(Duration::from_secs(5)),
            removed("gdrive2")
        );
        assert!(client.next_sources_changed(QUIET).is_none(), "only one");
    }
    assert_removed_everywhere(d, "gdrive2");
    dd.d.close();
}

// ---- a source that is offline or needs a new sign-in ------------------------------------

/// What is wrong with the provider.
#[derive(Clone, Copy, Debug)]
enum Trouble {
    Offline,
    AuthRequired,
}

impl Trouble {
    fn status(self) -> &'static str {
        match self {
            Trouble::Offline => "offline",
            Trouble::AuthRequired => "auth_required",
        }
    }
}

/// A GitHub source `g` whose next poll fails the way `trouble` says, polled
/// until `source list` shows the status.
fn github_in_trouble(trouble: Trouble) -> GitRig {
    let g = GitRig::start_with(true);
    match trouble {
        Trouble::Offline => g.rig.fake.network_down(),
        Trouble::AuthRequired => g.rig.fake.gh.revoke_token(),
    }
    g.sync();
    g.rig.d.wait_for(trouble.status(), || {
        (g.rig.d.source_row("g")?["status"] == json!(trouble.status())).then_some(())
    });
    g
}

#[test]
fn an_offline_or_auth_required_github_source_with_nothing_unsent_is_removed() {
    for trouble in [Trouble::Offline, Trouble::AuthRequired] {
        let g = github_in_trouble(trouble);
        let d = &g.rig.d;
        let mut client = d.connect();
        client.drain_graph_changed(QUIET);
        let before = g.requests();

        let reply = remove(d, "g", None);
        assert!(reply.get("error").is_none(), "{trouble:?}: {reply}");
        assert_removed_everywhere(d, "g");
        assert_eq!(g.requests(), before, "{trouble:?}: no request");
        assert_eq!(
            client.next_sources_changed(Duration::from_secs(5)),
            removed("g"),
            "{trouble:?}"
        );
    }
}

#[test]
fn an_offline_or_auth_required_github_source_with_an_edit_is_refused_then_forced() {
    for trouble in [Trouble::Offline, Trouble::AuthRequired] {
        let g = github_in_trouble(trouble);
        let d = &g.rig.d;
        write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
        let before = g.requests();

        let reply = remove(d, "g", None);
        assert_eq!(
            refusal(&reply)["not_pushed"],
            json!(1),
            "{trouble:?}: {reply}"
        );
        assert_eq!(
            std::fs::read(g.clone().join("README.md")).unwrap(),
            b"# Atlas\nmy edit\n",
            "{trouble:?}: the edit is intact"
        );
        assert_eq!(
            d.source_row("g").unwrap()["status"],
            json!(trouble.status()),
            "{trouble:?}: the status is as it was"
        );

        let reply = remove(d, "g", Some(json!(true)));
        assert!(reply.get("error").is_none(), "{trouble:?}: {reply}");
        assert_removed_everywhere(d, "g");
        assert_eq!(g.requests(), before, "{trouble:?}: no request");
    }
}

/// A Drive source `gdrive` that has fetched `Plan.md`, with Drive failing the
/// way `trouble` says at the next poll. `dirty`: `Plan.md` has an edit that
/// Drive holds at the door, so it is `pending_upload` and queued.
fn drive_in_trouble(trouble: Trouble, dirty: bool) -> DriveDaemon {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    if dirty {
        dd.drive.hold_uploads(UploadHold::BeforeApply);
        save(&file, b"edited\n");
        dd.wait_state("Plan.md", "pending_upload");
        dd.d.wait_for("the upload to reach Drive", || {
            (dd.drive.uploads_held() == 1).then_some(())
        });
    }
    match trouble {
        Trouble::Offline => dd.drive.fail_next(1, 503),
        Trouble::AuthRequired => dd.drive.revoke_token(),
    }
    dd.poll();
    dd.d.wait_for(trouble.status(), || {
        (dd.d.source_row("gdrive")?["status"] == json!(trouble.status())).then_some(())
    });
    dd
}

#[test]
fn an_offline_or_auth_required_drive_source_with_nothing_unsent_is_removed() {
    use tephra_remote::credentials::{CredentialStore, Provider};
    for trouble in [Trouble::Offline, Trouble::AuthRequired] {
        let dd = drive_in_trouble(trouble, false);
        let before = drive_requests(&dd);

        let reply = remove(&dd.d, "gdrive", None);
        assert!(reply.get("error").is_none(), "{trouble:?}: {reply}");
        assert_removed_everywhere(&dd.d, "gdrive");
        assert_eq!(drive_requests(&dd), before, "{trouble:?}: no request");
        assert!(
            dd.store.get(Provider::Google).unwrap().is_some(),
            "{trouble:?}: the credential stays"
        );
        dd.d.close();
    }
}

#[test]
fn an_offline_or_auth_required_drive_source_with_a_pending_upload_is_refused_then_forced() {
    for trouble in [Trouble::Offline, Trouble::AuthRequired] {
        let dd = drive_in_trouble(trouble, true);
        let queue = dd.queue();
        let before = drive_requests(&dd);

        let reply = remove(&dd.d, "gdrive", None);
        assert_eq!(
            refusal(&reply)["pending_upload"],
            json!(1),
            "{trouble:?}: {reply}"
        );
        assert_eq!(dd.queue(), queue, "{trouble:?}: the buffer is untouched");
        assert_eq!(
            dd.d.source_row("gdrive").unwrap()["status"],
            json!(trouble.status()),
            "{trouble:?}: the status is as it was"
        );

        let reply = remove(&dd.d, "gdrive", Some(json!(true)));
        assert!(reply.get("error").is_none(), "{trouble:?}: {reply}");
        assert_removed_everywhere(&dd.d, "gdrive");
        assert_eq!(drive_requests(&dd), before, "{trouble:?}: no request");
        dd.drive.release_uploads();
        dd.d.close();
    }
}

// ---- what the rest of the vault sees after a removal -------------------------------------

fn edges_of(d: &remote_support::Daemon, method: &str, id: &str) -> Vec<Value> {
    d.ok(method, json!({ "id": id }))
        .as_array()
        .unwrap()
        .clone()
}

fn fix_log_rows(d: &remote_support::Daemon, source: &str) -> i64 {
    tephra_cache::Cache::open_read_only(&d.cache_path)
        .unwrap()
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM fix_log WHERE old_source = ?1",
            [source],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn after_a_removal_links_to_the_source_are_unresolved_its_backlinks_are_gone_and_the_fix_log_stays()
{
    let g = GitRig::start();
    let d = &g.rig.d;
    let vault = fixtures::empty_source();
    vault.write("note.md", "# Note\n\nThe project: [[@g:README.md]]\n");
    d.ok(
        "source.register",
        json!({ "name": "vault", "root": vault.root().to_string_lossy() }),
    );
    // Both ways before: the note links to the repo file, and the repo's
    // README (read once at the first index) links to the note.
    let resolved = d.ok_when(
        "unit.resolve_link",
        json!({ "link": "[[@g:README.md]]" }),
        |_| true,
    );
    assert_eq!(resolved["unit"]["id"], json!("@g:README.md"), "{resolved}");
    let into_repo = d.ok_when(
        "graph.get_backlinks",
        json!({ "id": "@g:README.md" }),
        |r| r.as_array().is_some_and(|a| !a.is_empty()),
    );
    assert!(
        into_repo
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == json!("@vault:note.md")),
        "{into_repo:?}"
    );
    let from_repo = d.ok_when(
        "graph.get_backlinks",
        json!({ "id": "@vault:note.md" }),
        |r| r.as_array().is_some_and(|a| !a.is_empty()),
    );
    assert_eq!(from_repo[0]["from"], json!("@g:README.md"), "{from_repo}");
    // A line of the fix history that names the source.
    exec_sql(
        &d.cache_path,
        "INSERT INTO fix_log (created_at, kind, status, old_source, old_path) \
         VALUES (1, 'rename', 'applied', 'g', 'Old.md')",
    );
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);

    let reply = remove(d, "g", None);
    assert!(reply.get("error").is_none(), "{reply}");
    // The one push says so, and it comes after the graph is rebuilt.
    assert_eq!(
        client.next_sources_changed(Duration::from_secs(5)),
        removed("g")
    );

    let unresolved = d.rpc("unit.resolve_link", json!({ "link": "[[@g:README.md]]" }));
    assert!(
        unresolved.get("error").is_some(),
        "the link resolves to nothing, like a deleted local file: {unresolved}"
    );
    let forward = edges_of(d, "graph.get_forward_links", "@vault:note.md");
    assert_eq!(
        forward.len(),
        1,
        "the note's link is still an edge: {forward:?}"
    );
    assert_eq!(forward[0]["to"], json!("dangling"), "{forward:?}");
    assert!(
        edges_of(d, "graph.get_backlinks", "@vault:note.md").is_empty(),
        "backlinks from the removed source are gone"
    );
    let into_gone = d.rpc("graph.get_backlinks", json!({ "id": "@g:README.md" }));
    let left = into_gone["result"].as_array().cloned().unwrap_or_default();
    assert!(
        left.iter()
            .all(|e| !e["from"].as_str().unwrap_or("").starts_with("@g:")),
        "nothing of the removed source is left in the graph: {into_gone}"
    );
    assert_eq!(fix_log_rows(d, "g"), 1, "the fix history stays");
}

// ---- the other sources of the provider --------------------------------------------------

#[test]
fn a_second_drive_source_on_the_same_account_keeps_syncing_after_the_first_is_removed() {
    use tephra_remote::credentials::{CredentialStore, Provider};
    let dd = two_drive_sources();
    let d = &dd.d;
    let reply = remove(d, "gdrive", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "gdrive");

    // The account is still signed in, and the other source is polled with
    // it: a file added in its folder arrives.
    assert!(
        dd.store.get(Provider::Google).unwrap().is_some(),
        "the credential survives"
    );
    dd.drive.add_file(
        "m2",
        "Later.md",
        "text/markdown",
        Some("fld2"),
        b"# Later\n",
    );
    dd.poll();
    let row = d.source_row("gdrive2").unwrap();
    assert_eq!(row["status"], json!("active"), "{row}");
    assert!(
        dd.paths("gdrive2").contains(&"Later.md".to_string()),
        "{:?}",
        dd.paths("gdrive2")
    );
    // And it still fetches (a token is still read from the store).
    let fetched = d.ok("unit.fetch", json!({ "unit": "@gdrive2:Later.md" }));
    assert_eq!(
        std::fs::read(fetched["path"].as_str().unwrap()).unwrap(),
        b"# Later\n"
    );
    dd.d.close();
}

#[test]
fn a_second_github_source_keeps_polling_after_the_first_is_removed() {
    let g = GitRig::start();
    let d = &g.rig.d;
    d.add_github("g2", remote_support::REPO);
    d.wait_active("g2");
    let reply = remove(d, "g", Some(json!(true)));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_removed_everywhere(d, "g");

    let polls = g.rig.fake.polls().len();
    g.sync();
    assert!(
        g.rig.fake.polls().len() > polls,
        "GitHub was polled again, with the token the removal left in the store"
    );
    assert_eq!(d.source_row("g2").unwrap()["status"], json!("active"));
    assert!(
        !item_rows(&d.cache_path, "g2").is_empty(),
        "its index stays"
    );
}

// ---- F9: no credential in anything a removal says -----------------------------------------

/// Everything the daemon logs, at TRACE, from any thread of this test binary.
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> LogCapture {
        self.clone()
    }
}

fn log_capture() -> &'static LogCapture {
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other subscriber in this test binary");
        capture
    })
}

fn logged() -> String {
    String::from_utf8_lossy(&log_capture().0.lock().unwrap()).into_owned()
}

#[test]
fn no_credential_is_in_a_refusal_an_answer_an_error_a_push_or_a_log() {
    use remote_support::{GOOGLE_TOKEN, TOKEN};
    log_capture();
    tracing::info!("the capture is live");
    let mut said: Vec<String> = Vec::new();

    // GitHub: a refusal, an error, the list, a forced removal.
    let g = GitRig::start();
    let d = &g.rig.d;
    let mut client = d.connect();
    client.drain_graph_changed(QUIET);
    write(&g.clone(), "README.md", b"# Atlas\nmy edit\n");
    said.push(d.rpc("source.list", json!({})).to_string());
    said.push(remove(d, "g", None).to_string());
    said.push(remove(d, "g", Some(json!("true"))).to_string());
    said.push(remove(d, "../g", Some(json!(true))).to_string());
    said.push(
        client
            .reply("source.unregister", json!({ "name": "g" }))
            .to_string(),
    );
    said.push(remove(d, "g", Some(json!(true))).to_string());
    said.extend(
        client
            .next_sources_changed(Duration::from_secs(5))
            .map(|v| v.to_string()),
    );
    said.push(d.rpc("source.list", json!({})).to_string());
    said.extend(g.calls());

    // Drive: the same.
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.hold_uploads(UploadHold::BeforeApply);
    save(&file, b"edited\n");
    dd.wait_state("Plan.md", "pending_upload");
    said.push(dd.d.rpc("source.list", json!({})).to_string());
    said.push(remove(&dd.d, "gdrive", None).to_string());
    said.push(remove(&dd.d, "gdrive", Some(json!(true))).to_string());
    said.push(dd.d.rpc("source.list", json!({})).to_string());

    assert!(
        said.iter().any(|s| s.contains("unsent_work")),
        "the refusals are among what was checked"
    );
    let log = logged();
    assert!(log.contains("the capture is live"));
    dd.drive.release_uploads();
    for text in said.iter().chain(std::iter::once(&log)) {
        for secret in [TOKEN, GOOGLE_TOKEN, "Bearer"] {
            assert!(!text.contains(secret), "{secret} appears in: {text}");
        }
    }
    dd.d.close();
}
