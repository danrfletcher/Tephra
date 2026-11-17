//! GP13 and F14 through `link.rename_event`: a rename some of whose
//! linking files cannot be written is answered `partial`, with the
//! leftover already in the pending-fixes table, inside 5 s and before
//! anything rescans the source.
//!
//! A whole daemon (real registry, cache, server and write loop); the
//! write loop sits on a file system that edits `Weekly Review.md` each
//! time the writer is about to replace it. The sleeper does not sleep.

mod fix_world;
mod fixtures;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fix_world::{fixes_of, writer_over, Daemon, WAIT};
use fixtures::GP1_FILES;
use serde_json::json;
use tephra_cache::Cache;
use tephrad::refactor::{AtomicFs, RealFs};

/// Edits `victim` each time the writer syncs its replacement, so every
/// attempt finds the file changed since it was read. The edits are
/// counted.
struct ChangingFs {
    edit: AtomicBool,
    victim: &'static str,
    edits: AtomicUsize,
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
            self.edits.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

fn event_params(d: &Daemon, old: &str, new: &str) -> serde_json::Value {
    let root = d.notes_root();
    json!({
        "old_path": root.join(old).display().to_string(),
        "new_path": root.join(new).display().to_string(),
    })
}

fn pending_rows(d: &Daemon) -> Vec<tephra_cache::PendingFixRow> {
    Cache::open(&d.cache_path)
        .unwrap()
        .pending_fixes(None, None)
        .unwrap()
}

#[test]
fn gp13_a_file_that_changes_under_every_attempt_is_partial_with_the_leftover_stored() {
    let fs = Arc::new(ChangingFs {
        edit: AtomicBool::new(true),
        victim: "Weekly Review.md",
        edits: AtomicUsize::new(0),
    });
    let d = Daemon::start(&GP1_FILES, &[], writer_over(fs.clone()), false);
    let mut client = d.connect();
    d.mv("Training Plan.md", "Plan.md");
    let scanned_before = Cache::open(&d.cache_path)
        .unwrap()
        .get_source("notes")
        .unwrap()
        .unwrap()
        .last_scanned_at;

    let started = Instant::now();
    let frame = client.call(
        "link.rename_event",
        event_params(&d, "Training Plan.md", "Plan.md"),
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the reply took {:?}",
        started.elapsed()
    );
    assert_eq!(
        frame["result"],
        json!({
            "status": "partial",
            "files_changed": 2,
            "deferred": [{"file": "Weekly Review.md", "reason": "changed"}],
        }),
        "{frame}"
    );

    // The leftover is in the table by the time the reply has arrived.
    let rows = pending_rows(&d);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].file_path, "Weekly Review.md");
    assert_eq!(rows[0].source, "notes");
    assert_eq!(rows[0].old_path.as_deref(), Some("Training Plan.md"));
    assert_eq!(rows[0].new_path.as_deref(), Some("Plan.md"));
    assert_eq!(rows[0].state, tephra_cache::STATE_PENDING);

    // The reply did not wait for, and did not cause, a rescan.
    let scanned_after = Cache::open(&d.cache_path)
        .unwrap()
        .get_source("notes")
        .unwrap()
        .unwrap()
        .last_scanned_at;
    assert_eq!(
        scanned_before, scanned_after,
        "a rescan ran before the reply"
    );

    // The two writable files are rewritten; the third is Dan's, as
    // edited, and not lost.
    assert!(d.text("Goals.md").contains("[[Plan|plan]]"));
    assert!(d.text("Ideas/Rename Notes.md").contains("![[Plan]]"));
    assert!(d.text("Weekly Review.md").ends_with("edited meanwhile\n"));
    assert!(d.text("Weekly Review.md").contains("[[Training Plan]]"));
    // All three attempts at it were made.
    assert_eq!(fs.edits.load(Ordering::SeqCst), 3);

    // One log entry, partial; the same event again is answered from it
    // and neither writes nor adds a row.
    let entries = d.entries();
    assert_eq!(entries.iter().filter(|e| e.kind == "rename").count(), 1);
    assert_eq!(entries[0].status, "partial");
    let again = client.call(
        "link.rename_event",
        event_params(&d, "Training Plan.md", "Plan.md"),
    );
    assert_eq!(again["result"], json!({"status": "already_applied"}));
    assert_eq!(d.entries().len(), entries.len());
    assert_eq!(pending_rows(&d).len(), 1);
    d.close();
}

#[test]
fn gp13_the_retry_after_the_file_is_released_logs_applied_and_pushes_it() {
    let fs = Arc::new(ChangingFs {
        edit: AtomicBool::new(true),
        victim: "Weekly Review.md",
        edits: AtomicUsize::new(0),
    });
    let d = Daemon::start(&GP1_FILES, &[], writer_over(fs.clone()), false);
    let mut client = d.connect();
    d.mv("Training Plan.md", "Plan.md");
    let frame = client.call(
        "link.rename_event",
        event_params(&d, "Training Plan.md", "Plan.md"),
    );
    assert_eq!(frame["result"]["status"], json!("partial"), "{frame}");
    // The event's own push first.
    let (first, _) = client.next_fix_push(WAIT);
    assert_eq!(
        fixes_of(&first.expect("the event's push"))[0]["status"],
        json!("partial")
    );

    // Dan saves the file and the source is rescanned: the retry runs.
    fs.edit.store(false, Ordering::SeqCst);
    d.engine.after_rescan("notes").expect("retry");
    let (second, _) = client.next_fix_push(WAIT);
    let second = second.expect("the retry's push");
    assert_eq!(fixes_of(&second)[0]["status"], json!("applied"), "{second}");
    assert!(d.text("Weekly Review.md").contains("[[Plan]]"));
    assert!(pending_rows(&d).is_empty());
    d.close();
}

#[cfg(unix)]
#[test]
fn f14_a_read_only_file_is_never_overwritten_it_is_deferred_as_needs_manual_fix() {
    use std::os::unix::fs::PermissionsExt;

    let d = Daemon::start(&GP1_FILES, &[], fix_world::real_writer(), false);
    let mut client = d.connect();
    let path = d.notes.root().join("Weekly Review.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let before = fs::read(&path).unwrap();
    d.mv("Training Plan.md", "Plan.md");

    let frame = client.call(
        "link.rename_event",
        event_params(&d, "Training Plan.md", "Plan.md"),
    );
    let result = &frame["result"];
    assert_eq!(result["status"], json!("partial"), "{frame}");
    assert_eq!(result["files_changed"], json!(2));
    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    assert_eq!(result["deferred"][0]["file"], json!("Weekly Review.md"));
    assert!(
        !result["deferred"][0]["reason"].as_str().unwrap().is_empty(),
        "{frame}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "a read-only file was written"
    );

    let rows = pending_rows(&d);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].state, tephra_cache::STATE_NEEDS_MANUAL_FIX);
    // `fix.list` shows the entry as partial, with the two files written.
    let list = client.request("fix.list", json!({}));
    assert_eq!(list[0]["status"], json!("partial"), "{list}");
    assert_eq!(list[0]["files_changed"], json!(2));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    d.close();
}
