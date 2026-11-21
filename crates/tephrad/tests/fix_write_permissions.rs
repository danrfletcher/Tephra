//! G23 / G24 / E21 / F14 with real permission bits: 0444 and 0000
//! files, a 0555 directory, a symlink and (on macOS) the real `uchg`
//! flag. Unix only. Root ignores permission bits, so each test fails
//! loudly if the run started as root instead of passing for nothing.

#![cfg(unix)]

mod fixtures;

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fixtures::{tmp_files, FixWorld, GP1_FILES};
use tephrad::refactor::fix_write::{
    FailureClass, FileFixRequest, FixWriter, RetryPlan, Sleeper, WriteStatus, MAX_ITEM_ROUNDS,
};
use tephrad::refactor::{AtomicFs, RealFs};

fn require_non_root() {
    let euid = unsafe { libc::geteuid() };
    assert_ne!(
        euid, 0,
        "fix_write_permissions must run as a non-root user: root ignores permission bits, \
         so every test here would pass or fail for the wrong reason"
    );
}

#[derive(Default)]
struct CountingSleeper {
    slept: Mutex<Vec<Duration>>,
}

impl Sleeper for CountingSleeper {
    fn sleep(&self, d: Duration) {
        self.slept.lock().unwrap().push(d);
    }
}

fn writer_with(fs: Arc<dyn AtomicFs>) -> (FixWriter, Arc<CountingSleeper>) {
    let sleeper = Arc::new(CountingSleeper::default());
    (
        FixWriter::with_parts(Arc::new(Mutex::new(())), fs, sleeper.clone()),
        sleeper,
    )
}

fn real_writer() -> (FixWriter, Arc<CountingSleeper>) {
    writer_with(Arc::new(RealFs))
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Puts a mode back on drop, so a failed assertion never leaves a
/// directory or file the temp dir cannot remove.
struct Restore<'a>(&'a Path, u32);

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(self.1));
    }
}

fn gp1_renamed() -> FixWorld {
    let w = FixWorld::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    w
}

fn request_for<'a>(reqs: &'a [FileFixRequest], rel: &str) -> &'a FileFixRequest {
    let file = format!("@notes:{rel}");
    reqs.iter()
        .find(|r| r.file == file)
        .unwrap_or_else(|| panic!("no request for {file}"))
}

/// Bytes, mtime and inode of a file.
fn identity(path: &Path) -> (Vec<u8>, i64, i64, u64) {
    let m = fs::metadata(path).unwrap();
    (fs::read(path).unwrap(), m.mtime(), m.mtime_nsec(), m.ino())
}

// ---------------------------------------------------------------
// G24 / F14: a read-only file is never rewritten
// ---------------------------------------------------------------

#[test]
fn g24_a_0444_file_is_never_rewritten_and_its_bytes_mtime_and_inode_stay() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Weekly Review.md");
    chmod(&path, 0o444);
    let _restore = Restore(&path, 0o644);
    let before = identity(&path);

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Weekly Review.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert_eq!(r.retry, RetryPlan::OnlyAtStartupRescanOrFileChange);
    assert!(r.reason.as_deref().unwrap().contains("read-only"), "{r:?}");
    let intent = r.intent.expect("intent is kept for the retry");
    assert_eq!(intent.old_target, "Training Plan.md");
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert_eq!(identity(&path), before);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert!(tmp_files(w.notes.root()).is_empty());
}

/// An ACL that denies write but not delete leaves the owner-write bit on,
/// and a temp file renamed over the file would replace it. The writer asks
/// the kernel (`access`), so it is refused like a read-only file. macOS
/// only: it sets a real ACL with `chmod +a`.
#[cfg(target_os = "macos")]
#[test]
fn an_acl_that_denies_write_is_refused_before_a_temp_file_exists() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Weekly Review.md");
    let user = String::from_utf8(
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let user = user.trim();
    let set = std::process::Command::new("chmod")
        .arg("+a")
        .arg(format!("user:{user} deny write,writeattr,writeextattr"))
        .arg(&path)
        .status()
        .unwrap();
    assert!(set.success(), "could not set the ACL");
    struct ClearAcl<'a>(&'a Path);
    impl Drop for ClearAcl<'_> {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chmod")
                .arg("-N")
                .arg(self.0)
                .status();
        }
    }
    let _clear = ClearAcl(&path);
    let before = identity(&path);
    assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o200, 0);

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Weekly Review.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert_eq!(identity(&path), before, "the file was replaced");
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn f14_a_0444_file_is_not_overwritten_by_a_second_attempt_or_a_repeat_call_or_a_give_up() {
    require_non_root();
    let w = gp1_renamed();
    let mut reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Goals.md");
    chmod(&path, 0o444);
    let _restore = Restore(&path, 0o644);
    let before = identity(&path);
    let (writer, sleeper) = real_writer();

    // Called again and again, as a retry trigger would, and with the
    // round counter already at the cap.
    for round in [
        0,
        1,
        MAX_ITEM_ROUNDS - 1,
        MAX_ITEM_ROUNDS,
        MAX_ITEM_ROUNDS + 5,
    ] {
        let req = reqs
            .iter_mut()
            .find(|r| r.file == "@notes:Goals.md")
            .unwrap();
        req.prior_rounds = round;
        let r = writer.write_file(req);
        assert_eq!(
            r.status,
            WriteStatus::NeedsManualFix,
            "round {round}: {r:?}"
        );
        assert_eq!(r.attempts, 1, "round {round}");
        assert_eq!(identity(&path), before, "round {round}");
    }
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);

    // Even when the filesystem under it fails every step, a read-only
    // file is not touched (the guard is before the first step).
    let (writer, _) = writer_with(Arc::new(AlwaysFails));
    let r = writer.write_file(request_for(&reqs, "Goals.md"));
    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(identity(&path), before);
    assert!(tmp_files(w.notes.root()).is_empty());
}

/// Every write step fails with a transient error.
struct AlwaysFails;

impl AtomicFs for AlwaysFails {
    fn write_new(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }
    fn set_permissions(&self, _: &Path, _: fs::Permissions) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }
    fn sync(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }
}

#[test]
fn g24_owner_write_off_with_group_write_on_is_still_read_only() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Goals.md");
    chmod(&path, 0o464);
    let _restore = Restore(&path, 0o644);
    let before = identity(&path);
    let (writer, _) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Goals.md"));
    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(identity(&path), before);
}

#[test]
fn g24_a_read_only_file_with_nothing_to_rewrite_is_left_alone_without_a_complaint() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Goals.md");
    // The link was removed by the user earlier.
    fs::write(&path, "# Goals\n\nNo link.\n").unwrap();
    chmod(&path, 0o444);
    let _restore = Restore(&path, 0o644);
    let before = identity(&path);
    let (writer, _) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Goals.md"));
    assert_eq!(r.status, WriteStatus::NothingToWrite, "{r:?}");
    assert_eq!(identity(&path), before);
}

// ---------------------------------------------------------------
// G23: permanent failures, first attempt, no backoff
// ---------------------------------------------------------------

#[test]
fn g23_a_0000_file_is_permanent_on_the_first_attempt_with_no_sleep() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Weekly Review.md");
    chmod(&path, 0o000);
    let _restore = Restore(&path, 0o644);

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Weekly Review.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert_eq!(r.retry, RetryPlan::OnlyAtStartupRescanOrFileChange);
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert!(tmp_files(w.notes.root()).is_empty());
    chmod(&path, 0o644);
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Training Plan]].\n"
    );
}

#[test]
fn g23_a_0555_directory_is_permanent_on_the_first_attempt_and_the_file_is_untouched() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let dir = w.notes.root().join("Ideas");
    let path = dir.join("Rename Notes.md");
    let before = identity(&path);
    chmod(&dir, 0o555);
    let _restore = Restore(&dir, 0o755);

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Ideas/Rename Notes.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert_eq!(identity(&path), before);
    chmod(&dir, 0o755);
    assert!(tmp_files(w.notes.root()).is_empty());
}

#[test]
fn g23_a_symlink_is_refused_as_unsupported_and_stays_a_symlink() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let real = w.notes.root().join("Goals.md");
    let moved = w.notes.root().join("Goals-real.md");
    fs::rename(&real, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &real).unwrap();
    let before = fs::read(&moved).unwrap();

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Goals.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert!(r.reason.as_deref().unwrap().contains("symlink"), "{r:?}");
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert!(fs::symlink_metadata(&real)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&moved).unwrap(), before);
    assert!(tmp_files(w.notes.root()).is_empty());
}

/// A permanent error from any write step: first attempt, no sleep, the
/// temp file removed.
struct PermanentAt(&'static str, i32);

impl PermanentAt {
    fn err(&self, step: &str) -> io::Result<()> {
        if self.0 == step {
            Err(io::Error::from_raw_os_error(self.1))
        } else {
            Ok(())
        }
    }
}

impl AtomicFs for PermanentAt {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)?;
        self.err("write_new")
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)?;
        self.err("set_permissions")
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        RealFs.sync(path)?;
        self.err("sync")
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.err("rename")?;
        RealFs.rename(from, to)
    }
}

#[test]
fn g23_eperm_eacces_and_erofs_from_any_step_are_permanent_with_one_attempt() {
    require_non_root();
    for (step, code) in [
        ("write_new", libc::EACCES),
        ("set_permissions", libc::EPERM),
        ("sync", libc::EROFS),
        // The immutable flag and an ACL deny both show up at the rename.
        ("rename", libc::EPERM),
        ("rename", libc::EACCES),
    ] {
        let w = gp1_renamed();
        let reqs = w.requests("Training Plan.md", "Plan.md");
        let (writer, sleeper) = writer_with(Arc::new(PermanentAt(step, code)));
        let r = writer.write_file(request_for(&reqs, "Weekly Review.md"));
        assert_eq!(
            r.status,
            WriteStatus::NeedsManualFix,
            "{step}/{code}: {r:?}"
        );
        assert_eq!(r.attempts, 1, "{step}/{code}");
        assert_eq!(r.rounds, 0, "{step}/{code}");
        assert_eq!(sleeper.slept.lock().unwrap().len(), 0, "{step}/{code}");
        assert_eq!(
            w.text("Weekly Review.md"),
            "# Weekly Review\n\nThis week: [[Training Plan]].\n"
        );
        assert!(tmp_files(w.notes.root()).is_empty(), "{step}/{code}");
    }
}

#[test]
fn g23_transient_kinds_get_a_bounded_backoff_then_are_deferred_and_at_the_cap_need_a_manual_fix() {
    require_non_root();
    for kind in [
        io::ErrorKind::WouldBlock,
        io::ErrorKind::TimedOut,
        io::ErrorKind::Interrupted,
    ] {
        let w = gp1_renamed();
        let mut reqs = w.requests("Training Plan.md", "Plan.md");
        let req = reqs
            .iter_mut()
            .find(|r| r.file == "@notes:Weekly Review.md")
            .unwrap();
        struct Fails(io::ErrorKind);
        impl AtomicFs for Fails {
            fn write_new(&self, _: &Path, _: &[u8]) -> io::Result<()> {
                Err(io::Error::from(self.0))
            }
            fn set_permissions(&self, p: &Path, m: fs::Permissions) -> io::Result<()> {
                RealFs.set_permissions(p, m)
            }
            fn sync(&self, p: &Path) -> io::Result<()> {
                RealFs.sync(p)
            }
            fn rename(&self, f: &Path, t: &Path) -> io::Result<()> {
                RealFs.rename(f, t)
            }
        }
        let (writer, sleeper) = writer_with(Arc::new(Fails(kind)));
        let mut last = None;
        for round in 1..=MAX_ITEM_ROUNDS {
            let r = writer.write_file(req);
            assert_eq!(r.attempts, 3, "{kind:?} round {round}");
            assert_eq!(r.rounds, round);
            assert_eq!(r.failure_class, Some(FailureClass::Transient));
            req.prior_rounds = r.rounds;
            last = Some(r);
        }
        let last = last.unwrap();
        assert_eq!(last.status, WriteStatus::NeedsManualFix, "{kind:?}");
        assert_eq!(last.retry, RetryPlan::OnlyAtStartupRescanOrFileChange);
        assert_eq!(last.round_cap, MAX_ITEM_ROUNDS);
        // Two waits per round, never more: no retry storm.
        assert_eq!(
            sleeper.slept.lock().unwrap().len(),
            2 * MAX_ITEM_ROUNDS as usize
        );
        assert_eq!(
            w.text("Weekly Review.md"),
            "# Weekly Review\n\nThis week: [[Training Plan]].\n"
        );
        assert!(tmp_files(w.notes.root()).is_empty());
    }
}

// ---------------------------------------------------------------
// E21: one permanent failure among several files
// ---------------------------------------------------------------

#[test]
fn e21_one_permanent_failure_leaves_the_others_written_and_the_batch_partial() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let bad = w.notes.root().join("Goals.md");
    chmod(&bad, 0o444);
    let _restore = Restore(&bad, 0o644);
    let bad_before = identity(&bad);

    let (writer, sleeper) = real_writer();
    let batch = writer.write_batch(&reqs);

    assert!(batch.is_partial(), "{batch:?}");
    assert_eq!(
        batch.with_status(WriteStatus::Written).len(),
        2,
        "{batch:?}"
    );
    let manual = batch.with_status(WriteStatus::NeedsManualFix);
    assert_eq!(manual.len(), 1, "{batch:?}");
    assert_eq!(manual[0].file, "@notes:Goals.md");
    assert_eq!(manual[0].attempts, 1);
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0, "no sleep call");
    assert_eq!(identity(&bad), bad_before);
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\n"
    );
    assert_eq!(
        w.text("Ideas/Rename Notes.md"),
        "# Rename notes\n\n![[Plan]]\n"
    );
    assert!(tmp_files(w.notes.root()).is_empty());
}

// ---------------------------------------------------------------
// Immutable flag, for real (macOS)
// ---------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn g23_a_real_uchg_flag_is_permanent_at_once_with_no_retry_and_the_file_untouched() {
    require_non_root();
    let w = gp1_renamed();
    let reqs = w.requests("Training Plan.md", "Plan.md");
    let path = w.notes.root().join("Weekly Review.md");
    let before = identity(&path);

    struct Unflag<'a>(&'a Path);
    impl Drop for Unflag<'_> {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/chflags")
                .arg("nouchg")
                .arg(self.0)
                .status();
        }
    }
    let status = std::process::Command::new("/usr/bin/chflags")
        .arg("uchg")
        .arg(&path)
        .status()
        .expect("run chflags");
    assert!(status.success(), "chflags uchg failed");
    let _unflag = Unflag(&path);

    let (writer, sleeper) = real_writer();
    let r = writer.write_file(request_for(&reqs, "Weekly Review.md"));

    assert_eq!(r.status, WriteStatus::NeedsManualFix, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(r.failure_class, Some(FailureClass::Permanent));
    assert_eq!(sleeper.slept.lock().unwrap().len(), 0);
    assert_eq!(identity(&path), before);
    assert!(tmp_files(w.notes.root()).is_empty());
}
