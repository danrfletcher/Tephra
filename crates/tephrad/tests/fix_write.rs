//! G22 / G26 / E8 / E25 and the added cases of the fix writer
//! (`tephrad::refactor::fix_write`), through the real path: real files
//! in `fixtures::FixWorld` temp sources, a real registry, cache and
//! graph, and the real `FixWriter`. The only mocks are `AtomicFs`
//! impls that change the file between the fsync and the size/mtime
//! re-check, and a sleeper that counts instead of sleeping.

mod fixtures;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fixtures::{tmp_files, FixWorld, GP1_FILES};
use tephrad::refactor::fix_write::{
    write_backoff, FileFixRequest, FixWriter, RetryPlan, Sleeper, WriteStatus, MAX_WRITE_ATTEMPTS,
    REASON_CHANGED,
};
use tephrad::refactor::{AtomicFs, NoopWatcherPause, RealFs, RefactorEngine};

// ---------------------------------------------------------------
// Seams
// ---------------------------------------------------------------

#[derive(Default)]
struct CountingSleeper {
    slept: Mutex<Vec<Duration>>,
}

impl Sleeper for CountingSleeper {
    fn sleep(&self, d: Duration) {
        self.slept.lock().unwrap().push(d);
    }
}

impl CountingSleeper {
    fn count(&self) -> usize {
        self.slept.lock().unwrap().len()
    }
}

/// What the "user" does to the file in the window between a fsync and the
/// size/mtime re-check.
type Edit = Box<dyn Fn(u32, &Path) + Send + Sync>;

/// The real filesystem, except that after the N-th fsync `edit` runs
/// against the target (the file the temp file is about to replace).
struct EditAfterSync {
    syncs: AtomicU32,
    edit: Edit,
}

impl EditAfterSync {
    fn new(edit: impl Fn(u32, &Path) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(EditAfterSync {
            syncs: AtomicU32::new(0),
            edit: Box::new(edit),
        })
    }
}

/// `.<name>.tephra-tmp-N` beside the target, back to the target.
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

impl AtomicFs for EditAfterSync {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        RealFs.sync(path)?;
        let n = self.syncs.fetch_add(1, Ordering::SeqCst) + 1;
        (self.edit)(n, &target_of(path));
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    drop(f);
    // A save is a new mtime even within one clock tick.
    bump_mtime(path);
}

fn bump_mtime(path: &Path) {
    let now = fs::metadata(path).unwrap().modified().unwrap();
    filetime::set_file_mtime(
        path,
        filetime::FileTime::from_system_time(now + Duration::from_secs(7)),
    )
    .unwrap();
}

fn writer(fs: Arc<dyn AtomicFs>, sleeper: Arc<CountingSleeper>) -> FixWriter {
    FixWriter::with_parts(Arc::new(Mutex::new(())), fs, sleeper)
}

fn real_writer() -> (FixWriter, Arc<CountingSleeper>) {
    let sleeper = Arc::new(CountingSleeper::default());
    (writer(Arc::new(RealFs), sleeper.clone()), sleeper)
}

fn request_for<'a>(reqs: &'a [FileFixRequest], file: &str) -> &'a FileFixRequest {
    reqs.iter()
        .find(|r| r.file == file)
        .unwrap_or_else(|| panic!("no request for {file}: {reqs:?}"))
}

/// A world with the GP1 files, `Training Plan.md` renamed to `Plan.md`.
fn gp1_renamed() -> FixWorld {
    let w = FixWorld::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    w
}

const WEEKLY: &str = "@notes:Weekly Review.md";

// ---------------------------------------------------------------
// Log capture (the recovered-edit tracing line)
// ---------------------------------------------------------------

/// Log capture. One subscriber is installed for the whole test binary,
/// once, and stays. A subscriber set per test with `with_default` races
/// with the tests running on parallel threads (`tracing` caches, per
/// callsite, whether anyone is listening, and a callsite first hit by a
/// thread with no subscriber can stay silent for a test that sets one a
/// moment later), which left some captures empty. The writer appends to
/// a buffer of the emitting thread, and every test runs on its own
/// thread, so a capture sees its own lines and no other test's.
mod capture {
    use std::cell::RefCell;
    use std::io;
    use std::sync::Once;

    thread_local! {
        static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    struct ThreadLog;

    impl io::Write for ThreadLog {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            LOG.with(|l| l.borrow_mut().extend_from_slice(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct MakeThreadLog;

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeThreadLog {
        type Writer = ThreadLog;
        fn make_writer(&'a self) -> ThreadLog {
            ThreadLog
        }
    }

    /// Runs `f` and returns its result with the log lines it wrote on
    /// this thread.
    pub fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(MakeThreadLog)
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in this test binary");
        });
        LOG.with(|l| l.borrow_mut().clear());
        let out = f();
        let text = LOG.with(|l| String::from_utf8(std::mem::take(&mut *l.borrow_mut())).unwrap());
        (out, text)
    }
}

use capture::logged;

// ---------------------------------------------------------------
// Regression: the PR-3 fixture through the new writer
// ---------------------------------------------------------------

#[test]
fn the_pr3_fixture_through_the_writer_is_byte_identical_to_the_pr3_rewrite() {
    // Reference: PR-3's own `fix_rename` on one copy.
    let reference = gp1_renamed();
    let done = tephrad::fix::fix_rename(
        &reference.graph(),
        &reference.registry,
        "notes",
        "Training Plan.md",
        "Plan.md",
    )
    .expect("fix_rename");
    assert_eq!(done.files_changed, 3);

    // The same rename through the writer on another copy.
    let w = gp1_renamed();
    let (writer, sleeper) = real_writer();
    let batch = writer.write_batch(&w.requests("Training Plan.md", "Plan.md"));

    assert_eq!(batch.files.len(), 3, "{batch:?}");
    assert_eq!(batch.with_status(WriteStatus::Written).len(), 3);
    assert!(!batch.is_partial());
    assert!(batch
        .files
        .iter()
        .all(|f| f.attempts == 1 && f.recovered_edits == 0));
    assert_eq!(sleeper.count(), 0);
    for rel in [
        "Weekly Review.md",
        "Goals.md",
        "Ideas/Rename Notes.md",
        "Unrelated.md",
        "Plan.md",
    ] {
        assert_eq!(w.text(rel), reference.text(rel), "{rel}");
    }
    assert_eq!(w.text("Goals.md"), "# Goals\n\nSee [[Plan|plan]] first.\n");
    assert!(tmp_files(w.notes.root()).is_empty());
}

// ---------------------------------------------------------------
// F2: needs no stored preview and no prompt
// ---------------------------------------------------------------

#[test]
fn f2_the_writer_succeeds_with_the_engines_previews_empty_and_apply_still_needs_a_preview() {
    let w = gp1_renamed();
    let graph = Arc::new(tephrad::rpc::backend::SharedGraph::new(w.graph()));
    let registry = Arc::new(tephra_registry_for(&w));
    let engine = RefactorEngine::new(graph, registry, Arc::new(NoopWatcherPause));

    // The previews map is empty: nothing was ever previewed.
    let fix = FixWriter::new(&engine);
    let batch = fix.write_batch(&w.requests("Training Plan.md", "Plan.md"));
    assert_eq!(
        batch.with_status(WriteStatus::Written).len(),
        3,
        "{batch:?}"
    );
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\n"
    );

    // The manual path is as it was: apply without a preview is refused.
    let err = engine
        .apply("notes", "Goals.md", "Aims.md")
        .expect_err("apply with no preview");
    assert!(err.contains("no prior preview_refactor call"), "{err}");
}

/// A second registry over the same cache file: the engine only needs a
/// `SourceBackend` that lists `notes`.
fn tephra_registry_for(w: &FixWorld) -> tephrad::registry::SourceRegistry {
    tephrad::registry::SourceRegistry::new(
        tephra_cache::Cache::open(&w.cache_path).expect("open cache"),
    )
    .expect("registry")
}

// ---------------------------------------------------------------
// G22: re-derived from the current text
// ---------------------------------------------------------------

#[test]
fn g22_text_added_above_the_link_since_the_graph_was_built_is_no_problem() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    // The graph's spans are now wrong: lines and bytes moved down.
    let path = w.notes.root().join("Weekly Review.md");
    fs::write(
        &path,
        "# Weekly Review\n\nNew intro line one.\nNew intro line two.\n\nThis week: [[Training Plan]]. Also [[Training Plan]].\n",
    )
    .unwrap();

    let (writer, _) = real_writer();
    let r = writer.write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.links_changed, 2);
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nNew intro line one.\nNew intro line two.\n\nThis week: [[Plan]]. Also [[Plan]].\n"
    );
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn g22_a_link_deleted_since_the_read_means_nothing_to_write_and_the_file_is_untouched() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Weekly Review.md");
    fs::write(&path, "# Weekly Review\n\nThe link is gone.\n").unwrap();
    let before = (
        fs::read(&path).unwrap(),
        fs::metadata(&path).unwrap().modified().unwrap(),
    );

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::NothingToWrite, "{r:?}");
    assert_eq!(r.links_changed, 0);
    assert_eq!(r.attempts, 1);
    assert_eq!(r.intent, None);
    assert_eq!(sleeper.count(), 0);
    let after = (
        fs::read(&path).unwrap(),
        fs::metadata(&path).unwrap().modified().unwrap(),
    );
    assert_eq!(after, before);
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn g22_a_link_deleted_between_two_attempts_is_found_gone_on_the_second_one() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|n, target| {
        if n == 1 {
            // The user deletes the link and saves.
            fs::write(target, "# Weekly Review\n\nGone.\n").unwrap();
            bump_mtime(target);
        }
    });
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::NothingToWrite, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(w.text("Weekly Review.md"), "# Weekly Review\n\nGone.\n");
    assert!(tmp_files(w.notes.root()).is_empty());
}

// ---------------------------------------------------------------
// G22 / E25: a save between the read and the rename
// ---------------------------------------------------------------

#[test]
fn e25_a_user_save_after_the_fsync_is_caught_and_survives_beside_the_link_rewrite() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    // Only the first attempt is hit.
    let fs_ = EditAfterSync::new(|n, target| {
        if n == 1 {
            append(target, "A line the user just typed.\n");
        }
    });
    let sleeper = Arc::new(CountingSleeper::default());

    let (r, log) = logged(|| writer(fs_, sleeper.clone()).write_file(request_for(&reqs, WEEKLY)));

    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(r.recovered_edits, 1);
    assert_eq!(r.rounds, 0, "a success is not a deferred round");
    // Both the saved edit and the rewrite are in the file.
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\nA line the user just typed.\n"
    );
    // One wait between the two attempts, from the named constants.
    assert_eq!(*sleeper.slept.lock().unwrap(), vec![write_backoff(1)]);
    // The recovered edit is a tracing line for the fix log.
    assert!(log.contains("recovered an edit"), "{log}");
    assert!(log.contains(WEEKLY), "{log}");
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn g22_the_nth_attempt_succeeds_and_every_earlier_change_is_counted() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|n, target| {
        if n <= 2 {
            append(target, &format!("save {n}\n"));
        }
    });
    let sleeper = Arc::new(CountingSleeper::default());
    let (r, log) = logged(|| writer(fs_, sleeper.clone()).write_file(request_for(&reqs, WEEKLY)));

    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 3);
    assert_eq!(r.recovered_edits, 2);
    assert_eq!(log.matches("recovered an edit").count(), 2, "{log}");
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\nsave 1\nsave 2\n"
    );
    assert_eq!(sleeper.count(), 2);
}

// ---------------------------------------------------------------
// E8: changed during every attempt
// ---------------------------------------------------------------

#[test]
fn e8_a_file_that_changes_during_every_attempt_comes_back_deferred_with_its_intent() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|n, target| append(target, &format!("typing {n}\n")));
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper.clone()).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::Deferred, "{r:?}");
    assert_eq!(r.reason.as_deref(), Some(REASON_CHANGED));
    assert_eq!(r.attempts, MAX_WRITE_ATTEMPTS);
    assert_eq!(MAX_WRITE_ATTEMPTS, 3);
    assert_eq!(r.recovered_edits, 0);
    assert_eq!(r.rounds, 1);
    assert!(matches!(r.retry, RetryPlan::AfterBackoff { .. }), "{r:?}");
    let intent = r.intent.expect("the leftover is carried as intent");
    assert_eq!(intent.file, WEEKLY);
    assert_eq!(intent.old_target, "Training Plan.md");
    assert_eq!(intent.chain, vec!["Training Plan.md", "Plan.md"]);
    // The target holds only the user's own edits: no link was rewritten.
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Training Plan]].\ntyping 1\ntyping 2\ntyping 3\n"
    );
    assert!(tmp_files(w.notes.root()).is_empty());
    // Two waits, none after the last attempt.
    assert_eq!(sleeper.count(), 2);
}

#[test]
fn e8_the_deferred_file_is_written_by_a_retry_that_finds_it_quiet() {
    let w = gp1_renamed();
    let mut reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|n, target| append(target, &format!("typing {n}\n")));
    let sleeper = Arc::new(CountingSleeper::default());
    let first = writer(fs_, sleeper).write_file(request_for(&reqs, WEEKLY));
    assert_eq!(first.status, WriteStatus::Deferred);

    // The caller keeps the round counter and tries again, now quiet.
    let req = reqs.iter_mut().find(|r| r.file == WEEKLY).unwrap();
    req.prior_rounds = first.rounds;
    let (writer, _) = real_writer();
    let second = writer.write_file(req);
    assert_eq!(second.status, WriteStatus::Written, "{second:?}");
    assert_eq!(second.attempts, 1);
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\ntyping 1\ntyping 2\ntyping 3\n"
    );
}

// ---------------------------------------------------------------
// Added: mtime changes, bytes do not
// ---------------------------------------------------------------

#[test]
fn a_file_whose_mtime_changes_but_size_and_content_do_not_is_retried_and_the_second_attempt_succeeds(
) {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let before = w.text("Weekly Review.md");
    let fs_ = EditAfterSync::new(|n, target| {
        if n == 1 {
            bump_mtime(target); // `touch`: same size, same bytes
        }
    });
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(r.recovered_edits, 1);
    assert_eq!(before, "# Weekly Review\n\nThis week: [[Training Plan]].\n");
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\n"
    );
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn g22_a_size_change_with_the_same_mtime_is_caught_too() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|n, target| {
        if n == 1 {
            let mtime = fs::metadata(target).unwrap().modified().unwrap();
            let mut text = fs::read_to_string(target).unwrap();
            text.push_str("more\n");
            fs::write(target, text).unwrap();
            // Put the old mtime back: only the size gives it away.
            filetime::set_file_mtime(target, filetime::FileTime::from_system_time(mtime)).unwrap();
        }
    });
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\nmore\n"
    );
}

// ---------------------------------------------------------------
// Added: gone mid-fix
// ---------------------------------------------------------------

#[test]
fn a_file_deleted_mid_fix_gives_not_found_and_no_temp_file() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|_, target| fs::remove_file(target).unwrap());
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper.clone()).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::NotFound, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.intent, None);
    assert_eq!(sleeper.count(), 0);
    assert!(!w.notes.root().join("Weekly Review.md").exists());
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn a_file_renamed_away_mid_fix_gives_not_found_and_no_temp_file() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let fs_ = EditAfterSync::new(|_, target| {
        fs::rename(target, target.with_file_name("Weekly Review (moved).md")).unwrap()
    });
    let sleeper = Arc::new(CountingSleeper::default());
    let r = writer(fs_, sleeper).write_file(request_for(&reqs, WEEKLY));

    assert_eq!(r.status, WriteStatus::NotFound, "{r:?}");
    // The moved file is the user's, untouched.
    assert_eq!(
        w.text("Weekly Review (moved).md"),
        "# Weekly Review\n\nThis week: [[Training Plan]].\n"
    );
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn a_file_that_was_already_gone_before_the_fix_is_not_found() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    fs::remove_file(w.notes.root().join("Goals.md")).unwrap();
    let (writer, sleeper) = real_writer();
    let batch = writer.write_batch(&reqs);

    let goals = batch
        .files
        .iter()
        .find(|f| f.file == "@notes:Goals.md")
        .unwrap();
    assert_eq!(goals.status, WriteStatus::NotFound);
    // The others are written: the batch is partial.
    assert_eq!(batch.with_status(WriteStatus::Written).len(), 2);
    assert!(batch.is_partial());
    assert_eq!(sleeper.count(), 0);
    assert!(tmp_files(w.notes.root()).is_empty());
}

// ---------------------------------------------------------------
// Added: permissions and a Unicode path
// ---------------------------------------------------------------

#[cfg(unix)]
#[test]
fn permissions_and_a_multibyte_name_before_the_extension_are_kept_on_a_successful_write() {
    use std::os::unix::fs::PermissionsExt;
    let w = FixWorld::new(&[
        ("Target.md", "# Target\n"),
        ("Notes—ünï….md", "See [[Target]].\n"),
        ("run.sh.md", "See [[Target]].\n"),
    ]);
    w.rename("Target.md", "Goal.md");
    let unicode = w.notes.root().join("Notes—ünï….md");
    let plain = w.notes.root().join("run.sh.md");
    fs::set_permissions(&unicode, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).unwrap();

    let (writer, _) = real_writer();
    let batch = writer.write_batch(&w.requests("Target.md", "Goal.md"));

    assert_eq!(
        batch.with_status(WriteStatus::Written).len(),
        2,
        "{batch:?}"
    );
    assert_eq!(w.text("Notes—ünï….md"), "See [[Goal]].\n");
    assert_eq!(w.text("run.sh.md"), "See [[Goal]].\n");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&unicode), 0o640);
    assert_eq!(mode(&plain), 0o755);
    assert!(tmp_files(w.notes.root()).is_empty());
}

// ---------------------------------------------------------------
// Added: no temp file after any outcome (every outcome above also
// checks it; this one covers a failing write and a failing rename)
// ---------------------------------------------------------------

struct FailAt(&'static str);

impl AtomicFs for FailAt {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)?;
        if self.0 == "write_new" {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        Ok(())
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        if self.0 == "sync" {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        RealFs.sync(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.0 == "rename" {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        RealFs.rename(from, to)
    }
}

#[test]
fn a_write_that_fails_at_any_step_leaves_no_temp_file_and_the_original_alone() {
    for step in ["write_new", "sync", "rename"] {
        let w = gp1_renamed();
        let reqs = w.requests("Training Plan.md", "Plan.md");
        let sleeper = Arc::new(CountingSleeper::default());
        let r =
            writer(Arc::new(FailAt(step)), sleeper.clone()).write_file(request_for(&reqs, WEEKLY));
        assert_eq!(r.status, WriteStatus::Deferred, "{step}: {r:?}");
        assert_eq!(r.attempts, 3, "{step}");
        assert_eq!(sleeper.count(), 2, "{step}");
        assert_eq!(
            w.text("Weekly Review.md"),
            "# Weekly Review\n\nThis week: [[Training Plan]].\n",
            "{step}"
        );
        assert!(tmp_files(w.notes.root()).is_empty(), "{step}");
    }
}

#[test]
fn a_second_run_after_success_writes_nothing() {
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let (writer, _) = real_writer();
    assert_eq!(
        writer
            .write_batch(&reqs)
            .with_status(WriteStatus::Written)
            .len(),
        3
    );
    let snapshot = fixtures::tree_snapshot(w.notes.root());
    let again = writer.write_batch(&reqs);
    assert_eq!(
        again.with_status(WriteStatus::NothingToWrite).len(),
        3,
        "{again:?}"
    );
    assert_eq!(fixtures::tree_snapshot(w.notes.root()), snapshot);
}
