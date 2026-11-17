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

pub mod log;
pub mod pending;
pub mod retry;

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tephra_cache::{
    Cache, FixLogChange, NewFixLog, NewPendingFix, PendingFixRow, PendingOp, SourceStatus,
    STATE_NEEDS_MANUAL_FIX, STATE_PENDING,
};
use tephra_graph::{Edge, UnitGraph};

use crate::refactor::fix_write::{
    FileFixRequest, FileWriteResult, FixWriter, RewriteMode, WriteStatus,
};

pub use log::{FixDetail, FixEntryWire};
pub use pending::Verdict;
pub use retry::Trigger;

use log::{
    overall_status, FileEntry, FILE_NEEDS_MANUAL_FIX, FILE_NOTHING_TO_FIX, FILE_PENDING,
    FILE_WRITTEN, KIND_RENAME, KIND_UNCERTAIN, STATUS_UNCERTAIN,
};

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
}

impl FileWriter for FixWriter {
    fn write_file(&self, req: &FileFixRequest) -> FileWriteResult {
        FixWriter::write_file(self, req)
    }
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
}

impl FixEngine {
    pub fn new(cache_path: impl Into<PathBuf>, writer: Arc<dyn FileWriter>) -> Self {
        FixEngine {
            cache_path: cache_path.into(),
            writer,
            clock: Arc::new(SystemClock),
            stop_after_writes: None,
            run_lock: Mutex::new(()),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Test hook: each call stops, with [`FixError::Stopped`], right
    /// after the `n`-th file it writes has been recorded. Nothing is
    /// rolled back or finished: the cache and the disk are as a kill at
    /// that moment leaves them.
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
    /// broken. No pending work is created for it (G10).
    pub fn record_uncertain(&self, source: &str, path: &str) -> Result<FixEntryWire, FixError> {
        let _guard = self.lock();
        let cache = self.open()?;
        let detail = FixDetail {
            message: Some("vanished, no certain pair: links broken".to_string()),
            ..FixDetail::default()
        };
        let id = cache.insert_fix_log(&NewFixLog {
            created_at: self.clock.now(),
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
        let files = {
            let cache = self.open()?;
            let graph = UnitGraph::build_from_cache(&cache)?;
            linking_files(&graph, source, old_path, new_path)?
        };
        self.begin_fix(&FixSpec {
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
        let rows = {
            let cache = self.open()?;
            let graph = UnitGraph::build_from_cache(&cache)?;
            let mut rows: BTreeSet<PlanRow> = BTreeSet::new();
            for (old, new) in moves {
                // A moved file that cannot be fixed (not a unit now, or
                // its old path is one again) is left out; the rest
                // still are.
                let Ok(files) = linking_files(&graph, source, old, new) else {
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
            old_source: source.to_string(),
            new_source: source.to_string(),
            entry_old: old_dir.to_string(),
            entry_new: new_dir.to_string(),
            rows,
        })
    }

    /// Logs ONE entry for the rename and stores every linking file in
    /// the pending list, in one transaction, before the first write.
    /// Then works through the list. Returns the entry as it stands at
    /// the end (`partial` while work is left).
    pub fn begin_fix(&self, spec: &FixSpec) -> Result<FixEntryWire, FixError> {
        let files: BTreeSet<&(String, String)> = spec.files.iter().collect();
        self.begin_plan(&Plan {
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

    /// The body of [`FixEngine::begin_fix`] and
    /// [`FixEngine::begin_folder_rename`]: the entry and the work list
    /// in one transaction, then the work.
    fn begin_plan(&self, plan: &Plan) -> Result<FixEntryWire, FixError> {
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
                kind: KIND_RENAME.to_string(),
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
        self.run_picks(&cache, picks)?;
        entry(&cache, fix_id)
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
        self.run_picks(&cache, picks)
    }

    // -- the work ---------------------------------------------------

    fn run_picks(&self, cache: &Cache, picks: Vec<retry::Pick>) -> Result<RunSummary, FixError> {
        let mut summary = RunSummary::default();
        if picks.is_empty() {
            return Ok(summary);
        }
        let graph = UnitGraph::build_from_cache(cache)?;
        let mut writes = 0usize;
        for pick in picks {
            let wrote = self.process(cache, &graph, pick, &mut summary)?;
            if wrote {
                writes += 1;
                if self.stop_after_writes == Some(writes) {
                    return Err(FixError::Stopped);
                }
            }
        }
        Ok(summary)
    }

    /// One row, start to end. `true` when the file was written.
    fn process(
        &self,
        cache: &Cache,
        graph: &UnitGraph,
        pick: retry::Pick,
        summary: &mut RunSummary,
    ) -> Result<bool, FixError> {
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
            return Ok(false);
        };
        if source.status == SourceStatus::Offline {
            // Its files cannot be written, and a retry now would only
            // burn attempts: the row waits for the source.
            summary.skipped_offline += 1;
            return Ok(false);
        }

        let (target_source, target_path, chain) = match pending::recheck(cache, graph, &row)? {
            Verdict::Finish { status, reason } => {
                let outcome = if status == FILE_NEEDS_MANUAL_FIX {
                    Outcome::manual(reason, 0, 0, 0)
                } else {
                    Outcome::done(status, reason)
                };
                self.finish(cache, &row, outcome, summary)?;
                return Ok(false);
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
        let old_id = format!(
            "@{}:{}",
            row.old_source.as_deref().unwrap_or_default(),
            old_path
        );
        let new_id = format!("@{target_source}:{target_path}");
        let file_id = format!("@{}:{}", row.source, row.file_path);
        let edges = file_edges(graph, &old_id, &new_id, &file_id);
        if edges.is_empty() {
            self.finish(
                cache,
                &row,
                Outcome::done(FILE_NOTHING_TO_FIX, "the file has no link left to rewrite"),
                summary,
            )?;
            return Ok(false);
        }

        let bare_resolves = graph
            .shortest_unique_target(&new_id)
            .is_some_and(|t| !t.contains('/'));
        let mut req = FileFixRequest::new(
            file_id,
            Path::new(&source.path).join(&row.file_path),
            edges,
            old_path,
            target_path,
            RewriteMode::Fix { bare_resolves },
        );
        req.chain = chain.clone();
        req.prior_rounds = prior_rounds;

        let result = self.writer.write_file(&req);
        let wrote = result.status == WriteStatus::Written;
        let outcome = Outcome::from_write(&result, chain);
        self.finish(cache, &row, outcome, summary)?;
        Ok(wrote)
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

/// Every file with a wikilink that named `old_path`, as `(source, path)`.
fn linking_files(
    graph: &UnitGraph,
    source: &str,
    old_path: &str,
    new_path: &str,
) -> Result<Vec<(String, String)>, FixError> {
    let old_id = format!("@{source}:{old_path}");
    let new_id = format!("@{source}:{new_path}");
    if old_path == new_path {
        return Ok(Vec::new());
    }
    if graph.unit(&new_id).is_none() {
        return Err(FixError::Refused(format!(
            "'{new_path}' is not a unit in source '{source}'"
        )));
    }
    if graph.unit(&old_id).is_some() {
        return Err(FixError::Refused(format!(
            "'{old_path}' is still a unit in source '{source}': the rename has not been indexed"
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
    fn fix_rename(&self, source: &str, old: &str, new: &str) -> Result<(), String> {
        self.begin_rename(source, old, new)
            .map(|e| log_entry(&e))
            .map_err(|e| e.to_string())
    }

    fn fix_folder_rename(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<(), String> {
        self.begin_folder_rename(source, old_dir, new_dir, moves)
            .map(|e| log_entry(&e))
            .map_err(|e| e.to_string())
    }

    fn record_uncertain(&self, source: &str, path: &str) -> Result<(), String> {
        FixEngine::record_uncertain(self, source, path)
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
