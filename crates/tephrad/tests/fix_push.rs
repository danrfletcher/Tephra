//! PR-7: the `graph:changed` push carries what Tephra's own link fix did
//! (G17, G25), checked through the real path: a real `SourceRegistry`
//! with real watchers over temp sources, the real `FixEngine` writing
//! real files, the real `SharedGraph` and the real forwarder task
//! (`tephrad::forward::run`), and WebSocket clients on a real loopback
//! port, as in `rpc_server_ws.rs`. No mocked graph; the only stand-in is
//! `ChangingFs`, which edits a file under the writer's hands for the
//! "file changed during every attempt" case.
//!
//! No fixed sleeps wait for the daemon: every wait is bounded and ends
//! as soon as what it waits for has happened. The only fixed waits are
//! the quiet windows of the "no second push" checks, which have to let a
//! watcher debounce pass (`fix_world::QUIET`).

mod fix_world;
mod fixtures;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::DaemonEvent;
use tephrad::rename_detect::RenameFixer;

use fix_world::{fixes_of, real_writer, with_fixes, writer_over, Daemon, GP1_FILES, QUIET, WAIT};

const GP5_ENTRY: fn() -> Value = || {
    json!({
        "kind": "rename",
        "source": "notes",
        "old": "Training Plan.md",
        "new": "Plan.md",
        "files_changed": 3,
        "status": "applied",
        "deferred": 0,
    })
};

fn rename_and_fix(d: &Daemon, old: &str, new: &str) {
    d.mv_and_rescan(old, new);
    d.engine.fix_rename("notes", old, new).expect("fix_rename");
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

// ---------------------------------------------------------------
// G17: the payload
// ---------------------------------------------------------------

#[test]
fn gp5_a_rename_seen_by_the_watcher_sends_one_push_with_the_fix_and_backlinks_are_ready() {
    // GP2: a plain rename on disk; the daemon does the rest.
    let (d, mut client, push) =
        Daemon::start_and_see_rename_fixed(&GP1_FILES, "Training Plan.md", "Plan.md");
    assert_eq!(fixes_of(&push), &vec![GP5_ENTRY()], "{push}");
    assert_eq!(push["params"]["source"], json!("notes"));

    // The order the client relies on: the log entry and the refreshed
    // graph are there on receipt, with no wait.
    let entries = d.entries();
    let renames: Vec<_> = entries.iter().filter(|e| e.kind == "rename").collect();
    assert_eq!(renames.len(), 1, "{entries:?}");
    assert_eq!(renames[0].files_changed, 3);
    assert_eq!(renames[0].status, "applied");
    let backlinks = client.request("graph.get_backlinks", json!({ "id": "@notes:Plan.md" }));
    assert_eq!(
        backlinks.as_array().expect("a list").len(),
        3,
        "{backlinks}"
    );
    assert_eq!(d.backlinks("@notes:Plan.md"), 3);

    // The links on disk, written once.
    assert!(d.text("Weekly Review.md").contains("[[Plan]]"));
    assert!(d.text("Goals.md").contains("[[Plan|plan]]"));
    assert!(d.text("Ideas/Rename Notes.md").contains("![[Plan]]"));

    // One push for the batch: nothing with fixes follows, and no second
    // log entry.
    let later = client.drain(QUIET);
    assert!(with_fixes(&later).is_empty(), "a second push: {later:?}");
    assert_eq!(
        d.entries().iter().filter(|e| e.kind == "rename").count(),
        1,
        "{:?}",
        d.entries()
    );
    d.close();
}

#[test]
fn the_payload_keeps_source_and_paths_and_adds_exactly_the_fixes_list() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    assert_eq!(push["method"], json!("graph:changed"));
    assert!(push.get("id").is_none());
    let params = push["params"].as_object().unwrap();
    let mut keys: Vec<&str> = params.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["fixes", "paths", "source"]);
    // `source` and `paths` keep their names and types.
    assert_eq!(params["source"], json!("notes"));
    let mut paths: Vec<String> = params["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().expect("a path string").to_string())
        .collect();
    paths.sort();
    let root = d.notes_root();
    let mut want: Vec<String> = ["Goals.md", "Ideas/Rename Notes.md", "Weekly Review.md"]
        .iter()
        .map(|p| root.join(p).display().to_string())
        .collect();
    want.sort();
    assert_eq!(paths, want, "`paths` lists the rewritten files");
    // Data only (F13), and no `new_source` for a move inside one source:
    // the entry is exactly these seven keys.
    assert_eq!(params["fixes"], json!([GP5_ENTRY()]));
    d.close();
}

#[test]
fn a_push_on_receipt_has_the_log_entry_and_the_new_links_already() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    // Before the fix the graph still holds the links to the old name.
    assert_eq!(d.backlinks("@notes:Plan.md"), 0);
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    let (push, _) = client.next_fix_push(WAIT);
    assert!(push.is_some());
    // No wait between the push and these reads.
    assert_eq!(d.entries().iter().filter(|e| e.kind == "rename").count(), 1);
    assert_eq!(d.backlinks("@notes:Plan.md"), 3);
    d.close();
}

#[test]
fn several_renames_fixed_in_one_batch_make_one_push_with_one_entry_each() {
    let mut files: Vec<(&str, &str)> = GP1_FILES.to_vec();
    files.push(("Other Note.md", "# Other\n"));
    files.push(("Hub.md", "# Hub\n\n[[Other Note]] and [[Training Plan]].\n"));
    let d = Daemon::start(&files, &[], real_writer(), false);
    let mut client = d.connect();
    d.mv_and_rescan("Training Plan.md", "Plan.md");
    d.mv_and_rescan("Other Note.md", "Other.md");

    // What the rename detector's dispatch does for one batch.
    d.engine.start_batch();
    d.engine
        .fix_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    d.engine
        .fix_rename("notes", "Other Note.md", "Other.md")
        .unwrap();
    d.engine.finish_batch();

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 2, "{push}");
    assert_eq!(fixes[0]["old"], json!("Training Plan.md"));
    assert_eq!(fixes[0]["files_changed"], json!(4)); // GP1's three and Hub
    assert_eq!(fixes[1]["old"], json!("Other Note.md"));
    assert_eq!(fixes[1]["new"], json!("Other.md"));
    assert_eq!(fixes[1]["files_changed"], json!(1));
    // `Hub.md` was rewritten for both renames, and is listed once.
    let paths = push["params"]["paths"].as_array().unwrap();
    let hub = d.notes_root().join("Hub.md").display().to_string();
    assert_eq!(paths.iter().filter(|p| **p == json!(hub)).count(), 1);
    assert_eq!(paths.len(), 4);
    assert_eq!(
        d.text("Hub.md"),
        "# Hub\n\n[[Other]] and [[Plan]].\n",
        "both renames rewritten in the one file"
    );

    let later = client.drain(QUIET);
    assert!(with_fixes(&later).is_empty(), "a second push: {later:?}");
    d.close();
}

#[test]
fn a_folder_rename_is_one_entry_with_the_folders_old_and_new_path() {
    let d = Daemon::start(
        &[
            ("Plans/Training Plan.md", "# Training Plan\n"),
            ("Plans/Goals.md", "# Goals\n"),
            (
                "Weekly Review.md",
                "[[Plans/Training Plan]] and [[Plans/Goals]]\n",
            ),
        ],
        &[],
        real_writer(),
        false,
    );
    let mut client = d.connect();
    d.mv_and_rescan("Plans", "Archive");
    d.engine
        .fix_folder_rename(
            "notes",
            "Plans",
            "Archive",
            &[
                (
                    "Plans/Training Plan.md".to_string(),
                    "Archive/Training Plan.md".to_string(),
                ),
                ("Plans/Goals.md".to_string(), "Archive/Goals.md".to_string()),
            ],
        )
        .unwrap();
    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], json!("rename"));
    assert_eq!(fixes[0]["old"], json!("Plans"));
    assert_eq!(fixes[0]["new"], json!("Archive"));
    assert_eq!(fixes[0]["files_changed"], json!(1));
    assert_eq!(
        d.text("Weekly Review.md"),
        "[[Archive/Training Plan]] and [[Archive/Goals]]\n"
    );
    d.close();
}

#[test]
fn a_move_into_another_source_names_new_source_and_only_then() {
    let d = Daemon::start(
        &GP1_FILES,
        &[("Elsewhere.md", "# Elsewhere\n")],
        real_writer(),
        false,
    );
    let mut client = d.connect();
    // The file now lives in `other`; the engine is told the move.
    fs::rename(
        d.notes.root().join("Training Plan.md"),
        d.other.root().join("Training Plan.md"),
    )
    .unwrap();
    d.registry.rescan("notes").unwrap();
    d.registry.rescan("other").unwrap();
    let entry = d
        .engine
        .begin_fix_announced(&tephrad::fixes::FixSpec {
            old_source: "notes".into(),
            old_path: "Training Plan.md".into(),
            new_source: "other".into(),
            new_path: "Training Plan.md".into(),
            files: vec![("notes".into(), "Weekly Review.md".into())],
        })
        .expect("cross-source fix");
    d.engine.announce(entry.1);

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fix = &fixes_of(&push)[0];
    assert_eq!(fix["source"], json!("notes"));
    assert_eq!(fix["new_source"], json!("other"), "{fix}");
    d.close();
}

// ---------------------------------------------------------------
// G25: status and deferred
// ---------------------------------------------------------------

#[test]
fn a_read_only_linking_file_makes_the_fix_partial_with_deferred_one() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    set_mode(&d.notes.root().join("Goals.md"), 0o444);
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fix = &fixes_of(&push)[0];
    assert_eq!(fix["status"], json!("partial"), "{fix}");
    assert_eq!(fix["deferred"], json!(1));
    assert_eq!(
        fix["files_changed"],
        json!(2),
        "only files actually rewritten"
    );
    let paths = push["params"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    let goals = d.notes_root().join("Goals.md").display().to_string();
    assert!(
        !paths.contains(&json!(goals)),
        "the read-only file was not written"
    );
    assert!(
        d.text("Goals.md").contains("[[Training Plan|plan]]"),
        "untouched"
    );
    d.close();
}

#[test]
fn nothing_writable_is_needs_manual_fix_with_nothing_changed() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    for f in ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"] {
        set_mode(&d.notes.root().join(f), 0o444);
    }
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fix = &fixes_of(&push)[0];
    assert_eq!(fix["status"], json!("needs_manual_fix"), "{fix}");
    assert_eq!(fix["files_changed"], json!(0));
    assert_eq!(fix["deferred"], json!(3));
    assert_eq!(push["params"]["paths"], json!([]));
    d.close();
}

#[test]
fn a_later_retry_that_finishes_the_work_sends_its_own_applied_entry() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    let goals = d.notes.root().join("Goals.md");
    set_mode(&goals, 0o444);
    rename_and_fix(&d, "Training Plan.md", "Plan.md");
    let (first, _) = client.next_fix_push(WAIT);
    assert_eq!(fixes_of(&first.unwrap())[0]["status"], json!("partial"));

    // Dan makes the file writable and saves it: a batch with that file.
    set_mode(&goals, 0o644);
    let canonical = d.notes_root().join("Goals.md");
    d.engine
        .after_batch("notes", std::slice::from_ref(&canonical))
        .expect("retry");

    let (second, _) = client.next_fix_push(WAIT);
    let second = second.expect("the retry's own push");
    let fix = &fixes_of(&second)[0];
    assert_eq!(fix["status"], json!("applied"), "{fix}");
    assert_eq!(fix["deferred"], json!(0));
    assert_eq!(fix["files_changed"], json!(3));
    assert_eq!(fix["old"], json!("Training Plan.md"));
    assert_eq!(
        second["params"]["paths"],
        json!([canonical.display().to_string()])
    );
    assert!(d.text("Goals.md").contains("[[Plan|plan]]"));
    assert_eq!(d.backlinks("@notes:Plan.md"), 3);
    // Still one log entry: the retry updated it.
    assert_eq!(d.entries().iter().filter(|e| e.kind == "rename").count(), 1);
    d.close();
}

/// Edits one file each time the writer syncs its replacement, so every
/// attempt finds the file changed since it was read.
struct ChangingFs {
    edit: AtomicBool,
    victim: &'static str,
}

fn target_of(tmp: &Path) -> PathBuf {
    let name = tmp.file_name().unwrap().to_str().unwrap();
    let original = name
        .strip_prefix('.')
        .unwrap()
        .rsplit_once(".tephra-tmp-")
        .unwrap()
        .0;
    tmp.with_file_name(original)
}

impl AtomicFs for ChangingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        RealFs.sync(path)?;
        let target = target_of(path);
        if self.edit.load(Ordering::SeqCst)
            && target.file_name().and_then(|n| n.to_str()) == Some(self.victim)
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&target)?;
            f.write_all(b"edited meanwhile\n")?;
            drop(f);
            let now = fs::metadata(&target)?.modified()?;
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_system_time(now + Duration::from_secs(7)),
            )?;
        }
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

#[test]
fn a_file_changed_during_every_attempt_is_left_for_a_retry_and_the_push_says_partial() {
    let fs = Arc::new(ChangingFs {
        edit: AtomicBool::new(true),
        victim: "Goals.md",
    });
    let d = Daemon::start(&GP1_FILES, &[], writer_over(fs), false);
    let mut client = d.connect();
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    let (push, _) = client.next_fix_push(WAIT);
    let push = push.expect("a push with fixes");
    let fix = &fixes_of(&push)[0];
    assert_eq!(fix["status"], json!("partial"), "{fix}");
    assert_eq!(fix["deferred"], json!(1));
    assert_eq!(fix["files_changed"], json!(2));
    // The edit Dan (here: the stand-in) made is not lost.
    assert!(d.text("Goals.md").ends_with("edited meanwhile\n"));
    d.close();
}

// ---------------------------------------------------------------
// One push, or none
// ---------------------------------------------------------------

#[test]
fn a_rename_with_nothing_to_rewrite_sends_no_fixes_entry_and_no_fixes_push() {
    let d = Daemon::start(
        &[("Lonely.md", "# Lonely\n"), ("Other.md", "no links\n")],
        &[],
        real_writer(),
        false,
    );
    let mut client = d.connect();
    rename_and_fix(&d, "Lonely.md", "Alone.md");

    let pushes = client.drain(QUIET);
    assert!(
        with_fixes(&pushes).is_empty(),
        "no entry for a rename that rewrote nothing: {pushes:?}"
    );
    // Today's ordinary pushes have no `fixes` key at all.
    for p in &pushes {
        assert!(p["params"].get("fixes").is_none(), "{p}");
    }
    d.close();
}

#[test]
fn an_ordinary_edit_push_is_byte_identical_to_the_old_shape() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let mut client = d.connect();
    // An event the OS watcher drops under load is not delivered again, so
    // the edit is made again (new text each time) until its push arrives.
    let mut found = None;
    'rounds: for round in 1..=4 {
        fs::write(
            d.notes.root().join("Unrelated.md"),
            format!("# Unrelated\n\nedited by Dan {round}\n"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some((p, t)) = client.next_push_text(left) else {
                continue 'rounds;
            };
            if p["params"]["paths"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|x| x.as_str().is_some_and(|x| x.ends_with("/Unrelated.md")))
            }) {
                found = Some((p, t));
                break 'rounds;
            }
        }
    }
    let (push, text) = found.expect("a push for the edit");
    let path = d.notes_root().join("Unrelated.md").display().to_string();
    let want = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"graph:changed\",\"params\":{{\"paths\":[{}],\"source\":\"notes\"}}}}",
        serde_json::to_string(&path).unwrap()
    );
    assert_eq!(text, want, "{push}");
    d.close();
}

#[test]
fn the_payload_function_is_the_old_shape_without_fixes_and_adds_only_fixes_with_them() {
    let ordinary = DaemonEvent::SourceChanged {
        source: "notes".into(),
        paths: vec![PathBuf::from("/v/a.md")],
        fixes: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&tephrad::forward::payload(&ordinary)).unwrap(),
        r#"{"paths":["/v/a.md"],"source":"notes"}"#
    );
}

#[test]
fn with_no_client_connected_the_fix_still_happens_and_is_in_the_log() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    rename_and_fix(&d, "Training Plan.md", "Plan.md");

    // The push went to nobody and nothing failed.
    assert!(d.text("Weekly Review.md").contains("[[Plan]]"));
    let entries = d.entries();
    let renames: Vec<_> = entries.iter().filter(|e| e.kind == "rename").collect();
    assert_eq!(renames.len(), 1, "{entries:?}");
    assert_eq!(renames[0].status, "applied");

    // An HTTP-only client is served as before.
    let url = format!("http://{}/rpc", d.addr);
    let reply: Value = d.rt.block_on(async {
        reqwest::Client::new()
            .post(&url)
            .bearer_auth(&d.token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"graph.get_backlinks","params":{"id":"@notes:Plan.md"}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    });
    assert_eq!(reply["result"].as_array().unwrap().len(), 3, "{reply}");

    // And a client that connects later gets the next push: the forwarder
    // is still alive.
    let mut client = d.connect();
    rename_and_fix(&d, "Plan.md", "Training Plan.md");
    let (push, _) = client.next_fix_push(WAIT);
    assert!(push.is_some(), "the forwarder survived the unheard push");
    d.close();
}

#[test]
fn a_client_that_never_reads_lags_without_hurting_the_others_or_the_fix_log() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    let slow = d.connect(); // never read until the end
    let mut slow = slow;
    let mut fast = d.connect();
    let reader = {
        let rt = d.rt.clone();
        std::thread::spawn(move || {
            let _keep = rt;
            let mut fix_pushes = 0usize;
            let mut total = 0usize;
            while let Some(p) = fast.next_push(Duration::from_secs(4)) {
                total += 1;
                if p["params"].get("fixes").is_some() {
                    fix_pushes += 1;
                }
            }
            (total, fix_pushes)
        })
    };
    let noise = |n: usize, size: usize| {
        for i in 0..n {
            d.state.broadcast_graph_changed(
                json!({ "source": "noise", "paths": [format!("{i}{}", "x".repeat(size))] }),
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    // Enough bytes that the slow client's socket stops taking them and
    // its connection task is stuck mid-send.
    noise(500, 48 * 1024);
    // The fix: its push is queued behind what the slow client has not
    // taken, and is pushed out of its buffer by the flood after it.
    rename_and_fix(&d, "Training Plan.md", "Plan.md");
    noise(1200, 4 * 1024);

    // The fix is complete and logged whether or not anyone heard it (F13).
    let entries = d.entries();
    assert_eq!(entries.iter().filter(|e| e.kind == "rename").count(), 1);
    assert_eq!(d.backlinks("@notes:Plan.md"), 3);

    let (total, fix_pushes) = reader.join().unwrap();
    assert!(total > 100, "the fast client kept receiving ({total})");
    assert_eq!(fix_pushes, 1, "the fast client got the fix push");

    // The slow client missed pushes, and the connection still works.
    let seen = slow.drain(Duration::from_secs(2)).len();
    assert!(seen < 500 + 1 + 1200, "the slow client missed some: {seen}");
    let result = slow.request("graph.get_backlinks", json!({ "id": "@notes:Plan.md" }));
    assert_eq!(result.as_array().unwrap().len(), 3);
    d.close();
}

// ---------------------------------------------------------------
// Names
// ---------------------------------------------------------------

#[test]
fn unicode_and_multibyte_names_are_valid_utf8_json_not_escaped() {
    let d = Daemon::start(
        &[
            ("café notes.md", "# café\n"),
            ("Loose ends….md", "# Loose\n"),
            ("Ref.md", "[[café notes]] and [[Loose ends…]]\n"),
        ],
        &[],
        real_writer(),
        false,
    );
    let mut client = d.connect();
    d.mv_and_rescan("café notes.md", "café plan.md");
    d.mv_and_rescan("Loose ends….md", "Loose ends… v2.md");
    d.engine.start_batch();
    d.engine
        .fix_rename("notes", "café notes.md", "café plan.md")
        .unwrap();
    d.engine
        .fix_rename("notes", "Loose ends….md", "Loose ends… v2.md")
        .unwrap();
    d.engine.finish_batch();

    let (frame, text) = loop {
        let (p, t) = client.next_push_text(WAIT).expect("a push with fixes");
        if p["params"].get("fixes").is_some() {
            break (p, t);
        }
    };
    let fixes = fixes_of(&frame);
    assert_eq!(fixes[0]["old"], json!("café notes.md"));
    assert_eq!(fixes[0]["new"], json!("café plan.md"));
    assert_eq!(fixes[1]["old"], json!("Loose ends….md"));
    assert_eq!(fixes[1]["new"], json!("Loose ends… v2.md"));
    // The wire text itself holds the characters, not `\u00e9` escapes.
    for want in ["café notes.md", "café plan.md", "Loose ends….md"] {
        assert!(text.contains(want), "{want} not in {text}");
    }
    assert!(!text.contains("\\u"), "{text}");
    d.close();
}

// ---------------------------------------------------------------
// The fix log is written before anything is announced
// ---------------------------------------------------------------

#[test]
fn the_engine_hands_over_the_announcement_only_after_the_entry_is_logged() {
    use std::sync::Mutex;
    use tephrad::fixes::{FixAnnouncement, FixEngine, FixSink};

    struct Spy {
        cache: PathBuf,
        seen: Mutex<Vec<(usize, FixAnnouncement)>>,
    }
    impl FixSink for Spy {
        fn announce(&self, a: FixAnnouncement) {
            // At the moment of announcing, the entry is in the log.
            let n = FixEngine::new(&self.cache, real_writer())
                .list(None, 100)
                .unwrap()
                .len();
            self.seen.lock().unwrap().push((n, a));
        }
    }

    let d = Daemon::start(&GP1_FILES, &[], real_writer(), false);
    d.mv_and_rescan("Training Plan.md", "Plan.md");
    let engine = FixEngine::new(&d.cache_path, real_writer());
    let spy = Arc::new(Spy {
        cache: d.cache_path.clone(),
        seen: Mutex::new(Vec::new()),
    });
    engine.set_sink(spy.clone());
    engine
        .fix_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    let seen = spy.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, 1, "the entry was already in fix.list");
    assert_eq!(seen[0].1.notices.len(), 1);
    assert_eq!(seen[0].1.written.len(), 3);
    drop(seen);
    d.close();
}
