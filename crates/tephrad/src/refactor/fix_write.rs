//! The per-file write path of the automatic link fix (G22, G23, G24,
//! G26): rewrites the links in ONE linking file, safely, and says what
//! happened.
//!
//! What is here and what is not. This is the writer: given one file and
//! the links in it that named a renamed target, it reads the file as it
//! is now, works out the rewrite from that text, writes it and tells the
//! caller what came of it. Finding renames, the reply vocabulary of the
//! RPC, the fix log, the pending list and the triggers that call a
//! retry (startup, `source.rescan`, a file change) belong to other work;
//! this module only returns the facts they need ([`FileWriteResult`]).
//! The manual preview-then-apply path is not touched.
//!
//! What it shares with `link.apply_refactor`: [`compute_changes`] and
//! [`compute_fix_changes`] (which spans change, and to what) and
//! [`apply_changes_to_content`], the same `apply_lock` (as
//! [`super::RefactorEngine::writer_gate`]) and the same per-path
//! [`crate::file_lock`]. What it does not use: the manual refactor's
//! whole apply routine, with its long settle sleep after writing, its
//! full source rescan and its up-front check on modification time
//! alone. It needs no stored preview and asks nothing of the user.
//!
//! The write loop, per file (G22). Under the gate and the file's lock:
//!
//! 1. look at the file (a symlink, a directory, a missing file, an
//!    immutable flag, are answered here, before anything is created);
//! 2. take its fingerprint (size and mtime), read it, take the
//!    fingerprint again (a change during the read is a change);
//! 3. work out the rewrite from the text just read, by link text and
//!    never by byte offsets saved earlier: a link the file no longer
//!    contains gives nothing to write for it;
//! 4. refuse a file whose owner-write bit is off (G24, F14), before any
//!    temp file exists;
//! 5. write a temp file beside it, give it the file's permissions,
//!    fsync it;
//! 6. look at the original's fingerprint once more, size AND mtime, just
//!    before the rename. If it moved, the temp file is deleted, the
//!    original is left alone and the loop goes round again from 1;
//! 7. rename the temp file over the original.
//!
//! Up to [`MAX_WRITE_ATTEMPTS`] rounds of that; a short bounded backoff
//! ([`write_backoff`]) between them, taken with the gate and the lock
//! released. An attempt that found the file changed and a later one that
//! succeeded is a recovered edit: counted in the result and logged with
//! `tracing` (for the fix log to pick up).
//!
//! Failures are classified ([`classify_io_error`]):
//!
//! - Permanent (immutable flag, `EPERM`, `EACCES` including an ACL deny,
//!   an unwritable directory, a symlink, a read-only mode, a read-only
//!   file system): the first attempt ends it. Status
//!   [`WriteStatus::NeedsManualFix`], no backoff, no second attempt, and
//!   the result says it is retried only at startup, on `source.rescan`
//!   or when that file changes ([`RetryPlan::OnlyAtStartupRescanOrFileChange`]).
//! - Transient (changed on every attempt, `WouldBlock`, `TimedOut`, a
//!   locked or cloud placeholder file): [`WriteStatus::Deferred`] with
//!   the reason and a backoff for the caller to wait. Each such round is
//!   counted ([`FileWriteResult::rounds`]); at [`MAX_ITEM_ROUNDS`] the
//!   file becomes `NeedsManualFix`, so no retry storm can start. The
//!   counter and the cap are in the result for the caller to persist.
//!
//! A file that is deleted or renamed away mid-fix gives
//! [`WriteStatus::NotFound`]; no temp file is left behind on any path.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tephra_graph::Edge;

use super::{
    apply_changes_to_content, compute_changes, compute_fix_changes, compute_move_changes, AtomicFs,
    LinkChange, MoveRewrite, RealFs, RefactorEngine,
};
use crate::file_lock;

/// Write attempts in one call for one file (G22).
pub const MAX_WRITE_ATTEMPTS: u32 = 3;
/// Wait before the second attempt in one call; doubled for each next one.
pub const WRITE_BACKOFF_BASE: Duration = Duration::from_millis(25);
/// The longest wait between two attempts in one call.
pub const WRITE_BACKOFF_MAX: Duration = Duration::from_millis(200);
/// Calls ("rounds") that may end in a transient failure for one file
/// before it becomes `needs_manual_fix` (G23). A caller keeps the count
/// and passes it back in as [`FileFixRequest::prior_rounds`].
pub const MAX_ITEM_ROUNDS: u32 = 5;
/// The wait the caller is advised to leave after a first deferred round;
/// doubled for each next round.
pub const ITEM_BACKOFF_BASE: Duration = Duration::from_secs(2);
/// The longest wait advised between two rounds.
pub const ITEM_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The reason text of a file that changed during every attempt.
pub const REASON_CHANGED: &str = "changed";

/// How long to wait between two attempts: [`WRITE_BACKOFF_BASE`] doubled
/// for every attempt already made after the first, never over
/// [`WRITE_BACKOFF_MAX`]. `attempt` is the attempt that just failed (1-based).
pub fn write_backoff(attempt: u32) -> Duration {
    doubled(WRITE_BACKOFF_BASE, attempt, WRITE_BACKOFF_MAX)
}

/// The wait advised before the next round, after `rounds` rounds have
/// ended in a transient failure (1-based): [`ITEM_BACKOFF_BASE`] doubled
/// each time, never over [`ITEM_BACKOFF_MAX`].
pub fn item_backoff(rounds: u32) -> Duration {
    doubled(ITEM_BACKOFF_BASE, rounds, ITEM_BACKOFF_MAX)
}

fn doubled(base: Duration, n: u32, max: Duration) -> Duration {
    let shift = n.saturating_sub(1).min(20);
    base.checked_mul(1u32 << shift).unwrap_or(max).min(max)
}

/// Waiting between attempts, behind a trait so a test can count the
/// sleeps and not take them.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, d: Duration);
}

/// The real thing: the thread sleeps.
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

// ---------------------------------------------------------------
// Failure classes
// ---------------------------------------------------------------

/// Whether trying again could help.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Nothing the writer does will change the outcome: the first
    /// attempt ends it and it is not retried by the writer.
    Permanent,
    /// May pass: retried with a bounded backoff and a cap.
    Transient,
}

/// Sorts an I/O error into [`FailureClass`].
///
/// Permanent: `PermissionDenied` (`EACCES`, `EPERM`: an immutable flag,
/// an ACL deny, an unwritable directory), `Unsupported` (the symlink
/// refusal), `EROFS` (read-only file system), and the shapes of path that
/// can never be written (`EISDIR`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`).
///
/// Transient: `WouldBlock`, `TimedOut`, `Interrupted`, and `EBUSY`,
/// `ETXTBSY`, `EDEADLK` (a file locked by someone else, or a cloud
/// placeholder macOS will not fetch for this process). Anything not
/// recognised is transient too: it is bounded by the attempt and round
/// caps, and giving up on an unknown error is the safe side.
pub fn classify_io_error(e: &io::Error) -> FailureClass {
    use io::ErrorKind as K;
    if let Some(code) = e.raw_os_error() {
        match code {
            libc::EPERM
            | libc::EACCES
            | libc::EROFS
            | libc::EISDIR
            | libc::ENOTDIR
            | libc::ELOOP
            | libc::ENAMETOOLONG => return FailureClass::Permanent,
            libc::EAGAIN
            | libc::EBUSY
            | libc::ETXTBSY
            | libc::EDEADLK
            | libc::ETIMEDOUT
            | libc::EINTR => return FailureClass::Transient,
            _ => {}
        }
    }
    match e.kind() {
        K::PermissionDenied | K::Unsupported => FailureClass::Permanent,
        K::WouldBlock | K::TimedOut | K::Interrupted => FailureClass::Transient,
        _ => FailureClass::Transient,
    }
}

// ---------------------------------------------------------------
// Request and result
// ---------------------------------------------------------------

/// How the rewrite is derived from the current text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteMode {
    /// [`compute_fix_changes`]: wikilinks only, a bare target kept bare
    /// only when `bare_resolves`.
    Fix { bare_resolves: bool },
    /// [`compute_changes`]: every link kind, as the manual refactor does.
    Manual,
    /// [`compute_move_changes`]: the links of a file after the file they
    /// name moved to another source (G6).
    Move(MoveRewrite),
}

/// One linking file and the links in it that named the old target.
#[derive(Debug, Clone)]
pub struct FileFixRequest {
    /// The file's federated id (`@source:rel_path`), as in the edges.
    pub file: String,
    /// Where it is on disk.
    pub abs_path: PathBuf,
    /// The graph's edges from this file to the old target. Only their
    /// link text is used (`to_raw`, `kind`); spans are found again in
    /// the text as it is when written.
    pub edges: Vec<Edge>,
    pub old_path: String,
    pub new_path: String,
    pub mode: RewriteMode,
    /// The renames leading from the old target to where it is now
    /// (`[A, B, C]`). Empty means `[old_path, new_path]`. Carried into
    /// the leftover [`FixIntent`] unchanged.
    pub chain: Vec<String>,
    /// Rounds of this file that already ended in a transient failure
    /// (0 for a first try). Kept by the caller between calls.
    pub prior_rounds: u32,
}

impl FileFixRequest {
    pub fn new(
        file: impl Into<String>,
        abs_path: impl Into<PathBuf>,
        edges: Vec<Edge>,
        old_path: impl Into<String>,
        new_path: impl Into<String>,
        mode: RewriteMode,
    ) -> Self {
        FileFixRequest {
            file: file.into(),
            abs_path: abs_path.into(),
            edges,
            old_path: old_path.into(),
            new_path: new_path.into(),
            mode,
            chain: Vec::new(),
            prior_rounds: 0,
        }
    }

    fn intent(&self) -> FixIntent {
        let chain = if self.chain.is_empty() {
            vec![self.old_path.clone(), self.new_path.clone()]
        } else {
            self.chain.clone()
        };
        FixIntent {
            file: self.file.clone(),
            old_target: self.old_path.clone(),
            chain,
        }
    }
}

/// Work left over, stored as intent and never as recorded edits: the
/// file, the old target and the chain of renames. A retry re-derives
/// everything from the file's text at that time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixIntent {
    pub file: String,
    pub old_target: String,
    pub chain: Vec<String>,
}

/// What came of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStatus {
    /// Rewritten on disk.
    Written,
    /// The current text holds no link to rewrite (none ever, or the user
    /// removed it); the file was not touched.
    NothingToWrite,
    /// The file is gone (deleted or renamed away); nothing was written.
    NotFound,
    /// Changed during every attempt, or a transient error each time:
    /// untouched, to be retried at the next sync. Carries the intent.
    Deferred,
    /// Not written and not going to be by the writer: a permanent
    /// failure, or the round cap was reached. Carries the intent.
    NeedsManualFix,
}

/// When the file will be tried again, as the writer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "when")]
pub enum RetryPlan {
    /// Nothing to retry.
    NoRetry,
    /// Retry after the next sync, after at least this long.
    AfterBackoff { after_ms: u64 },
    /// Never retried by the writer itself: only at startup, on
    /// `source.rescan` or when that file changes. Those triggers are
    /// wired outside this module.
    OnlyAtStartupRescanOrFileChange,
}

/// One file's result: everything the fix log and the retry list need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileWriteResult {
    pub file: String,
    pub status: WriteStatus,
    /// Link spans rewritten (`0` unless `Written`).
    pub links_changed: usize,
    /// Attempts that found the file changed before one that succeeded.
    pub recovered_edits: u32,
    /// Write attempts made in this call (1 for a permanent failure).
    pub attempts: u32,
    /// Rounds of this file that have ended in a transient failure,
    /// this call included.
    pub rounds: u32,
    /// The cap on `rounds` ([`MAX_ITEM_ROUNDS`]).
    pub round_cap: u32,
    pub failure_class: Option<FailureClass>,
    /// Why, for `Deferred` and `NeedsManualFix` (`"changed"` for a file
    /// that changed under every attempt).
    pub reason: Option<String>,
    pub retry: RetryPlan,
    /// The leftover work, for `Deferred` and `NeedsManualFix`.
    pub intent: Option<FixIntent>,
}

/// A batch of files: one file's failure never stops the others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BatchResult {
    pub files: Vec<FileWriteResult>,
}

impl BatchResult {
    pub fn with_status(&self, status: WriteStatus) -> Vec<&FileWriteResult> {
        self.files.iter().filter(|f| f.status == status).collect()
    }

    /// Some file was written and some other was left (deferred, needs a
    /// manual fix or gone).
    pub fn is_partial(&self) -> bool {
        let wrote = self.files.iter().any(|f| f.status == WriteStatus::Written);
        let left = self.files.iter().any(|f| {
            matches!(
                f.status,
                WriteStatus::Deferred | WriteStatus::NeedsManualFix | WriteStatus::NotFound
            )
        });
        wrote && left
    }
}

// ---------------------------------------------------------------
// The writer
// ---------------------------------------------------------------

enum Attempt {
    Written {
        links: usize,
    },
    NothingToWrite,
    NotFound,
    /// The file moved under this attempt (size or mtime).
    Changed,
    Permanent(String),
    Transient(String),
}

/// Writes link fixes into files, one at a time. Holds the filesystem
/// (so a test can intercept it), the sleeper and the shared gate.
pub struct FixWriter {
    fs: Arc<dyn AtomicFs>,
    sleeper: Arc<dyn Sleeper>,
    gate: Arc<Mutex<()>>,
    /// Whether a file written is recorded as Tephra's own write
    /// ([`FixWriter::record_own_writes`]).
    own_writes: AtomicBool,
}

impl FixWriter {
    /// The production wiring: the real filesystem and real sleeps, on
    /// `engine`'s own gate. Needs no stored preview from the engine.
    pub fn new(engine: &RefactorEngine) -> Self {
        Self::with_parts(
            engine.writer_gate(),
            Arc::new(RealFs),
            Arc::new(ThreadSleeper),
        )
    }

    /// [`FixWriter::new`] with the gate, filesystem and sleeper given.
    pub fn with_parts(
        gate: Arc<Mutex<()>>,
        fs: Arc<dyn AtomicFs>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        FixWriter {
            fs,
            sleeper,
            gate,
            own_writes: AtomicBool::new(false),
        }
    }

    /// With `on`, every file this writer writes is recorded as Tephra's
    /// own write ([`crate::dup_ids::note_self_write`], the path + size +
    /// mtime record the duplicate ID rename uses), so the watcher drops
    /// that file's event and the write never starts another batch or fix
    /// (E17). Only for a caller that indexes the written files itself:
    /// the record makes the watcher skip them. A write that fails is
    /// never recorded, so no stale record can hide a later real edit.
    pub fn record_own_writes(&self, on: bool) {
        self.own_writes.store(on, Ordering::SeqCst);
    }

    /// Every request in order. A permanent failure in one leaves the
    /// others written; the result says which is which.
    pub fn write_batch(&self, requests: &[FileFixRequest]) -> BatchResult {
        BatchResult {
            files: requests.iter().map(|r| self.write_file(r)).collect(),
        }
    }

    /// Rewrites the links in one file: up to [`MAX_WRITE_ATTEMPTS`]
    /// attempts (see the module doc).
    pub fn write_file(&self, req: &FileFixRequest) -> FileWriteResult {
        let mut changed_attempts = 0u32;
        let mut last_transient: Option<String> = None;
        let mut attempts = 0u32;
        for n in 1..=MAX_WRITE_ATTEMPTS {
            attempts = n;
            match self.attempt(req) {
                Attempt::Written { links } => {
                    if self.own_writes.load(Ordering::SeqCst) {
                        crate::dup_ids::note_self_write(&req.abs_path);
                    }
                    log_recovered(req, changed_attempts, attempts);
                    return result(
                        req,
                        WriteStatus::Written,
                        links,
                        changed_attempts,
                        attempts,
                        req.prior_rounds,
                    );
                }
                Attempt::NothingToWrite => {
                    log_recovered(req, changed_attempts, attempts);
                    return result(
                        req,
                        WriteStatus::NothingToWrite,
                        0,
                        changed_attempts,
                        attempts,
                        req.prior_rounds,
                    );
                }
                Attempt::NotFound => {
                    return result(req, WriteStatus::NotFound, 0, 0, attempts, req.prior_rounds);
                }
                Attempt::Permanent(reason) => {
                    tracing::warn!(
                        file = %req.file,
                        reason = %reason,
                        "fix write needs a manual fix: permanent failure, not retried by the writer"
                    );
                    let mut r = result(
                        req,
                        WriteStatus::NeedsManualFix,
                        0,
                        0,
                        attempts,
                        req.prior_rounds,
                    );
                    r.failure_class = Some(FailureClass::Permanent);
                    r.reason = Some(reason);
                    r.retry = RetryPlan::OnlyAtStartupRescanOrFileChange;
                    r.intent = Some(req.intent());
                    return r;
                }
                Attempt::Changed => {
                    changed_attempts += 1;
                    last_transient = Some(REASON_CHANGED.to_string());
                }
                Attempt::Transient(reason) => {
                    last_transient = Some(reason);
                }
            }
            if n < MAX_WRITE_ATTEMPTS {
                self.sleeper.sleep(write_backoff(n));
            }
        }

        // Every attempt failed in a way that may pass.
        let reason = last_transient.unwrap_or_else(|| REASON_CHANGED.to_string());
        let rounds = req.prior_rounds.saturating_add(1);
        let mut r = result(req, WriteStatus::Deferred, 0, 0, attempts, rounds);
        r.failure_class = Some(FailureClass::Transient);
        r.intent = Some(req.intent());
        if rounds >= MAX_ITEM_ROUNDS {
            tracing::warn!(
                file = %req.file,
                rounds,
                reason = %reason,
                "fix write needs a manual fix: gave up after the round cap"
            );
            r.status = WriteStatus::NeedsManualFix;
            r.reason = Some(format!("gave up after {rounds} rounds: {reason}"));
            r.retry = RetryPlan::OnlyAtStartupRescanOrFileChange;
        } else {
            r.reason = Some(reason);
            r.retry = RetryPlan::AfterBackoff {
                after_ms: item_backoff(rounds).as_millis() as u64,
            };
        }
        r
    }

    /// One attempt, under the gate and the file's lock.
    fn attempt(&self, req: &FileFixRequest) -> Attempt {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        let _file_lock = file_lock::lock(&req.abs_path);
        let abs = req.abs_path.as_path();

        // 1. The entry itself (a symlink is not followed).
        let meta = match fs::symlink_metadata(abs) {
            Ok(m) => m,
            Err(e) => return from_io(&e, "cannot look at the file"),
        };
        if meta.file_type().is_symlink() {
            let e = io::Error::new(
                io::ErrorKind::Unsupported,
                "it is a symlink, and rewriting it atomically would replace the link itself with a plain file",
            );
            return from_io(&e, "refusing to rewrite");
        }
        if !meta.is_file() {
            return Attempt::Permanent("not a regular file".to_string());
        }
        if is_immutable(&meta) {
            let e = io::Error::from_raw_os_error(libc::EPERM);
            return from_io(&e, "the file has the immutable flag set");
        }

        // 2. Fingerprint, read, fingerprint.
        let before = match self.fs.fingerprint(abs) {
            Ok(f) => f,
            Err(e) => return from_io(&e, "cannot look at the file"),
        };
        let bytes = match fs::read(abs) {
            Ok(b) => b,
            Err(e) => return from_io(&e, "cannot read the file"),
        };
        match self.fs.fingerprint(abs) {
            Ok(f) if f == before => {}
            Ok(_) => return Attempt::Changed,
            Err(e) => return from_io(&e, "cannot look at the file"),
        }
        let content = match String::from_utf8(bytes) {
            Ok(c) => c,
            Err(_) => {
                return Attempt::Permanent(
                    "the file is not UTF-8 text, so its links are not rewritten".to_string(),
                )
            }
        };

        // 3. The rewrite, from the text just read.
        let contents = HashMap::from([(req.file.clone(), content.clone())]);
        let changes: Vec<LinkChange> = match &req.mode {
            RewriteMode::Fix { bare_resolves } => compute_fix_changes(
                &req.edges,
                &contents,
                &req.old_path,
                &req.new_path,
                *bare_resolves,
            ),
            RewriteMode::Manual => {
                compute_changes(&req.edges, &contents, &req.old_path, &req.new_path)
            }
            RewriteMode::Move(rewrite) => compute_move_changes(&req.edges, &contents, rewrite),
        };
        if changes.is_empty() {
            return Attempt::NothingToWrite;
        }

        // 4. A file whose owner-write bit is off is never rewritten,
        // and no temp file exists yet.
        if owner_write_off(&meta) {
            return Attempt::Permanent(
                "the file is read-only, so its links are not rewritten".to_string(),
            );
        }
        let refs: Vec<&LinkChange> = changes.iter().collect();
        let new_content = match apply_changes_to_content(&content, &refs) {
            Ok(c) => c,
            Err(e) => return Attempt::Transient(e),
        };

        // 5-7. Temp file, fsync, re-check, rename.
        match self.replace(abs, &meta, new_content.as_bytes(), before) {
            Ok(Replaced::Done) => Attempt::Written {
                links: changes.len(),
            },
            Ok(Replaced::Changed) => Attempt::Changed,
            Err(e) => from_io(&e, "could not write the file"),
        }
    }

    /// Steps 5 to 7. The temp file is removed on every path but the
    /// successful rename.
    fn replace(
        &self,
        target: &Path,
        meta: &fs::Metadata,
        content: &[u8],
        before: (u64, Option<std::time::SystemTime>),
    ) -> io::Result<Replaced> {
        let dir = target.parent().unwrap_or_else(|| Path::new("."));
        let file_name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        // `.<name>.tephra-tmp-<digits>`: the form the source scan sweeps.
        let tmp = dir.join(format!(".{file_name}.tephra-tmp-{}", rand::random::<u32>()));

        let prepared = (|| -> io::Result<()> {
            self.fs.write_new(&tmp, content)?;
            self.fs.set_permissions(&tmp, meta.permissions())?;
            self.fs.sync(&tmp)
        })();
        if let Err(e) = prepared {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }

        // Size AND mtime, just before the rename.
        match self.fs.fingerprint(target) {
            Ok(now) if now == before => {}
            Ok(_) => {
                let _ = fs::remove_file(&tmp);
                return Ok(Replaced::Changed);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }

        match self.fs.rename(&tmp, target) {
            Ok(()) => Ok(Replaced::Done),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

enum Replaced {
    Done,
    Changed,
}

fn from_io(e: &io::Error, what: &str) -> Attempt {
    if e.kind() == io::ErrorKind::NotFound {
        return Attempt::NotFound;
    }
    let text = format!("{what}: {e}");
    match classify_io_error(e) {
        FailureClass::Permanent => Attempt::Permanent(text),
        FailureClass::Transient => Attempt::Transient(text),
    }
}

fn result(
    req: &FileFixRequest,
    status: WriteStatus,
    links_changed: usize,
    recovered_edits: u32,
    attempts: u32,
    rounds: u32,
) -> FileWriteResult {
    FileWriteResult {
        file: req.file.clone(),
        status,
        links_changed,
        recovered_edits,
        attempts,
        rounds,
        round_cap: MAX_ITEM_ROUNDS,
        failure_class: None,
        reason: None,
        retry: RetryPlan::NoRetry,
        intent: None,
    }
}

/// One `tracing` line per recovered edit, for the fix log (PR-5).
fn log_recovered(req: &FileFixRequest, recovered: u32, attempts: u32) {
    for edit in 1..=recovered {
        tracing::info!(
            file = %req.file,
            edit,
            attempts,
            "fix write recovered an edit: the file changed under an attempt, then the rewrite was derived again from the newer text"
        );
    }
}

/// Whether the file's owner-write bit is off. On Unix that is
/// `mode & 0o200 == 0` (`Permissions::readonly` tests only that no write
/// bit at all is set); elsewhere the platform's read-only attribute.
fn owner_write_off(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o200 == 0
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly()
    }
}

/// macOS `chflags uchg` / `schg`: renaming over such a file fails with
/// `EPERM`, but it is cheaper and cleaner to see it before a temp file
/// is made. Other systems learn of it from the `EPERM` itself.
fn is_immutable(meta: &fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const UF_IMMUTABLE: u32 = 0x0000_0002;
        const SF_IMMUTABLE: u32 = 0x0002_0000;
        meta.st_flags() & (UF_IMMUTABLE | SF_IMMUTABLE) != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = meta;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::SystemTime;

    // --- failure classes ------------------------------------------------

    fn kind(k: io::ErrorKind) -> io::Error {
        io::Error::new(k, "injected")
    }

    fn os(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn every_g23_failure_is_sorted_into_its_class() {
        use FailureClass::{Permanent, Transient};
        let table: Vec<(&str, io::Error, FailureClass)> = vec![
            ("immutable flag (EPERM)", os(libc::EPERM), Permanent),
            ("ACL deny / EACCES", os(libc::EACCES), Permanent),
            (
                "PermissionDenied kind",
                kind(io::ErrorKind::PermissionDenied),
                Permanent,
            ),
            (
                "unwritable directory (EACCES on create)",
                os(libc::EACCES),
                Permanent,
            ),
            (
                "symlink refusal (Unsupported)",
                kind(io::ErrorKind::Unsupported),
                Permanent,
            ),
            ("read-only file system (EROFS)", os(libc::EROFS), Permanent),
            ("is a directory", os(libc::EISDIR), Permanent),
            ("WouldBlock", kind(io::ErrorKind::WouldBlock), Transient),
            ("TimedOut", kind(io::ErrorKind::TimedOut), Transient),
            ("Interrupted", kind(io::ErrorKind::Interrupted), Transient),
            ("EAGAIN", os(libc::EAGAIN), Transient),
            ("locked (EBUSY)", os(libc::EBUSY), Transient),
            ("text file busy (ETXTBSY)", os(libc::ETXTBSY), Transient),
            ("cloud placeholder (EDEADLK)", os(libc::EDEADLK), Transient),
            ("ETIMEDOUT", os(libc::ETIMEDOUT), Transient),
            (
                "anything unrecognised",
                kind(io::ErrorKind::Other),
                Transient,
            ),
        ];
        for (name, err, want) in table {
            assert_eq!(classify_io_error(&err), want, "{name}: {err}");
        }
    }

    // --- backoff bounds -------------------------------------------------

    #[test]
    fn backoffs_double_and_never_pass_their_caps() {
        assert_eq!(write_backoff(1), WRITE_BACKOFF_BASE);
        assert_eq!(write_backoff(2), WRITE_BACKOFF_BASE * 2);
        assert_eq!(write_backoff(40), WRITE_BACKOFF_MAX);
        assert_eq!(write_backoff(u32::MAX), WRITE_BACKOFF_MAX);
        assert_eq!(item_backoff(1), ITEM_BACKOFF_BASE);
        assert_eq!(item_backoff(2), ITEM_BACKOFF_BASE * 2);
        assert_eq!(item_backoff(u32::MAX), ITEM_BACKOFF_MAX);
        let mut last = Duration::ZERO;
        for n in 1..=MAX_ITEM_ROUNDS + 3 {
            let d = item_backoff(n);
            assert!(d >= last && d <= ITEM_BACKOFF_MAX, "round {n}: {d:?}");
            last = d;
        }
    }

    // --- retry state with a fake sleeper and a file that always moves ---

    #[derive(Default)]
    struct CountingSleeper {
        slept: StdMutex<Vec<Duration>>,
    }

    impl Sleeper for CountingSleeper {
        fn sleep(&self, d: Duration) {
            self.slept.lock().unwrap().push(d);
        }
    }

    /// The real filesystem, except that the file's mtime moves after
    /// every fsync, so the re-check before the rename always fails.
    struct AlwaysMoves {
        syncs: AtomicU32,
    }

    impl AtomicFs for AlwaysMoves {
        fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
            RealFs.write_new(path, content)
        }
        fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
            RealFs.set_permissions(path, perms)
        }
        fn sync(&self, path: &Path) -> io::Result<()> {
            let n = self.syncs.fetch_add(1, Ordering::SeqCst) + 1;
            RealFs.sync(path)?;
            // The temp file is `.<name>.tephra-tmp-N` beside the target.
            let name = path.file_name().unwrap().to_str().unwrap();
            let target = path.with_file_name(
                name.trim_start_matches('.')
                    .split(".tephra-tmp-")
                    .next()
                    .unwrap(),
            );
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_system_time(
                    SystemTime::now() + Duration::from_secs(10 + u64::from(n)),
                ),
            )
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            RealFs.rename(from, to)
        }
    }

    fn edge(from: &str, to: &str, raw: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: tephra_graph::EdgeTarget::Resolved(to.to_string()),
            to_raw: raw.to_string(),
            kind: tephra_parser::LINK_KIND_WIKILINK.to_string(),
            offline: false,
            line: None,
        }
    }

    fn request(dir: &Path, prior_rounds: u32) -> FileFixRequest {
        let path = dir.join("Weekly Review.md");
        let mut r = FileFixRequest::new(
            "@notes:Weekly Review.md",
            path,
            vec![edge(
                "@notes:Weekly Review.md",
                "@notes:Plan.md",
                "Training Plan",
            )],
            "Training Plan.md",
            "Plan.md",
            RewriteMode::Fix {
                bare_resolves: true,
            },
        );
        r.prior_rounds = prior_rounds;
        r
    }

    #[test]
    fn a_file_that_moves_every_attempt_is_deferred_then_needs_a_manual_fix_at_the_round_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Weekly Review.md");
        let original = "See [[Training Plan]].\n";
        fs::write(&path, original).unwrap();

        let sleeper = Arc::new(CountingSleeper::default());
        let writer = FixWriter::with_parts(
            Arc::new(Mutex::new(())),
            Arc::new(AlwaysMoves {
                syncs: AtomicU32::new(0),
            }),
            sleeper.clone(),
        );

        let mut rounds = 0;
        let mut last = None;
        for round in 1..=MAX_ITEM_ROUNDS {
            let r = writer.write_file(&request(dir.path(), rounds));
            assert_eq!(r.attempts, MAX_WRITE_ATTEMPTS, "round {round}");
            assert_eq!(r.round_cap, MAX_ITEM_ROUNDS);
            assert_eq!(r.rounds, round);
            assert_eq!(r.recovered_edits, 0);
            assert_eq!(r.failure_class, Some(FailureClass::Transient));
            assert_eq!(
                r.intent,
                Some(FixIntent {
                    file: "@notes:Weekly Review.md".into(),
                    old_target: "Training Plan.md".into(),
                    chain: vec!["Training Plan.md".into(), "Plan.md".into()],
                })
            );
            if round < MAX_ITEM_ROUNDS {
                assert_eq!(r.status, WriteStatus::Deferred, "round {round}");
                assert_eq!(r.reason.as_deref(), Some(REASON_CHANGED));
                assert_eq!(
                    r.retry,
                    RetryPlan::AfterBackoff {
                        after_ms: item_backoff(round).as_millis() as u64
                    }
                );
            } else {
                assert_eq!(r.status, WriteStatus::NeedsManualFix);
                assert_eq!(r.retry, RetryPlan::OnlyAtStartupRescanOrFileChange);
            }
            rounds = r.rounds;
            last = Some(r);
        }
        assert_eq!(last.unwrap().status, WriteStatus::NeedsManualFix);

        // Two waits between three attempts, none after the last, each
        // from the named constants.
        let slept = sleeper.slept.lock().unwrap().clone();
        let per_call = vec![write_backoff(1), write_backoff(2)];
        let expected: Vec<Duration> = (0..MAX_ITEM_ROUNDS)
            .flat_map(|_| per_call.clone())
            .collect();
        assert_eq!(slept, expected);
        assert!(slept.iter().all(|d| *d <= WRITE_BACKOFF_MAX));
        // The file was never written and nothing is left beside it.
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["Weekly Review.md".to_string()]);
    }

    #[test]
    fn a_permanent_failure_takes_one_attempt_and_sleeps_never() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Weekly Review.md");
        fs::write(&path, "See [[Training Plan]].\n").unwrap();
        // Owner-write off: refused before any temp file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        }
        let sleeper = Arc::new(CountingSleeper::default());
        let writer =
            FixWriter::with_parts(Arc::new(Mutex::new(())), Arc::new(RealFs), sleeper.clone());
        let r = writer.write_file(&request(dir.path(), 0));
        assert_eq!(r.status, WriteStatus::NeedsManualFix);
        assert_eq!(r.attempts, 1);
        assert_eq!(r.rounds, 0);
        assert_eq!(r.failure_class, Some(FailureClass::Permanent));
        assert_eq!(r.retry, RetryPlan::OnlyAtStartupRescanOrFileChange);
        assert!(sleeper.slept.lock().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "See [[Training Plan]].\n"
        );
    }
}
