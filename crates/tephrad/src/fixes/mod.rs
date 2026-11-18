//! The fix log and the pending-fixes engine (G9, G10): what the
//! automatic link fix records, what it keeps for later, and when it
//! tries again.
//!
//! The one thing that touches a linking file is PR-4's write loop
//! ([`crate::refactor::fix_write::FixWriter`], behind [`FileWriter`]).
//! This module decides which files go to it, when, and what the log
//! says came of it:
//!
//! - [`FixEngine::begin_fix`] logs one entry for one rename and stores
//!   every linking file in `pending_fixes` in the same transaction,
//!   BEFORE any file is written. Then it works through the list: each
//!   file's outcome (the row deleted or kept, the entry's per-file
//!   result and status updated) is one transaction too. A crash leaves
//!   a row for every file not yet recorded, and the one entry.
//! - [`FixEngine::resume`] (startup), [`FixEngine::after_rescan`] and
//!   [`FixEngine::after_batch`] pick the rows up again ([`retry`]):
//!   each is re-checked against the graph and the log first
//!   ([`pending`]), then handed to the write loop. A retry that
//!   succeeds updates the entry it belongs to; it never adds one.
//! - Nothing is stored as a recorded edit: a row is the file, the old
//!   target and the new one, and the retry re-derives the edit from the
//!   file's text as it is then.
//!
//! The log is a record only (F9): there is no undo or replay here.

pub mod event;
pub mod log;
pub mod pending;
pub mod retry;
pub mod state;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tephra_cache::{
    Cache, FixLogChange, NewFixLog, NewPendingFix, PendingFixRow, PendingOp, SourceStatus,
    STATE_NEEDS_MANUAL_FIX, STATE_PENDING,
};
use tephra_graph::{Edge, UnitGraph};

use crate::refactor::fix_write::{
    FileFixRequest, FileWriteResult, FixWriter, RewriteMode, WriteStatus,
};
use crate::refactor::{MoveRewrite, OwnLink};

pub use log::{FixDetail, FixEntryWire, FixNotice};
pub use pending::Verdict;
pub use retry::Trigger;

use log::{
    overall_status, FileEntry, FILE_NEEDS_MANUAL_FIX, FILE_NOTHING_TO_FIX, FILE_PENDING,
    FILE_WRITTEN, KIND_MOVE, KIND_RENAME, KIND_UNCERTAIN, STATUS_UNCERTAIN,
};

/// What an uncertain entry says when the watcher logs it.
const MSG_UNCERTAIN: &str = "vanished, no certain pair: links broken";

/// How long a second report of the same vanished file can be the first one
/// again.
pub const UNCERTAIN_DEDUPE_SECS: i64 = 120;

/// What makes a second "vanished" entry for one path, inside
/// [`UNCERTAIN_DEDUPE_SECS`], the same vanishing seen twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dedupe {
    /// Nothing: every call logs an entry.
    Never,
    /// Any recent entry for the path.
    AnyRecent,
    /// A recent entry an exact event logged as "moved outside sources".
    AfterOutsideEvent,
}

// ---------------------------------------------------------------
// Errors, seams
// ---------------------------------------------------------------

/// Why an engine call failed. A file that could not be fixed is not an
/// error: it is a result in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixError {
    /// The cache could not be read or written.
    Cache(String),
    /// The call cannot be answered at all (no such unit, a rename that
    /// has not been indexed).
    Refused(String),
    /// A test hook stopped the run after a set number of writes,
    /// leaving the cache and the disk exactly as a crash would.
    Stopped,
}

impl fmt::Display for FixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixError::Cache(m) | FixError::Refused(m) => write!(f, "{m}"),
            FixError::Stopped => {
                write!(f, "stopped by the test hook after the set number of writes")
            }
        }
    }
}

impl std::error::Error for FixError {}

impl From<tephra_cache::CacheError> for FixError {
    fn from(e: tephra_cache::CacheError) -> Self {
        FixError::Cache(e.to_string())
    }
}

impl From<tephra_graph::GraphError> for FixError {
    fn from(e: tephra_graph::GraphError) -> Self {
        FixError::Cache(e.to_string())
    }
}

/// The write loop, behind a trait so the retry rules can be tested with
/// its results given directly (no filesystem flags needed).
pub trait FileWriter: Send + Sync {
    fn write_file(&self, req: &FileFixRequest) -> FileWriteResult;

    /// Asks the writer to record each file it writes as Tephra's own
    /// write ([`crate::dup_ids::note_self_write`]), so the watcher drops
    /// the file's event. Called by [`FixEngine::set_sink`]: the sink is
    /// what indexes the written files again, so the watcher need not.
    /// A writer that has no such record ignores it.
    fn record_own_writes(&self, _on: bool) {}
}

impl FileWriter for FixWriter {
    fn write_file(&self, req: &FileFixRequest) -> FileWriteResult {
        FixWriter::write_file(self, req)
    }

    fn record_own_writes(&self, on: bool) {
        FixWriter::record_own_writes(self, on)
    }
}

// ---------------------------------------------------------------
// What a fix tells the clients
// ---------------------------------------------------------------

/// A linking file a fix rewrote: the source it is in and where it is on
/// disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenFile {
    pub source: String,
    pub path: PathBuf,
}

/// What one batch of fixes has to announce (G17): the entries of the
/// `fixes` list, and every file rewritten (which has to be indexed again,
/// and is what the push's `paths` lists). Several renames fixed together
/// are merged into one, so they make one push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixAnnouncement {
    pub notices: Vec<FixNotice>,
    pub written: Vec<WrittenFile>,
}

impl FixAnnouncement {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.written.is_empty()
    }

    /// Adds `other`: its entries after this one's, its files the ones
    /// this one does not hold yet.
    pub fn merge(&mut self, other: FixAnnouncement) {
        self.notices.extend(other.notices);
        for file in other.written {
            if !self.written.contains(&file) {
                self.written.push(file);
            }
        }
    }
}

/// Where the engine hands what it announces when no caller collects it:
/// a retry (startup, `source.rescan`, a watcher batch) has no caller to
/// return it to. The daemon's implementation is
/// [`crate::registry::Announcer`]: it indexes the written files, then
/// sends one event, which the forwarder turns into the `graph:changed`
/// push after refreshing the in-memory graph.
///
/// The engine never waits for a client: it hands the announcement over
/// and goes on (F13).
pub trait FixSink: Send + Sync {
    fn announce(&self, announcement: FixAnnouncement);
}

/// The time, in epoch seconds, behind a trait so a test can move it.
pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// A clock a test sets by hand.
#[derive(Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(now: i64) -> Self {
        ManualClock(AtomicI64::new(now))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------
// Requests and summaries
// ---------------------------------------------------------------

/// A rename to fix, and the linking files to fix it in. Callers that
/// have only the rename use [`FixEngine::begin_rename`], which finds
/// the files in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixSpec {
    pub old_source: String,
    pub old_path: String,
    pub new_source: String,
    pub new_path: String,
    /// `(source, path)` of each linking file.
    pub files: Vec<(String, String)>,
}

/// What [`FixEngine::begin_plan`] stores: the entry's own old and new
/// path (a file's, or a folder's), and the work, one row per linking
/// file per moved file.
struct Plan {
    /// [`KIND_RENAME`], or [`KIND_MOVE`] for a file that went to another
    /// source.
    kind: &'static str,
    old_source: String,
    new_source: String,
    entry_old: String,
    entry_new: String,
    rows: Vec<PlanRow>,
}

/// One pending row: `file` is `(source, path)` of the linking file, and
/// `old` to `new` the rename it is to be fixed for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PlanRow {
    file: (String, String),
    old: String,
    new: String,
}

/// What one pass over the pending rows did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub written: usize,
    /// Rows finished without a write (nothing to fix, or cancelled).
    pub dropped: usize,
    /// Rows left waiting for a later trigger (a transient failure).
    pub deferred: usize,
    pub needs_manual_fix: usize,
    /// Rows left alone because their source is offline.
    pub skipped_offline: usize,
}

/// What one run of the write loop leaves for an announcement.
#[derive(Default)]
struct Report {
    /// Each entry the run worked on, as it stood before.
    before: HashMap<i64, FixNotice>,
    written: Vec<WrittenFile>,
}

impl Report {
    /// The announcement for this run. `fresh` are the entries the run
    /// itself began: one is announced when it wrote a file or left one
    /// behind (a rename with nothing to rewrite is no news). Any other
    /// entry (a retry's) is announced when the run changed it.
    fn announcement(&self, cache: &Cache, fresh: &[i64]) -> Result<FixAnnouncement, FixError> {
        let mut ids: Vec<i64> = self.before.keys().copied().collect();
        ids.sort_unstable();
        let mut notices = Vec::new();
        for id in ids {
            let Some(row) = cache.get_fix_log(id)? else {
                continue;
            };
            let now = FixNotice::from_row(&row);
            let news = if fresh.contains(&id) {
                now.files_changed > 0 || now.deferred > 0
            } else {
                self.before.get(&id) != Some(&now)
            };
            if news {
                notices.push(now);
            }
        }
        Ok(FixAnnouncement {
            notices,
            written: self.written.clone(),
        })
    }
}

// ---------------------------------------------------------------
// The engine
// ---------------------------------------------------------------

/// The fix log and the pending list over one cache file. One call at a
/// time: the engine's own lock serializes them, so a watcher batch and
/// a `source.rescan` never retry the same row twice at once.
pub struct FixEngine {
    cache_path: PathBuf,
    writer: Arc<dyn FileWriter>,
    clock: Arc<dyn Clock>,
    stop_after_writes: Option<usize>,
    run_lock: Mutex<()>,
    /// Held from the question "is this rename in the log already?" to
    /// the entry that answers it (G4): the exact event and the watcher's
    /// pairing of one rename, arriving together, never both start a fix.
    /// Always taken before `run_lock`, never after it.
    decision_lock: Mutex<()>,
    /// Where a retry's announcement goes ([`FixSink`]). Unset in tests
    /// that read the log only; then nothing is announced and the writer
    /// does not record its writes as its own.
    sink: RwLock<Option<Arc<dyn FixSink>>>,
    /// The announcement each thread inside [`RenameFixer::start_batch`]
    /// has so far: the renames of one watcher batch are fixed one after
    /// the other on one thread, and make one push.
    batches: Mutex<HashMap<std::thread::ThreadId, FixAnnouncement>>,
}

impl FixEngine {
    pub fn new(cache_path: impl Into<PathBuf>, writer: Arc<dyn FileWriter>) -> Self {
        FixEngine {
            cache_path: cache_path.into(),
            writer,
            clock: Arc::new(SystemClock),
            stop_after_writes: None,
            run_lock: Mutex::new(()),
            decision_lock: Mutex::new(()),
            sink: RwLock::new(None),
            batches: Mutex::new(HashMap::new()),
        }
    }

    /// Sets where announcements go, and has the writer record each file
    /// it writes as Tephra's own, so the watcher does not read that write
    /// as a change (E17): the sink indexes the written files itself.
    pub fn set_sink(&self, sink: Arc<dyn FixSink>) {
        self.writer.record_own_writes(true);
        *self.sink.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    /// What a fix started by the rename detector does with its
    /// announcement: adds it to this thread's batch if one is open, and
    /// otherwise announces it at once.
    fn announce_or_collect(&self, announcement: FixAnnouncement) {
        let mut batches = self.batches.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(open) = batches.get_mut(&std::thread::current().id()) {
            open.merge(announcement);
            return;
        }
        drop(batches);
        self.announce(announcement);
    }

    /// Hands `announcement` to the sink, if there is one. A retry calls
    /// this itself; a fix a caller starts returns its announcement
    /// instead, so the caller can merge it with the push it is already
    /// making.
    pub fn announce(&self, announcement: FixAnnouncement) {
        if announcement.is_empty() {
            return;
        }
        let sink = self.sink.read().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(sink) = sink {
            sink.announce(announcement);
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Test hook: each call stops, with [`FixError::Stopped`], right
    /// after the `n`-th file it writes has been recorded; with `n` of 0
    /// it stops after the entry and its work list are stored and before
    /// the first file is written. Nothing is rolled back or finished:
    /// the cache and the disk are as a kill at that moment leaves them.
    pub fn with_stop_after_writes(mut self, n: usize) -> Self {
        self.stop_after_writes = Some(n);
        self
    }

    fn open(&self) -> Result<Cache, FixError> {
        Ok(Cache::open(&self.cache_path)?)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.run_lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    // -- the log ---------------------------------------------------

    /// The log, newest first. `since` is epoch seconds; `limit` is
    /// clamped to [`tephra_cache::MAX_LIST_LIMIT`].
    pub fn list(&self, since: Option<i64>, limit: usize) -> Result<Vec<FixEntryWire>, FixError> {
        let cache = self.open()?;
        Ok(cache
            .list_fix_log(since, limit)?
            .iter()
            .map(FixEntryWire::from)
            .collect())
    }

    /// Logs a file that vanished with no certain pair: its links are
    /// broken. No pending work is created for it (G10). Every call logs
    /// an entry.
    pub fn record_uncertain(&self, source: &str, path: &str) -> Result<FixEntryWire, FixError> {
        let _decision = self.decision_guard();
        self.record_uncertain_inner(source, path, MSG_UNCERTAIN, Dedupe::Never)
    }

    /// [`FixEngine::record_uncertain`] for a caller that holds the
    /// [`FixEngine::decision_guard`] already, with its own message and
    /// its own idea of what counts as the same vanishing seen twice.
    pub(crate) fn record_uncertain_inner(
        &self,
        source: &str,
        path: &str,
        message: &str,
        dedupe: Dedupe,
    ) -> Result<FixEntryWire, FixError> {
        let _guard = self.lock();
        let cache = self.open()?;
        let now = self.clock.now();
        let same = cache
            .fix_log_summaries(source)?
            .into_iter()
            .rev()
            .find(|e| {
                e.kind == KIND_UNCERTAIN
                    && e.old_source.as_deref() == Some(source)
                    && e.old_path.as_deref() == Some(path)
            })
            .filter(|e| now - e.created_at <= UNCERTAIN_DEDUPE_SECS);
        if let Some(existing) = same {
            let repeat = match dedupe {
                Dedupe::Never => false,
                // An exact event saying "moved outside" for a path that
                // already has a recent entry: the same vanishing.
                Dedupe::AnyRecent => true,
                // The watcher's report of a path an exact event already
                // logged as moved outside.
                Dedupe::AfterOutsideEvent => cache
                    .get_fix_log(existing.id)?
                    .map(|row| {
                        FixDetail::parse(row.detail_json.as_deref())
                            .message
                            .as_deref()
                            == Some(event::MSG_MOVED_OUTSIDE)
                    })
                    .unwrap_or(false),
            };
            if repeat {
                return entry(&cache, existing.id);
            }
        }
        let detail = FixDetail {
            message: Some(message.to_string()),
            ..FixDetail::default()
        };
        let id = cache.insert_fix_log(&NewFixLog {
            created_at: now,
            kind: KIND_UNCERTAIN.to_string(),
            status: STATUS_UNCERTAIN.to_string(),
            old_source: Some(source.to_string()),
            old_path: Some(path.to_string()),
            new_source: None,
            new_path: None,
            files_changed: 0,
            detail_json: Some(detail.to_json()),
        })?;
        entry(&cache, id)
    }

    /// Takes the lock that makes "is this rename in the log?" and the
    /// entry that answers it one step ([`FixEngine::decision_lock`]).
    pub(crate) fn decision_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.decision_lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    // -- starting a fix ---------------------------------------------

    /// Fixes the links to `old_path` after it was renamed to
    /// `new_path` inside `source`: finds the linking files in the graph
    /// (as it is after the rename was indexed) and calls
    /// [`FixEngine::begin_fix`].
    pub fn begin_rename(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<FixEntryWire, FixError> {
        self.begin_rename_announced(source, old_path, new_path)
            .map(|(entry, _)| entry)
    }

    /// [`FixEngine::begin_rename`], and what the fix has to announce.
    pub fn begin_rename_announced(
        &self,
        source: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<(FixEntryWire, FixAnnouncement), FixError> {
        let files = {
            let cache = self.open()?;
            let graph = UnitGraph::build_from_cache(&cache)?;
            linking_files(&graph, source, old_path, source, new_path)?
        };
        self.begin_fix_announced(&FixSpec {
            old_source: source.to_string(),
            old_path: old_path.to_string(),
            new_source: source.to_string(),
            new_path: new_path.to_string(),
            files,
        })
    }

    /// Fixes the links to every file of a folder renamed from `old_dir`
    /// to `new_dir` inside `source` (G1): `moves` are the `(old, new)`
    /// paths of the files that moved. ONE entry is logged, with the
    /// folder's own old and new path; each linking file is stored once
    /// per moved file it links to, each row naming that file's old and
    /// new path, because the write loop fixes one rename at a time.
    /// `files_changed` counts the linking files written, not the files
    /// that moved. A moved file that is not a unit, or that nothing
    /// links to, adds no work.
    pub fn begin_folder_rename(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<FixEntryWire, FixError> {
        self.begin_folder_rename_announced(source, old_dir, new_dir, moves)
            .map(|(entry, _)| entry)
    }

    /// [`FixEngine::begin_folder_rename`], and what the fix has to
    /// announce.
    pub fn begin_folder_rename_announced(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<(FixEntryWire, FixAnnouncement), FixError> {
        let rows = {
            let cache = self.open()?;
            let graph = UnitGraph::build_from_cache(&cache)?;
            let mut rows: BTreeSet<PlanRow> = BTreeSet::new();
            for (old, new) in moves {
                // A moved file that cannot be fixed (not a unit now, or
                // its old path is one again) is left out; the rest
                // still are.
                let Ok(files) = linking_files(&graph, source, old, source, new) else {
                    continue;
                };
                for file in files {
                    rows.insert(PlanRow {
                        file,
                        old: old.clone(),
                        new: new.clone(),
                    });
                }
            }
            rows.into_iter().collect::<Vec<_>>()
        };
        self.begin_plan(&Plan {
            kind: KIND_RENAME,
            old_source: source.to_string(),
            new_source: source.to_string(),
            entry_old: old_dir.to_string(),
            entry_new: new_dir.to_string(),
            rows,
        })
    }

    /// Fixes the links of a file that went from `old_source` to
    /// `new_source` (G6), as ONE entry of kind `move`: the links in the
    /// old source (rewrite 1), the links in the new one (rewrite 2) and
    /// the moved file's own links (rewrite 3). Every file is stored in
    /// the pending list in one transaction before the first write.
    /// `Refused` when `new_path` is not a unit of `new_source`, or the
    /// old path still is one of `old_source` (the move was not indexed).
    pub fn begin_move_announced(
        &self,
        old_source: &str,
        old_path: &str,
        new_source: &str,
        new_path: &str,
    ) -> Result<(FixEntryWire, FixAnnouncement), FixError> {
        let rows = {
            let cache = self.open()?;
            let graph = UnitGraph::build_from_cache(&cache)?;
            let files = linking_files(&graph, old_source, old_path, new_source, new_path)?;
            let mut rows: BTreeSet<PlanRow> = files
                .into_iter()
                .map(|file| PlanRow {
                    file,
                    old: old_path.to_string(),
                    new: new_path.to_string(),
                })
                .collect();
            // The moved file's own links, when it has any that change.
            let own = own_links(&graph, old_source, old_path, new_source, new_path);
            if !own.1.is_empty() {
                rows.insert(PlanRow {
                    file: (new_source.to_string(), new_path.to_string()),
                    old: old_path.to_string(),
                    new: new_path.to_string(),
                });
            }
            rows.into_iter().collect::<Vec<_>>()
        };
        self.begin_plan(&Plan {
            kind: KIND_MOVE,
            old_source: old_source.to_string(),
            new_source: new_source.to_string(),
            entry_old: old_path.to_string(),
            entry_new: new_path.to_string(),
            rows,
        })
    }

    /// Whether the log has this exact move already, and it still stands
    /// (the watcher's pairing and an exact event are two signals of one
    /// move, E7). It stands when no later `rename` or `move` entry took
    /// the file away from the new place again, or brought a file back to
    /// the old one: after `A -> B`, `B -> A`, a second `A -> B` is a new
    /// move to fix, not the first one seen twice (the log chain, as for a
    /// rename in `state::decide`).
    pub(crate) fn move_logged(
        &self,
        old_source: &str,
        old_path: &str,
        new_source: &str,
        new_path: &str,
    ) -> Result<bool, FixError> {
        let cache = self.open()?;
        // Both sources' entries, oldest first (id order); a move names
        // both, so a same-source pair is read twice and deduplicated.
        let mut log = cache.fix_log_summaries(old_source)?;
        if new_source != old_source {
            log.extend(cache.fix_log_summaries(new_source)?);
        }
        log.sort_by_key(|e| e.id);
        log.dedup_by_key(|e| e.id);

        let at = |source: &Option<String>, path: &Option<String>, want: (&str, &str)| {
            source.as_deref() == Some(want.0) && path.as_deref() == Some(want.1)
        };
        let Some(last) = log.iter().rev().find(|e| {
            e.kind == KIND_MOVE
                && at(&e.old_source, &e.old_path, (old_source, old_path))
                && at(&e.new_source, &e.new_path, (new_source, new_path))
        }) else {
            return Ok(false);
        };
        let undone = log.iter().any(|e| {
            e.id > last.id
                && (e.kind == KIND_MOVE || e.kind == KIND_RENAME)
                && (at(&e.old_source, &e.old_path, (new_source, new_path))
                    || at(&e.new_source, &e.new_path, (old_source, old_path)))
        });
        Ok(!undone)
    }

    /// Whether the log has `path` of `source` leaving it by a recent
    /// move to another source.
    pub(crate) fn move_departed(&self, source: &str, path: &str) -> Result<bool, FixError> {
        let cache = self.open()?;
        let now = self.clock.now();
        Ok(cache.fix_log_summaries(source)?.into_iter().any(|e| {
            e.kind == KIND_MOVE
                && e.old_source.as_deref() == Some(source)
                && e.old_path.as_deref() == Some(path)
                && now - e.created_at <= UNCERTAIN_DEDUPE_SECS
        }))
    }

    /// Logs ONE entry for the rename and stores every linking file in
    /// the pending list, in one transaction, before the first write.
    /// Then works through the list. Returns the entry as it stands at
    /// the end (`partial` while work is left).
    pub fn begin_fix(&self, spec: &FixSpec) -> Result<FixEntryWire, FixError> {
        self.begin_fix_announced(spec).map(|(entry, _)| entry)
    }

    /// [`FixEngine::begin_fix`], and what the fix has to announce.
    pub fn begin_fix_announced(
        &self,
        spec: &FixSpec,
    ) -> Result<(FixEntryWire, FixAnnouncement), FixError> {
        let files: BTreeSet<&(String, String)> = spec.files.iter().collect();
        self.begin_plan(&Plan {
            kind: if spec.old_source == spec.new_source {
                KIND_RENAME
            } else {
                KIND_MOVE
            },
            old_source: spec.old_source.clone(),
            new_source: spec.new_source.clone(),
            entry_old: spec.old_path.clone(),
            entry_new: spec.new_path.clone(),
            rows: files
                .into_iter()
                .map(|file| PlanRow {
                    file: file.clone(),
                    old: spec.old_path.clone(),
                    new: spec.new_path.clone(),
                })
                .collect(),
        })
    }

    /// The body of [`FixEngine::begin_fix_announced`] and
    /// [`FixEngine::begin_folder_rename_announced`]: the entry and the
    /// work list in one transaction, then the work. The log entry is
    /// complete before this returns, so whoever announces it does so
    /// after the entry is in `fix.list`.
    fn begin_plan(&self, plan: &Plan) -> Result<(FixEntryWire, FixAnnouncement), FixError> {
        let _guard = self.lock();
        let cache = self.open()?;
        let now = self.clock.now();
        // One entry per linking file, however many moved files it links
        // to.
        let files: BTreeSet<&(String, String)> = plan.rows.iter().map(|r| &r.file).collect();
        let detail = FixDetail {
            files: files
                .iter()
                .map(|(s, p)| FileEntry::pending(s, p))
                .collect(),
            ..FixDetail::default()
        };
        let pending: Vec<NewPendingFix> = plan
            .rows
            .iter()
            .map(|r| NewPendingFix {
                source: r.file.0.clone(),
                file_path: r.file.1.clone(),
                old_source: Some(plan.old_source.clone()),
                old_path: Some(r.old.clone()),
                new_source: Some(plan.new_source.clone()),
                new_path: Some(r.new.clone()),
            })
            .collect();
        let (fix_id, _) = cache.start_fix(
            &NewFixLog {
                created_at: now,
                kind: plan.kind.to_string(),
                status: overall_status(&detail.files).to_string(),
                old_source: Some(plan.old_source.clone()),
                old_path: Some(plan.entry_old.clone()),
                new_source: Some(plan.new_source.clone()),
                new_path: Some(plan.entry_new.clone()),
                files_changed: 0,
                detail_json: Some(detail.to_json()),
            },
            &pending,
        )?;
        let rows = cache.pending_fixes(None, Some(fix_id))?;
        let picks = rows
            .into_iter()
            .map(|row| retry::Pick {
                row,
                prior_rounds: 0,
            })
            .collect();
        let mut report = Report::default();
        let run = self.run_picks(&cache, picks, &mut report);
        let announcement = report.announcement(&cache, &[fix_id]);
        if let Err(e) = run {
            // A caller that gets an error has no announcement to merge
            // into its own: whatever was written is announced now, so
            // the files are indexed again.
            if let Ok(a) = announcement {
                self.announce(a);
            }
            return Err(e);
        }
        Ok((entry(&cache, fix_id)?, announcement?))
    }

    // -- retrying ---------------------------------------------------

    /// At start: every pending row of every source (G9).
    pub fn resume(&self) -> Result<RunSummary, FixError> {
        self.retry(&Trigger::Startup)
    }

    /// After `source.rescan` of `source`: every pending row of it.
    pub fn after_rescan(&self, source: &str) -> Result<RunSummary, FixError> {
        self.retry(&Trigger::Rescan {
            source: source.to_string(),
        })
    }

    /// After a watcher batch in `source` that changed `changed` (paths
    /// on disk): the rows whose backoff has passed, and the rows that
    /// need a person whose file is in the batch.
    pub fn after_batch(&self, source: &str, changed: &[PathBuf]) -> Result<RunSummary, FixError> {
        let rel = {
            let cache = self.open()?;
            relative_paths(&cache, source, changed)?
        };
        self.retry(&Trigger::Batch {
            source: source.to_string(),
            changed: rel,
        })
    }

    /// One pass for `trigger`.
    pub fn retry(&self, trigger: &Trigger) -> Result<RunSummary, FixError> {
        let _guard = self.lock();
        let cache = self.open()?;
        let rows = cache.pending_fixes(None, None)?;
        let picks = retry::select(rows, trigger, self.clock.now());
        let mut report = Report::default();
        let run = self.run_picks(&cache, picks, &mut report);
        // A retry has no caller to return the announcement to (G25):
        // what it finished goes out through the sink, as its own entry.
        if let Ok(a) = report.announcement(&cache, &[]) {
            self.announce(a);
        }
        run
    }

    // -- the work ---------------------------------------------------

    fn run_picks(
        &self,
        cache: &Cache,
        picks: Vec<retry::Pick>,
        report: &mut Report,
    ) -> Result<RunSummary, FixError> {
        let mut summary = RunSummary::default();
        if picks.is_empty() {
            return Ok(summary);
        }
        // Each entry as it stands before this pass, to tell afterwards
        // which ones it changed.
        for pick in &picks {
            if let Some(id) = pick.row.fix_id {
                if let std::collections::hash_map::Entry::Vacant(slot) = report.before.entry(id) {
                    if let Some(row) = cache.get_fix_log(id)? {
                        slot.insert(FixNotice::from_row(&row));
                    }
                }
            }
        }
        let graph = UnitGraph::build_from_cache(cache)?;
        let mut writes = 0usize;
        if self.stop_after_writes == Some(0) {
            return Err(FixError::Stopped);
        }
        for pick in picks {
            let wrote = self.process(cache, &graph, pick, &mut summary)?;
            if let Some(file) = wrote {
                report.written.push(file);
                writes += 1;
                if self.stop_after_writes == Some(writes) {
                    return Err(FixError::Stopped);
                }
            }
        }
        Ok(summary)
    }

    /// One row, start to end. The file, when it was written.
    fn process(
        &self,
        cache: &Cache,
        graph: &UnitGraph,
        pick: retry::Pick,
        summary: &mut RunSummary,
    ) -> Result<Option<WrittenFile>, FixError> {
        let retry::Pick {
            mut row,
            prior_rounds,
        } = pick;

        let Some(source) = cache.get_source(&row.source)? else {
            let why = format!("source {} is no longer registered", row.source);
            self.finish(
                cache,
                &row,
                Outcome::done(FILE_NOTHING_TO_FIX, why),
                summary,
            )?;
            return Ok(None);
        };
        if source.status == SourceStatus::Offline {
            // Its files cannot be written, and a retry now would only
            // burn attempts: the row waits for the source.
            summary.skipped_offline += 1;
            return Ok(None);
        }

        let (target_source, target_path, chain) = match pending::recheck(cache, graph, &row)? {
            Verdict::Finish { status, reason } => {
                let outcome = if status == FILE_NEEDS_MANUAL_FIX {
                    Outcome::manual(reason, 0, 0, 0)
                } else {
                    Outcome::done(status, reason)
                };
                self.finish(cache, &row, outcome, summary)?;
                return Ok(None);
            }
            Verdict::Proceed {
                source,
                path,
                chain,
            } => (source, path, chain),
        };

        // A chain A to B to C: the row now names C.
        if row.new_path.as_deref() != Some(target_path.as_str()) {
            cache.apply_fix_step(
                row.id,
                &PendingOp::Repoint {
                    new_source: target_source.clone(),
                    new_path: target_path.clone(),
                },
                self.clock.now(),
                None,
                |_| None,
            )?;
            row.new_source = Some(target_source.clone());
            row.new_path = Some(target_path.clone());
        }

        let old_path = row.old_path.clone().unwrap_or_default();
        let old_source = row.old_source.clone().unwrap_or_default();
        let old_id = format!("@{old_source}:{old_path}");
        let new_id = format!("@{target_source}:{target_path}");
        let file_id = format!("@{}:{}", row.source, row.file_path);
        let bare_resolves = graph
            .shortest_unique_target(&new_id)
            .is_some_and(|t| !t.contains('/'));
        // A file that went to another source is fixed by one of the three
        // rewrites of a move, by where the linking file is (G6).
        let (edges, mode) = if old_source == target_source {
            (
                file_edges(graph, &old_id, &new_id, &file_id),
                RewriteMode::Fix { bare_resolves },
            )
        } else if file_id == new_id {
            let (edges, links) =
                own_links(graph, &old_source, &old_path, &target_source, &target_path);
            (
                edges,
                RewriteMode::Move(MoveRewrite::Own {
                    new_path: target_path.clone(),
                    bare_resolves,
                    links,
                }),
            )
        } else if row.source == target_source {
            (
                file_edges(graph, &old_id, &new_id, &file_id),
                RewriteMode::Move(MoveRewrite::NewSource {
                    new_path: target_path.clone(),
                    bare_resolves,
                }),
            )
        } else {
            (
                file_edges(graph, &old_id, &new_id, &file_id),
                RewriteMode::Move(MoveRewrite::OldSource {
                    new_source: target_source.clone(),
                    new_path: target_path.clone(),
                }),
            )
        };
        if edges.is_empty() {
            self.finish(
                cache,
                &row,
                Outcome::done(FILE_NOTHING_TO_FIX, "the file has no link left to rewrite"),
                summary,
            )?;
            return Ok(None);
        }

        let abs_path = Path::new(&source.path).join(&row.file_path);
        let mut req = FileFixRequest::new(
            file_id,
            abs_path.clone(),
            edges,
            old_path,
            target_path,
            mode,
        );
        req.chain = chain.clone();
        req.prior_rounds = prior_rounds;

        let result = self.writer.write_file(&req);
        let wrote = result.status == WriteStatus::Written;
        let outcome = Outcome::from_write(&result, chain);
        self.finish(cache, &row, outcome, summary)?;
        Ok(wrote.then(|| WrittenFile {
            source: row.source.clone(),
            path: abs_path,
        }))
    }

    /// Records one row's outcome: the row and the entry in one
    /// transaction.
    fn finish(
        &self,
        cache: &Cache,
        row: &PendingFixRow,
        outcome: Outcome,
        summary: &mut RunSummary,
    ) -> Result<(), FixError> {
        match (&outcome.keep, outcome.file_status) {
            (None, FILE_WRITTEN) => summary.written += 1,
            (None, _) => summary.dropped += 1,
            (Some(k), _) if k.state == STATE_NEEDS_MANUAL_FIX => summary.needs_manual_fix += 1,
            (Some(_), _) => summary.deferred += 1,
        }
        let op = match &outcome.keep {
            None => PendingOp::Delete,
            Some(k) => PendingOp::Update {
                state: k.state.to_string(),
                reason: outcome.reason.clone(),
                attempts: i64::from(k.rounds),
            },
        };
        cache.apply_fix_step(row.id, &op, self.clock.now(), row.fix_id, |log| {
            let mut detail = FixDetail::parse(log.detail_json.as_deref());
            let file = detail.file_mut(&row.source, &row.file_path);
            file.status = outcome.file_status.to_string();
            file.links_changed = outcome.links_changed;
            file.attempts = outcome.attempts;
            file.reason = outcome.reason.clone();
            file.recovered_edits += outcome.recovered_edits;
            if outcome.chain.len() > 2 {
                file.chain = outcome.chain.clone();
            }
            detail.recovered_edits += outcome.recovered_edits;
            Some(FixLogChange {
                status: overall_status(&detail.files).to_string(),
                files_changed: detail.files_written(),
                detail_json: Some(detail.to_json()),
            })
        })?;
        Ok(())
    }
}

/// What came of one row, ready to record.
struct Outcome {
    /// The file's status in the entry's detail.
    file_status: &'static str,
    reason: Option<String>,
    links_changed: usize,
    attempts: u32,
    recovered_edits: u32,
    chain: Vec<String>,
    /// `Some` when the row stays in the list.
    keep: Option<Keep>,
}

struct Keep {
    state: &'static str,
    /// Rounds that ended in a transient failure.
    rounds: u32,
}

impl Outcome {
    /// A row finished without a write.
    fn done(status: &'static str, reason: impl Into<String>) -> Self {
        Outcome {
            file_status: status,
            reason: Some(reason.into()),
            links_changed: 0,
            attempts: 0,
            recovered_edits: 0,
            chain: Vec::new(),
            keep: None,
        }
    }

    /// A row kept as needing a person.
    fn manual(reason: String, rounds: u32, attempts: u32, recovered_edits: u32) -> Self {
        Outcome {
            file_status: FILE_NEEDS_MANUAL_FIX,
            reason: Some(reason),
            links_changed: 0,
            attempts,
            recovered_edits,
            chain: Vec::new(),
            keep: Some(Keep {
                state: STATE_NEEDS_MANUAL_FIX,
                rounds,
            }),
        }
    }

    fn from_write(result: &FileWriteResult, chain: Vec<String>) -> Self {
        let base = |file_status: &'static str, keep: Option<Keep>| Outcome {
            file_status,
            reason: result.reason.clone(),
            links_changed: result.links_changed,
            attempts: result.attempts,
            recovered_edits: result.recovered_edits,
            chain,
            keep,
        };
        match result.status {
            WriteStatus::Written => base(FILE_WRITTEN, None),
            WriteStatus::NothingToWrite => {
                let mut o = base(FILE_NOTHING_TO_FIX, None);
                o.reason = Some("the file has no link left to rewrite".to_string());
                o
            }
            WriteStatus::NotFound => {
                let mut o = base(FILE_NOTHING_TO_FIX, None);
                o.reason = Some("the file no longer exists".to_string());
                o
            }
            WriteStatus::Deferred => base(
                FILE_PENDING,
                Some(Keep {
                    state: STATE_PENDING,
                    rounds: result.rounds,
                }),
            ),
            WriteStatus::NeedsManualFix => base(
                FILE_NEEDS_MANUAL_FIX,
                Some(Keep {
                    state: STATE_NEEDS_MANUAL_FIX,
                    rounds: result.rounds,
                }),
            ),
        }
    }
}

fn entry(cache: &Cache, id: i64) -> Result<FixEntryWire, FixError> {
    cache
        .get_fix_log(id)?
        .map(|row| FixEntryWire::from(&row))
        .ok_or_else(|| FixError::Cache(format!("fix log entry {id} is gone")))
}

/// The wikilink edges from `file_id` to the old target: those that
/// dangle and those the rename handed to another file of the same name.
/// A self-link of the renamed file recorded under the old id belongs to
/// the file at the new one (as in [`crate::fix::fix_rename`]).
fn file_edges(graph: &UnitGraph, old_id: &str, new_id: &str, file_id: &str) -> Vec<Edge> {
    graph
        .get_rename_backlinks(old_id, new_id)
        .into_iter()
        .filter(|e| e.kind == tephra_parser::LINK_KIND_WIKILINK)
        .map(|mut e| {
            if e.from == old_id {
                e.from = new_id.to_string();
            }
            e
        })
        .filter(|e| e.from == file_id)
        .collect()
}

/// Every file with a wikilink that named `old_path` of `old_source`,
/// as `(source, path)`, after it was renamed (or moved to another source)
/// to `new_path` of `new_source`.
fn linking_files(
    graph: &UnitGraph,
    old_source: &str,
    old_path: &str,
    new_source: &str,
    new_path: &str,
) -> Result<Vec<(String, String)>, FixError> {
    let old_id = format!("@{old_source}:{old_path}");
    let new_id = format!("@{new_source}:{new_path}");
    if old_source == new_source && old_path == new_path {
        return Ok(Vec::new());
    }
    if graph.unit(&new_id).is_none() {
        return Err(FixError::Refused(format!(
            "'{new_path}' is not a unit in source '{new_source}'"
        )));
    }
    if graph.unit(&old_id).is_some() {
        return Err(FixError::Refused(format!(
            "'{old_path}' is still a unit in source '{old_source}': the rename has not been indexed"
        )));
    }
    let files: BTreeSet<(String, String)> = graph
        .get_rename_backlinks(&old_id, &new_id)
        .into_iter()
        .filter(|e| e.kind == tephra_parser::LINK_KIND_WIKILINK)
        .filter_map(|e| {
            let from = if e.from == old_id {
                new_id.clone()
            } else {
                e.from
            };
            let (s, p) = from.strip_prefix('@')?.split_once(':')?;
            Some((s.to_string(), p.to_string()))
        })
        .collect();
    Ok(files.into_iter().collect())
}

/// What the links of the moved file itself need when it went from
/// `old_source` to `new_source` (rewrite 3): the file's wikilink edges
/// that change, and what each changes to (see [`OwnLink`]). Every link
/// of the file is looked at in the graph as it is after the move; where
/// it pointed before is worked out by resolving it in the source the
/// file left (the old file is no unit of it any more).
fn own_links(
    graph: &UnitGraph,
    old_source: &str,
    old_path: &str,
    new_source: &str,
    new_path: &str,
) -> (Vec<Edge>, HashMap<String, OwnLink>) {
    let new_id = format!("@{new_source}:{new_path}");
    let (old_stem, _) = crate::refactor::strip_md_ext_pub(old_path);
    let old_base = old_stem.rsplit('/').next().unwrap_or(old_stem);
    let names_old_file = |target: &str| {
        let (t, _) = crate::refactor::strip_md_ext_pub(target);
        t.eq_ignore_ascii_case(old_stem) || t.eq_ignore_ascii_case(old_base)
    };
    let mut edges = Vec::new();
    let mut links: HashMap<String, OwnLink> = HashMap::new();
    for edge in graph.get_forward_links_all(&new_id) {
        if edge.kind != tephra_parser::LINK_KIND_WIKILINK {
            continue;
        }
        let w = tephra_parser::parse_wikilink_inner(&edge.to_raw);
        if w.target.is_empty() {
            continue;
        }
        let file_ref = |source: &str| format!("@{source}:{}", w.target);
        let action = match w.source_prefix.as_deref() {
            None => match graph.resolve_link(&file_ref(old_source), None) {
                Ok(_) => Some(OwnLink::AddPrefix(old_source.to_string())),
                Err(_) if names_old_file(&w.target) => Some(OwnLink::SelfLink),
                Err(_) => None,
            },
            Some(p) if p == old_source => {
                // `@old:<own old path>`: the file itself. Anything else
                // with that prefix still names the other source, right.
                let full = crate::refactor::strip_md_ext_pub(&w.target).0;
                full.eq_ignore_ascii_case(old_stem)
                    .then_some(OwnLink::SelfLink)
            }
            Some(p) if p == new_source => {
                let prefixed = graph.resolve_link(&file_ref(new_source), None);
                let bare = graph.resolve_link(&w.target, Some(new_source));
                match (prefixed, bare) {
                    (Ok(a), Ok(b)) if a.unit.id == b.unit.id => Some(OwnLink::DropPrefix),
                    _ => None,
                }
            }
            Some(_) => None,
        };
        if let Some(action) = action {
            links.insert(edge.to_raw.clone(), action);
            edges.push(edge);
        }
    }
    (edges, links)
}

/// `changed` (paths on disk) relative to the root of `source`. Paths
/// outside the root are left out.
fn relative_paths(
    cache: &Cache,
    source: &str,
    changed: &[PathBuf],
) -> Result<HashSet<String>, FixError> {
    let Some(row) = cache.get_source(source)? else {
        return Ok(HashSet::new());
    };
    let root = PathBuf::from(&row.path);
    let canonical_root = root.canonicalize().ok();
    Ok(changed
        .iter()
        .filter_map(|p| {
            let strip = |p: &Path| {
                p.strip_prefix(&root)
                    .ok()
                    .or_else(|| canonical_root.as_ref().and_then(|r| p.strip_prefix(r).ok()))
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            };
            // The path as given, else with its folder resolved (a
            // symlinked temp folder; the file itself may be gone).
            strip(p).or_else(|| {
                let parent = p.parent()?.canonicalize().ok()?;
                strip(&parent.join(p.file_name()?))
            })
        })
        .collect())
}

impl crate::rename_detect::RenameFixer for FixEngine {
    fn start_batch(&self) {
        self.batches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(std::thread::current().id(), FixAnnouncement::default());
    }

    fn finish_batch(&self) {
        let open = self
            .batches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&std::thread::current().id());
        if let Some(open) = open {
            self.announce(open);
        }
    }

    fn fix_rename(&self, source: &str, old: &str, new: &str) -> Result<(), String> {
        // G4: the same rename may be in the log already (an exact event
        // got there first). The pairing then does nothing.
        let _decision = self.decision_guard();
        let target = match self.decide_for_watcher(source, old, new) {
            Ok(Some(target)) => target,
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        self.begin_rename_announced(source, old, &target)
            .map(|(e, a)| {
                log_entry(&e);
                self.announce_or_collect(a);
            })
            .map_err(|e| e.to_string())
    }

    fn fix_folder_rename(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<(), String> {
        let _decision = self.decision_guard();
        let moves = match self.watcher_folder_moves(source, old_dir, new_dir, moves) {
            Ok(Some(moves)) => moves,
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        self.begin_folder_rename_announced(source, old_dir, new_dir, &moves)
            .map(|(e, a)| {
                log_entry(&e);
                self.announce_or_collect(a);
            })
            .map_err(|e| e.to_string())
    }

    fn fix_move(
        &self,
        old_source: &str,
        old: &str,
        new_source: &str,
        new: &str,
    ) -> Result<(), String> {
        // E7: the same move may be in the log already (an exact event got
        // there first). The pairing then does nothing.
        let _decision = self.decision_guard();
        if self
            .move_logged(old_source, old, new_source, new)
            .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        match self.begin_move_announced(old_source, old, new_source, new) {
            Ok((e, a)) => {
                log_entry(&e);
                self.announce_or_collect(a);
                Ok(())
            }
            // The new file is not a unit (ignored, not indexed) or the
            // old one still is: the pair is no move Tephra can fix, and
            // the vanished side is reported like any other.
            Err(FixError::Refused(why)) => {
                tracing::debug!(old_source, old, new_source, new, %why, "move refused by the engine");
                self.record_uncertain_inner(
                    old_source,
                    old,
                    MSG_UNCERTAIN,
                    Dedupe::AfterOutsideEvent,
                )
                .map(|e| log_entry(&e))
                .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn record_uncertain(&self, source: &str, path: &str) -> Result<(), String> {
        // An exact event moved this file already: its links were fixed
        // (or reported), so the vanishing is no news (G4).
        let _decision = self.decision_guard();
        if self
            .log_says_departed(source, path)
            .map_err(|e| e.to_string())?
            || self
                .move_departed(source, path)
                .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        self.record_uncertain_inner(source, path, MSG_UNCERTAIN, Dedupe::AfterOutsideEvent)
            .map(|e| log_entry(&e))
            .map_err(|e| e.to_string())
    }
}

/// One info line in the daemon log for a fix-log entry the automatic
/// fix just wrote (the entry itself is in `fix.list`).
fn log_entry(e: &FixEntryWire) {
    tracing::info!(
        fix_id = e.id,
        kind = %e.kind,
        old = e.old.as_deref().unwrap_or(""),
        new = e.new.as_deref().unwrap_or(""),
        files_changed = e.files_changed,
        status = %e.status,
        "fix"
    );
}
