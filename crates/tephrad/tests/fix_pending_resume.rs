//! G9 / G10 / E9 through the real path: real files in temp sources, a
//! real registry, cache and graph, and the real write loop. A "kill" is
//! the engine's test hook, which stops a run after N writes and leaves
//! the cache and the disk as they are; "restart" is a new engine over
//! the same cache file.

mod fixtures;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fixtures::{FixWorld, GP1_FILES};
use tephra_cache::{Cache, CURRENT_SCHEMA_VERSION, STATE_NEEDS_MANUAL_FIX, STATE_PENDING};
use tephrad::fixes::log::{FixDetail, FILE_NEEDS_MANUAL_FIX, FILE_PENDING, FILE_WRITTEN};
use tephrad::fixes::{FileWriter, FixEngine, FixError};
use tephrad::refactor::fix_write::{FileFixRequest, FileWriteResult, FixWriter, ThreadSleeper};
use tephrad::refactor::RealFs;

const RENAMED: [&str; 3] = ["Goals.md", "Ideas/Rename Notes.md", "Weekly Review.md"];

fn real_writer() -> Arc<dyn FileWriter> {
    Arc::new(FixWriter::with_parts(
        Arc::new(Mutex::new(())),
        Arc::new(RealFs),
        Arc::new(ThreadSleeper),
    ))
}

fn engine(w: &FixWorld) -> FixEngine {
    FixEngine::new(&w.cache_path, real_writer())
}

fn world() -> FixWorld {
    let w = FixWorld::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    w
}

fn cache(w: &FixWorld) -> Cache {
    Cache::open(&w.cache_path).expect("open cache")
}

struct Snapshot {
    bytes: String,
    mtime: SystemTime,
}

fn snapshot(w: &FixWorld, rel: &str) -> Snapshot {
    Snapshot {
        bytes: w.text(rel),
        mtime: fs::metadata(w.notes.root().join(rel))
            .unwrap()
            .modified()
            .unwrap(),
    }
}

fn is_fixed(w: &FixWorld, rel: &str) -> bool {
    let text = w.text(rel);
    text.contains("[[Plan") && !text.contains("[[Training Plan")
}

fn detail(w: &FixWorld, id: i64) -> FixDetail {
    let row = cache(w).get_fix_log(id).unwrap().unwrap();
    FixDetail::parse(row.detail_json.as_deref())
}

// ---------------------------------------------------------------
// G9, G10: the happy path
// ---------------------------------------------------------------

#[test]
fn one_rename_gives_one_applied_entry_naming_three_files() {
    let w = world();
    let entry = engine(&w)
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .expect("fix");
    assert_eq!(entry.kind, "rename");
    assert_eq!(entry.status, "applied");
    assert_eq!(entry.files_changed, 3);
    assert_eq!(entry.old.as_deref(), Some("Training Plan.md"));
    assert_eq!(entry.new.as_deref(), Some("Plan.md"));
    assert_eq!(entry.sources, ["notes"]);
    for rel in RENAMED {
        assert!(is_fixed(&w, rel), "{rel}: {}", w.text(rel));
    }
    assert_eq!(cache(&w).list_fix_log(None, 50).unwrap().len(), 1);
    assert_eq!(cache(&w).pending_fixes(None, None).unwrap(), []);
    let d = detail(&w, entry.id);
    assert_eq!(d.files.len(), 3);
    assert!(d.files.iter().all(|f| f.status == FILE_WRITTEN));
    assert_eq!(d.files[0].source, "notes");
}

#[test]
fn a_rename_nothing_links_to_is_one_applied_entry_with_no_files() {
    let w = FixWorld::new(&[("Solo.md", "# Solo\n")]);
    w.rename("Solo.md", "Single.md");
    let entry = engine(&w)
        .begin_rename("notes", "Solo.md", "Single.md")
        .unwrap();
    assert_eq!((entry.status.as_str(), entry.files_changed), ("applied", 0));
    assert_eq!(cache(&w).pending_fixes(None, None).unwrap(), []);
}

#[test]
fn begin_rename_refuses_a_rename_that_was_not_indexed() {
    let w = FixWorld::new(&GP1_FILES);
    let err = engine(&w)
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap_err();
    assert!(matches!(err, FixError::Refused(_)), "{err:?}");
    assert_eq!(cache(&w).table_row_count("fix_log").unwrap(), 0);
}

/// G9: every file is stored before the first write.
#[test]
fn the_work_list_and_the_entry_are_stored_before_the_first_write() {
    struct Spy {
        cache_path: PathBuf,
        inner: Arc<dyn FileWriter>,
        seen: Mutex<Vec<(usize, usize)>>,
    }
    impl FileWriter for Spy {
        fn write_file(&self, req: &FileFixRequest) -> FileWriteResult {
            let c = Cache::open(&self.cache_path).unwrap();
            let pending = c.pending_fixes(None, None).unwrap().len();
            let log = c.list_fix_log(None, 50).unwrap().len();
            self.seen.lock().unwrap().push((pending, log));
            self.inner.write_file(req)
        }
    }
    let w = world();
    let spy = Arc::new(Spy {
        cache_path: w.cache_path.clone(),
        inner: real_writer(),
        seen: Mutex::new(Vec::new()),
    });
    FixEngine::new(&w.cache_path, spy.clone())
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    // At each write: still three rows minus the files already recorded,
    // and always the one entry.
    assert_eq!(*spy.seen.lock().unwrap(), [(3, 1), (2, 1), (1, 1)]);
}

// ---------------------------------------------------------------
// E9: kill and restart at each step of a 3-file fix
// ---------------------------------------------------------------

#[test]
fn a_kill_after_n_writes_resumes_with_the_rest_and_never_touches_a_written_file_again() {
    for n in 1..=3usize {
        let w = world();
        let killed = engine(&w).with_stop_after_writes(n).begin_rename(
            "notes",
            "Training Plan.md",
            "Plan.md",
        );
        assert_eq!(killed.unwrap_err(), FixError::Stopped, "n={n}");

        // Exactly the first n files (in list order) are rewritten.
        let done: Vec<&str> = RENAMED.iter().copied().take(n).collect();
        for rel in RENAMED {
            assert_eq!(is_fixed(&w, rel), done.contains(&rel), "n={n} {rel}");
        }
        assert_eq!(
            cache(&w).pending_fixes(None, None).unwrap().len(),
            3 - n,
            "n={n}: a row for every file not yet recorded"
        );
        let log = cache(&w).list_fix_log(None, 50).unwrap();
        assert_eq!(log.len(), 1, "n={n}");
        assert_eq!(log[0].files_changed, n as i64);
        assert_eq!(log[0].status, if n == 3 { "applied" } else { "partial" });

        let before: Vec<Snapshot> = done.iter().map(|r| snapshot(&w, r)).collect();

        // Restart.
        let summary = engine(&w).resume().expect("resume");
        assert_eq!(summary.written, 3 - n, "n={n}");

        for rel in RENAMED {
            assert!(is_fixed(&w, rel), "n={n} {rel}");
        }
        for (rel, was) in done.iter().zip(&before) {
            let now = snapshot(&w, rel);
            assert_eq!(now.bytes, was.bytes, "n={n} {rel} rewritten again");
            assert_eq!(now.mtime, was.mtime, "n={n} {rel} touched again");
        }
        let log = cache(&w).list_fix_log(None, 50).unwrap();
        assert_eq!(log.len(), 1, "n={n}: still one entry");
        assert_eq!(
            (log[0].status.as_str(), log[0].files_changed),
            ("applied", 3)
        );
        assert_eq!(cache(&w).pending_fixes(None, None).unwrap(), []);
    }
}

#[test]
fn a_kill_after_the_list_was_stored_and_before_any_write_resumes_all_three() {
    let w = world();
    // The state a kill right after the transaction leaves: entry and
    // rows, nothing written. The engine's own hook cannot stop before
    // the first write, so the transaction is made with the cache API.
    let c = cache(&w);
    let files: Vec<_> = RENAMED
        .iter()
        .map(|p| tephra_cache::NewPendingFix {
            source: "notes".into(),
            file_path: (*p).into(),
            old_source: Some("notes".into()),
            old_path: Some("Training Plan.md".into()),
            new_source: Some("notes".into()),
            new_path: Some("Plan.md".into()),
        })
        .collect();
    let detail = FixDetail {
        files: RENAMED
            .iter()
            .map(|p| tephrad::fixes::log::FileEntry::pending("notes", p))
            .collect(),
        ..FixDetail::default()
    };
    c.start_fix(
        &tephra_cache::NewFixLog {
            created_at: 1_000,
            kind: "rename".into(),
            status: "partial".into(),
            old_source: Some("notes".into()),
            old_path: Some("Training Plan.md".into()),
            new_source: Some("notes".into()),
            new_path: Some("Plan.md".into()),
            files_changed: 0,
            detail_json: Some(detail.to_json()),
        },
        &files,
    )
    .unwrap();
    drop(c);
    assert!(RENAMED.iter().all(|r| !is_fixed(&w, r)));

    let summary = engine(&w).resume().unwrap();
    assert_eq!(summary.written, 3);
    assert!(RENAMED.iter().all(|r| is_fixed(&w, r)));
    let log = cache(&w).list_fix_log(None, 50).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].status.as_str(), log[0].files_changed),
        ("applied", 3)
    );
    assert_eq!(log[0].created_at, 1_000, "the entry keeps its own time");
}

/// E9: the entry and its rows are one transaction.
#[test]
fn a_failure_while_the_entry_is_written_leaves_no_entry_no_rows_and_no_file_changed() {
    let w = world();
    cache(&w)
        .connection()
        .execute_batch(
            "CREATE TRIGGER refuse_third BEFORE INSERT ON pending_fixes \
             WHEN NEW.file_path = 'Weekly Review.md' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let err = engine(&w)
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap_err();
    assert!(matches!(err, FixError::Cache(_)), "{err:?}");
    assert_eq!(cache(&w).table_row_count("fix_log").unwrap(), 0);
    assert_eq!(cache(&w).table_row_count("pending_fixes").unwrap(), 0);
    assert!(
        RENAMED.iter().all(|r| !is_fixed(&w, r)),
        "nothing was written"
    );
}

#[test]
fn a_restart_with_nothing_pending_writes_nothing_and_logs_nothing() {
    let w = world();
    engine(&w)
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    let before: Vec<Snapshot> = RENAMED.iter().map(|r| snapshot(&w, r)).collect();
    let log_before = cache(&w).list_fix_log(None, 50).unwrap();

    let summary = engine(&w).resume().unwrap();

    assert_eq!(summary, Default::default());
    assert_eq!(cache(&w).list_fix_log(None, 50).unwrap(), log_before);
    for (rel, was) in RENAMED.iter().zip(&before) {
        let now = snapshot(&w, rel);
        assert_eq!((now.bytes, now.mtime), (was.bytes.clone(), was.mtime));
    }

    // And on a world that never had a fix at all.
    let fresh = FixWorld::new(&GP1_FILES);
    assert_eq!(engine(&fresh).resume().unwrap(), Default::default());
    assert_eq!(cache(&fresh).table_row_count("fix_log").unwrap(), 0);
}

#[test]
fn the_log_and_the_pending_list_are_still_there_after_a_cache_schema_bump() {
    let w = world();
    let _ =
        engine(&w)
            .with_stop_after_writes(1)
            .begin_rename("notes", "Training Plan.md", "Plan.md");
    let log_before = cache(&w).list_fix_log(None, 50).unwrap();
    let pending_before = cache(&w).pending_fixes(None, None).unwrap();
    assert_eq!((log_before.len(), pending_before.len()), (1, 2));
    cache(&w)
        .connection()
        .execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION - 1],
        )
        .unwrap();

    let reopened = cache(&w);
    assert_eq!(
        reopened.rebuilt_from_version(),
        Some(CURRENT_SCHEMA_VERSION - 1)
    );
    assert_eq!(reopened.list_fix_log(None, 50).unwrap(), log_before);
    assert_eq!(reopened.pending_fixes(None, None).unwrap(), pending_before);
}

// ---------------------------------------------------------------
// G9: a permanent failure among three files
// ---------------------------------------------------------------

fn make_read_only(w: &FixWorld, rel: &str) {
    let path = w.notes.root().join(rel);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
}

fn make_writable(w: &FixWorld, rel: &str) {
    let path = w.notes.root().join(rel);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
}

fn mode(w: &FixWorld, rel: &str) -> u32 {
    fs::metadata(w.notes.root().join(rel))
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn one_permanent_failure_among_three_files_gives_a_partial_entry_and_starts_no_retry_loop() {
    let w = world();
    make_read_only(&w, "Goals.md");
    let original_goals = w.text("Goals.md");
    let e = engine(&w);

    let entry = e
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();

    // The other two are written; the entry is partial; the file needs a person.
    assert!(is_fixed(&w, "Weekly Review.md"));
    assert!(is_fixed(&w, "Ideas/Rename Notes.md"));
    assert_eq!(w.text("Goals.md"), original_goals);
    assert_eq!(mode(&w, "Goals.md"), 0o444, "F14: the mode is untouched");
    assert_eq!((entry.status.as_str(), entry.files_changed), ("partial", 2));
    let rows = cache(&w).pending_fixes(None, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file_path, "Goals.md");
    assert_eq!(rows[0].state, STATE_NEEDS_MANUAL_FIX);
    let d = detail(&w, entry.id);
    let goals = d.files.iter().find(|f| f.path == "Goals.md").unwrap();
    assert_eq!(goals.status, FILE_NEEDS_MANUAL_FIX);
    assert!(goals.reason.as_deref().unwrap_or("").contains("read-only"));

    // No retry loop: later batches elsewhere in the source do nothing to it.
    let row_before = rows[0].clone();
    for _ in 0..3 {
        let s = e
            .after_batch("notes", &[w.notes.root().join("Unrelated.md")])
            .unwrap();
        assert_eq!(s, Default::default());
    }
    assert_eq!(
        cache(&w).get_pending_fix(row_before.id).unwrap().unwrap(),
        row_before
    );
    assert_eq!(mode(&w, "Goals.md"), 0o444);
}

#[test]
fn a_permanent_failure_is_retried_at_startup_on_rescan_and_when_that_file_changes_and_the_retry_updates_the_same_entry(
) {
    // Startup, rescan and a change of the file each retry it; while the
    // file stays read-only each retry is refused and the mode untouched.
    let w = world();
    make_read_only(&w, "Goals.md");
    let e = engine(&w);
    let entry = e
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    assert_eq!(entry.status, "partial");

    let s = e.resume().unwrap();
    assert_eq!(s.needs_manual_fix, 1, "retried at startup");
    let s = e.after_rescan("notes").unwrap();
    assert_eq!(s.needs_manual_fix, 1, "retried on source.rescan");
    let s = e
        .after_batch("notes", &[w.notes.root().join("Goals.md")])
        .unwrap();
    assert_eq!(s.needs_manual_fix, 1, "retried when the file changed");
    assert_eq!(mode(&w, "Goals.md"), 0o444);
    assert!(!is_fixed(&w, "Goals.md"));

    // The person fixes the mode and edits the file: the next batch writes it.
    make_writable(&w, "Goals.md");
    let before = w.text("Goals.md");
    let s = e
        .after_batch("notes", &[w.notes.root().join("Goals.md")])
        .unwrap();
    assert_eq!(s.written, 1);
    let after = w.text("Goals.md");
    // F3: only link text changed; every byte outside the link is the same.
    assert_eq!(
        after,
        before.replace("[[Training Plan|plan]]", "[[Plan|plan]]")
    );
    assert_ne!(after, before);

    let log = cache(&w).list_fix_log(None, 50).unwrap();
    assert_eq!(log.len(), 1, "the retry updated the entry, it added none");
    assert_eq!(log[0].id, entry.id);
    assert_eq!(
        (log[0].status.as_str(), log[0].files_changed),
        ("applied", 3)
    );
    assert_eq!(cache(&w).pending_fixes(None, None).unwrap(), []);
}

#[test]
fn a_retry_never_rewrites_a_file_whose_owner_write_bit_is_off() {
    let w = world();
    make_read_only(&w, "Weekly Review.md");
    let before = snapshot(&w, "Weekly Review.md");
    let e = engine(&w);
    e.begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    for _ in 0..2 {
        e.resume().unwrap();
        e.after_rescan("notes").unwrap();
        e.after_batch("notes", &[w.notes.root().join("Weekly Review.md")])
            .unwrap();
    }
    let now = snapshot(&w, "Weekly Review.md");
    assert_eq!((now.bytes, now.mtime), (before.bytes, before.mtime));
    assert_eq!(mode(&w, "Weekly Review.md"), 0o444);
    let rows = cache(&w).pending_fixes(None, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, STATE_NEEDS_MANUAL_FIX);
}

#[test]
fn a_file_all_three_of_whose_peers_fail_permanently_makes_a_needs_manual_fix_entry() {
    let w = world();
    for rel in RENAMED {
        make_read_only(&w, rel);
    }
    let entry = engine(&w)
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    assert_eq!(
        (entry.status.as_str(), entry.files_changed),
        ("needs_manual_fix", 0)
    );
    let pending = cache(&w).pending_fixes(None, None).unwrap();
    assert!(pending.iter().all(|r| r.state == STATE_NEEDS_MANUAL_FIX));
    assert_eq!(pending.len(), 3);
    // Never marked as still pending in the entry.
    assert!(detail(&w, entry.id)
        .files
        .iter()
        .all(|f| f.status != FILE_PENDING && f.status != STATE_PENDING));
}
