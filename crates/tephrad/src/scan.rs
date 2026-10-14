//! The walk-parse-insert pipeline shared by an initial full scan
//! (GP2/GP3), a forced rescan (`source.rescan`), and the batched
//! change-apply path used by the debounced watcher (GP6/E5).
//!
//! Both entry points go through `tephra_parser::parse_unit_bytes` for
//! extraction and write through `tephra_cache`'s typed row shapes
//! (`NewUnit`/`NewLink`/`NewTag`/`NewBlock`), so a unit's cache row
//! shape here is always in sync with what the parser crate produces
//! — there is no separate re-description of that schema in this
//! module.
//!
//! Only markdown files (`.md`/`.markdown`, case-insensitive) are ever
//! fed through the parser (GP3: "feeds every markdown file through
//! the PR-3 parser", R3 review finding): a non-ignored non-markdown
//! file (a `Cargo.toml`, a `main.rs`, in a code-repository source)
//! is still walked and counted in `files_seen`, but never parsed or
//! written as a unit row — the walk and the debounced batch-apply
//! path both apply this filter identically, so a code-repo source
//! never ends up with false wikilinks/tags derived from its source
//! files.
//!
//! Transaction strategy (R11 review finding): a full scan or forced
//! rescan commits its writes in bounded chunks ([`SCAN_COMMIT_CHUNK_SIZE`])
//! rather than holding one open transaction (and so SQLite's single
//! writer lock) for the entire walk — a large source's scan must not
//! make every other source's watcher batch, or the shared registry's
//! own `list`/`register`/`rescan` connection, sit out the busy timeout
//! and fail for the whole scan's duration (breaking GP4/GP6 in
//! practice even though every source's in-process state is already
//! independent). The debounced batch-apply path ([`apply_change_batch`])
//! stays single-transaction (E5: "a single rescan transaction ...
//! never one write per event"), but now opens it with `BEGIN
//! IMMEDIATE` and retries on `SQLITE_BUSY` instead of reading first
//! (risking a failed upgrade) and dropping the whole batch on the
//! first contention it hits.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Transaction, TransactionBehavior};
use tephra_cache::Cache;

use crate::ignore::IgnoreRules;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanStats {
    pub files_seen: usize,
    pub files_ignored: usize,
    pub units_written: usize,
    pub links_written: usize,
    pub tags_written: usize,
    pub blocks_written: usize,
}

impl ScanStats {
    /// Folds another batch's counts into this one — used whenever one
    /// file's [`write_unit`] result needs to be added into a running
    /// total, in both the full-walk and the directory-upsert paths.
    fn accumulate(&mut self, other: ScanStats) {
        self.units_written += other.units_written;
        self.links_written += other.links_written;
        self.tags_written += other.tags_written;
        self.blocks_written += other.blocks_written;
    }
}

/// The result of [`apply_change_batch`]: the usual per-batch counts,
/// plus the set of absolute paths that were actually upserted or
/// deleted — distinct from the raw set of changed paths the watcher
/// observed, which may include ignored paths, directories, or paths
/// that turned out not to need any cache write at all (R5 review
/// finding). A caller reporting `source_changed` uses
/// `changed_paths`, not the raw input batch.
#[derive(Debug, Default, Clone)]
pub struct ChangeOutcome {
    pub stats: ScanStats,
    pub changed_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
    Walk(walkdir::Error),
    Cache(tephra_cache::CacheError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io(e) => write!(f, "{e}"),
            ScanError::Walk(e) => write!(f, "{e}"),
            ScanError::Cache(e) => write!(f, "{e}"),
            ScanError::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<std::io::Error> for ScanError {
    fn from(e: std::io::Error) -> Self {
        ScanError::Io(e)
    }
}

impl From<walkdir::Error> for ScanError {
    fn from(e: walkdir::Error) -> Self {
        ScanError::Walk(e)
    }
}

impl From<tephra_cache::CacheError> for ScanError {
    fn from(e: tephra_cache::CacheError) -> Self {
        ScanError::Cache(e)
    }
}

impl From<rusqlite::Error> for ScanError {
    fn from(e: rusqlite::Error) -> Self {
        ScanError::Sqlite(e)
    }
}

/// Number of files written per committed chunk during a full scan or
/// forced rescan (R11): bounded so a long walk over a very large
/// source periodically commits and releases SQLite's single writer
/// lock, instead of holding one open transaction for the walk's
/// entire duration. This trades strict all-or-nothing atomicity for
/// the whole walk (a genuine cache-level write failure partway
/// through now leaves whatever chunks already committed in place,
/// rather than rolling the entire walk back) for that interleaving —
/// the realistic per-entry failure modes (an unreadable file, a
/// non-UTF8 file, an unreadable directory) are already handled by
/// logging and skipping rather than propagating (see [`write_unit`]
/// and the walk loop below), so the only way a chunk's commit is
/// never reached at all is a genuine SQLite-level failure (disk full,
/// corruption) — already a serious failure mode regardless of
/// transaction boundaries.
const SCAN_COMMIT_CHUNK_SIZE: usize = 500;

/// How long [`walk_and_write`] sleeps after committing each chunk,
/// before opening the next chunk's `BEGIN IMMEDIATE` (R16 review
/// finding). Committing a chunk releases SQLite's write lock, but
/// another connection that's currently waiting for it (stuck in its
/// own `busy_timeout`'s internal sleep-and-poll loop, e.g. a
/// concurrent `register()`'s insert, `source.rescan`'s own
/// transition, or another source's debounced batch) does not notice
/// a released lock the instant it's released — it only notices the
/// next time its own backoff timer happens to wake it up. Without
/// this sleep, this thread's next `begin_immediate` call (made with
/// no delay at all right after the commit) almost always wins that
/// race before the waiter's timer ever fires, so a long scan could
/// starve every other writer for its *entire* duration even though
/// the write lock is, technically, only held for a bounded chunk at a
/// time (the review's probe: a concurrent `register()` issued partway
/// through a long scan returned only once the scan fully finished,
/// and a second, longer scan made a concurrent `register()` fail
/// outright with "database is locked"). This sleep gives any waiting
/// connection's poll a real chance to land in the gap, on every single
/// chunk boundary rather than only once at the very end. Kept short
/// (10ms, not e.g. 100ms+) so this stays a small fraction of a
/// chunk's own work (parsing and writing up to
/// [`SCAN_COMMIT_CHUNK_SIZE`] files) rather than a dominant cost on a
/// very large source's scan time.
const CHUNK_COMMIT_YIELD: Duration = Duration::from_millis(10);

/// How many times a `SQLITE_BUSY` failure is retried, and the backoff
/// slept between attempts (R11). SQLite's own `busy_timeout` (set on
/// every connection, see `tephra_cache::cache::apply_pragmas`) already
/// waits out a lock that's currently held before returning
/// `SQLITE_BUSY` to the caller — these retries are a second, coarser
/// layer on top of that: a whole failed attempt (not just the
/// `BEGIN`) is retried wholesale, so a batch that loses the race
/// entirely (rather than merely waiting out a brief lock hold) is
/// retried rather than silently discarded on its very first
/// `SQLITE_BUSY`, which is what this module's previous "log and drop"
/// behavior did.
const MAX_BUSY_RETRIES: u32 = 8;
const BUSY_RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// Whether `err` is SQLite's `SQLITE_BUSY` (the database is locked by
/// another connection), as opposed to any other failure that retrying
/// would just reproduce identically.
fn is_database_busy(err: &ScanError) -> bool {
    matches!(
        err,
        ScanError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// Retries `attempt` up to [`MAX_BUSY_RETRIES`] times, sleeping
/// [`BUSY_RETRY_BACKOFF`] between attempts, whenever it fails with a
/// `SQLITE_BUSY`-classified error (R11) — any other error (including
/// a non-busy SQLite error) is returned immediately on its first
/// occurrence, since retrying those would just repeat the same
/// failure.
fn retry_on_busy<T>(mut attempt: impl FnMut() -> Result<T, ScanError>) -> Result<T, ScanError> {
    let mut tries = 0;
    loop {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) if is_database_busy(&e) && tries < MAX_BUSY_RETRIES => {
                tries += 1;
                std::thread::sleep(BUSY_RETRY_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Opens a `BEGIN IMMEDIATE` transaction against `cache`'s connection
/// (R11): acquires SQLite's write lock up front, rather than the
/// `BEGIN DEFERRED` a plain `unchecked_transaction()` uses, which only
/// takes a lock lazily on a connection's first write — leaving a
/// transaction that reads before it writes (as
/// [`apply_change_batch`] does, via `get_unit_by_rel_path`) exposed to
/// an immediate, non-retryable failure if another connection committed
/// a change in between, rather than a retryable "the lock is currently
/// held" wait.
fn begin_immediate(cache: &Cache) -> Result<Transaction<'_>, ScanError> {
    Transaction::new_unchecked(cache.connection(), TransactionBehavior::Immediate)
        .map_err(ScanError::from)
}

/// File extensions (case-insensitive, no leading dot) treated as
/// markdown (R3/GP3): only these are ever parsed and written as a
/// unit row. No per-source override in v1, matching G2's "no
/// per-source override" precedent for the ignore rules.
const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// Whether `path`'s extension marks it as a markdown file this scan
/// should actually parse. A non-ignored file with any other
/// extension (or none) is still walked and counted as "seen", but is
/// never handed to [`tephra_parser::parse_unit_bytes`] or written as
/// a unit row.
fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|md_ext| ext.eq_ignore_ascii_case(md_ext))
        })
        .unwrap_or(false)
}

/// Relative path, normalized to forward slashes regardless of
/// platform, for storage in `units.rel_path` and for matching against
/// [`IgnoreRules`].
fn to_rel_string(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let joined: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join("/"))
    }
}

fn file_mtime(path: &Path) -> Option<i64> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs() as i64)
}

/// Parses one markdown file and writes its unit/link/tag/block rows,
/// under the given `source_id`. Returns `Ok(None)` (not an error) for
/// a file that can't be read as UTF-8 text or whose bytes couldn't be
/// read at all — a single unreadable file must not abort the rest of
/// the scan or the whole rescan transaction (E1/E5: the walk and the
/// change-apply batch both keep going past one bad file). Callers are
/// responsible for only calling this on a path that has already
/// passed [`is_markdown_path`]; this function itself does not
/// re-check the extension.
///
/// Always an upsert, never a blind insert (R15 review finding): any
/// existing row at this `(source_id, rel_path)` is deleted first.
/// Before this fix, only [`upsert_directory`] and
/// [`apply_change_batch_once`] deleted the old row first — the plain
/// full-walk path ([`walk_and_write`]'s main loop, shared by
/// [`scan_source`] and [`rescan_source`]) called `insert_unit`
/// directly. That blind insert hits `UNIQUE(source_id, rel_path)` and
/// aborts the whole walk whenever a row for this path already exists
/// when the walk reaches it — which a forced rescan can trigger on
/// its own: `rescan_source` clears a source's rows and commits that
/// clear in its very first chunk ([`SCAN_COMMIT_CHUNK_SIZE`]), so the
/// live watcher's own debounced batch can land on a not-yet-walked
/// file in a later chunk — via its own, independent connection —
/// after the clear committed but before this walk gets there,
/// upserting a fresh row for it. Deleting any existing row here first
/// makes every write-path in this module idempotent with respect to a
/// pre-existing row, regardless of how it got there.
fn write_unit(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    abs_path: &Path,
) -> Result<Option<ScanStats>, ScanError> {
    let rel_path = match to_rel_string(root, abs_path) {
        Some(r) => r,
        None => return Ok(None),
    };

    let bytes = match fs::read(abs_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), error = %e, "failed to read file during scan; skipping");
            return Ok(None);
        }
    };

    let parsed = match tephra_parser::parse_unit_bytes(abs_path, &bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), error = %e, "file is not valid UTF-8; skipping");
            return Ok(None);
        }
    };

    // R15: delete any existing row for this path before inserting the
    // freshly parsed one, so this is always an upsert.
    delete_existing_unit(cache, source_id, &rel_path)?;

    let mtime = file_mtime(abs_path);
    let unit_id = cache.insert_unit(&parsed.to_new_unit(source_id, &rel_path, mtime))?;

    let mut stats = ScanStats {
        files_seen: 0,
        files_ignored: 0,
        units_written: 1,
        links_written: 0,
        tags_written: 0,
        blocks_written: 0,
    };

    for link in parsed.to_new_links(unit_id) {
        cache.insert_link(&link)?;
        stats.links_written += 1;
    }
    for tag in parsed.to_new_tags(unit_id) {
        cache.insert_tag(&tag)?;
        stats.tags_written += 1;
    }
    for block in parsed.to_new_blocks(unit_id) {
        cache.insert_block(&block)?;
        stats.blocks_written += 1;
    }

    Ok(Some(stats))
}

/// Deletes the one unit row at `source_id`/`rel_path` (if any) and
/// its cascading links/tags/blocks, via `cache`'s typed helpers.
/// Returns whether a row actually existed to delete.
fn delete_existing_unit(cache: &Cache, source_id: i64, rel_path: &str) -> Result<bool, ScanError> {
    if let Some(existing) = cache.get_unit_by_rel_path(source_id, rel_path)? {
        cache.delete_unit(existing.id)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Shared walk core for [`scan_source`] and [`rescan_source`]: walks
/// `root` (streaming — see the module doc and E1), applying `ignore`
/// before descending into any directory so an ignored subtree's files
/// are never even listed, let alone parsed. Every non-ignored
/// markdown file is parsed and its rows written via `cache`;
/// non-markdown files are counted as seen but never parsed (R3).
///
/// A single unreadable directory entry (e.g. a permission-denied
/// subdirectory) is logged and skipped rather than aborting the whole
/// walk (R2) — the same "log and keep going" treatment
/// [`write_unit`] already gives one unreadable file.
///
/// This never collects the full file list into memory at once: the
/// `walkdir::WalkDir` iterator yields one entry at a time, and each
/// file's rows are written to the cache before moving to the next —
/// peak memory is bounded by one file's content plus its parsed
/// structures, not by the size of the source tree.
///
/// `first_tx` is the already-open transaction this walk's first chunk
/// writes into (so a caller, e.g. [`rescan_source`], can fold a
/// preceding delete into that same first chunk); every
/// [`SCAN_COMMIT_CHUNK_SIZE`]th file commits it and opens a fresh one
/// (R11 — see the module doc comment for the full rationale), and the
/// final (possibly partial) chunk is committed once the walk is
/// exhausted.
fn walk_and_write(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
    first_tx: Transaction<'_>,
) -> Result<ScanStats, ScanError> {
    let mut stats = ScanStats::default();
    let mut tx = Some(first_tx);
    let mut since_commit: usize = 0;

    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let rel = match entry.path().strip_prefix(root) {
                Ok(r) => r,
                Err(_) => return true,
            };
            !ignore.is_ignored(rel)
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "failed to walk a directory entry during scan; skipping");
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }

        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        stats.files_seen += 1;
        if ignore.is_ignored(rel) {
            stats.files_ignored += 1;
            continue;
        }
        if !is_markdown_path(entry.path()) {
            continue;
        }

        if let Some(file_stats) = write_unit(cache, source_id, root, entry.path())? {
            stats.accumulate(file_stats);
        }

        since_commit += 1;
        if since_commit >= SCAN_COMMIT_CHUNK_SIZE {
            tx.take()
                .expect("a transaction is always open mid-walk")
                .commit()?;
            // R16: give any connection currently waiting on this
            // database's write lock a real chance to grab it before
            // immediately reclaiming it for the next chunk — see
            // `CHUNK_COMMIT_YIELD`'s doc comment.
            std::thread::sleep(CHUNK_COMMIT_YIELD);
            tx = Some(begin_immediate(cache)?);
            since_commit = 0;
        }
    }

    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(stats)
}

/// `source.register`'s initial full scan (GP2/GP3), against a source
/// with no pre-existing rows. Commits in bounded chunks rather than
/// one transaction for the whole walk (R11 — see the module doc
/// comment).
pub fn scan_source(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let first_tx = begin_immediate(cache)?;
    walk_and_write(cache, source_id, root, ignore, first_tx)
}

/// `source.rescan`'s forced full re-walk: clears this source's
/// existing rows and re-walks it, with the clear folded into the
/// walk's first committed chunk (R2/R11) — unlike a naive "delete,
/// then scan" pair run as fully separate transactions, a failure
/// before the first chunk commits rolls the delete back too, instead
/// of leaving the source's rows wiped with nothing re-inserted at
/// all. Past that first chunk, later chunks commit independently (R11
/// — see the module doc comment): a failure deep into a very large
/// rescan can still leave the source with only part of its rows
/// restored until the next rescan, a deliberate trade against holding
/// the sole SQLite writer lock for the whole walk.
pub fn rescan_source(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let first_tx = begin_immediate(cache)?;
    cache.delete_units_for_source(source_id)?;
    walk_and_write(cache, source_id, root, ignore, first_tx)
}

/// Walks an existing directory that arrived as one changed path (a
/// create or a move-in: R4) and upserts every non-ignored markdown
/// file under it, the same way [`walk_and_write`] does for a full
/// scan — except scoped to this one subtree instead of the whole
/// source, and recording every touched absolute path into
/// `touched_paths` so the caller can report exactly what changed.
fn upsert_directory(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    abs_dir: &Path,
    ignore: &IgnoreRules,
    stats: &mut ScanStats,
    touched_paths: &mut Vec<PathBuf>,
) -> Result<(), ScanError> {
    let walker = walkdir::WalkDir::new(abs_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let rel = match entry.path().strip_prefix(root) {
                Ok(r) => r,
                Err(_) => return true,
            };
            !ignore.is_ignored(rel)
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "failed to walk a directory change event; skipping entry");
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }

        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if ignore.is_ignored(rel) {
            stats.files_ignored += 1;
            continue;
        }
        let rel_path = match to_rel_string(root, entry.path()) {
            Some(r) => r,
            None => continue,
        };

        let had_existing = delete_existing_unit(cache, source_id, &rel_path)?;
        stats.files_seen += 1;
        let mut wrote_unit = false;
        if is_markdown_path(entry.path()) {
            if let Some(file_stats) = write_unit(cache, source_id, root, entry.path())? {
                stats.accumulate(file_stats);
                wrote_unit = true;
            }
        }
        if had_existing || wrote_unit {
            touched_paths.push(entry.path().to_path_buf());
        }
    }

    Ok(())
}

/// Applies one debounce-coalesced batch of changed absolute paths
/// (GP6) as a single cache transaction (E5: "coalesces into a single
/// rescan transaction against the cache, never one write per
/// event") — re-parsing and writing only the file(s) that actually
/// changed, never the whole source.
///
/// Each changed path is handled by what it currently is on disk,
/// since a shell `mv` arrives as a bare delete+create rather than a
/// rename the project can tell apart (R4, a team lesson from the
/// Obsidian side of this project too):
/// - An existing regular file: its old row (if any) is replaced by a
///   fresh parse — unless it's non-markdown, in which case any old
///   row is still removed (R3) but nothing is reinserted.
/// - An existing directory (a create or a move-in): walked and every
///   markdown file under it is upserted ([`upsert_directory`]) — a
///   directory arriving as a single changed path is otherwise never
///   reconciled at all.
/// - A path that no longer exists (a delete or a move-out): its own
///   row (if any) is deleted, *and* every row whose `rel_path` is
///   nested under it is deleted too — covering a directory that was
///   removed or moved out, whose own files would otherwise be left
///   behind as stale rows forever.
///
/// Retries the whole attempt on `SQLITE_BUSY` (R11 — see the module
/// doc comment) rather than logging and discarding the batch on its
/// first contention with another connection (this source's own
/// in-progress rescan, or another source's scan sharing the same
/// WAL-mode database file): each retried attempt opens its own fresh
/// transaction and starts from this batch's original `changed` set
/// again, so a batch is only ever fully applied once or not at all,
/// never partially re-applied across attempts.
pub fn apply_change_batch(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    changed: &HashSet<PathBuf>,
    ignore: &IgnoreRules,
) -> Result<ChangeOutcome, ScanError> {
    retry_on_busy(|| apply_change_batch_once(cache, source_id, root, changed, ignore))
}

/// One attempt at [`apply_change_batch`]'s work, run inside its own
/// `BEGIN IMMEDIATE` transaction (R11): the write lock is acquired up
/// front, before any of this batch's reads (e.g.
/// `delete_existing_unit`'s `get_unit_by_rel_path` lookup), so this
/// never hits the specific "read first, then fail to upgrade to a
/// writer because another connection committed in between" failure
/// mode a `BEGIN DEFERRED` transaction is exposed to.
fn apply_change_batch_once(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    changed: &HashSet<PathBuf>,
    ignore: &IgnoreRules,
) -> Result<ChangeOutcome, ScanError> {
    let mut stats = ScanStats::default();
    let mut touched_paths: Vec<PathBuf> = Vec::new();

    // Brackets the whole batch in one BEGIN IMMEDIATE/COMMIT (E5):
    // `cache`'s typed insert/delete helpers below still operate
    // through `cache`'s own `&self` methods, but since those execute
    // against the very same underlying SQLite connection this
    // transaction guard was opened on, every one of them runs inside
    // it. Dropped without `commit()` (e.g. via an early `?` return
    // below) rolls the whole batch back instead of leaving a partial
    // write.
    let tx_guard = begin_immediate(cache)?;

    for abs_path in changed {
        let rel_path = match to_rel_string(root, abs_path) {
            Some(r) => r,
            None => continue,
        };
        let rel = Path::new(&rel_path);
        if ignore.is_ignored(rel) {
            stats.files_ignored += 1;
            continue;
        }

        match fs::metadata(abs_path) {
            Ok(meta) if meta.is_dir() => {
                upsert_directory(
                    cache,
                    source_id,
                    root,
                    abs_path,
                    ignore,
                    &mut stats,
                    &mut touched_paths,
                )?;
            }
            Ok(meta) if meta.is_file() => {
                let had_existing = delete_existing_unit(cache, source_id, &rel_path)?;
                stats.files_seen += 1;
                let mut wrote_unit = false;
                if is_markdown_path(abs_path) {
                    if let Some(file_stats) = write_unit(cache, source_id, root, abs_path)? {
                        stats.accumulate(file_stats);
                        wrote_unit = true;
                    }
                }
                if had_existing || wrote_unit {
                    touched_paths.push(abs_path.clone());
                }
            }
            Ok(_) => {
                // Neither a regular file nor a directory (e.g. a
                // socket/FIFO/device node) — not something this scan
                // pipeline tracks as a unit; nothing to do.
            }
            Err(_) => {
                // The path no longer exists: a delete, or a move-out
                // of a file or a whole directory (R4). Clean up both
                // its own row (if it was a tracked file) and every
                // row nested under it (if it was a tracked
                // directory) — exactly one of these two deletes will
                // ever find anything for a given real-world path, but
                // trying both means a directory's full subtree is
                // reliably reclaimed without having to know in
                // advance which one it used to be.
                let had_existing = delete_existing_unit(cache, source_id, &rel_path)?;
                let nested_deleted = cache.delete_units_under_prefix(source_id, &rel_path)?;
                if had_existing || nested_deleted > 0 {
                    touched_paths.push(abs_path.clone());
                }
            }
        }
    }

    tx_guard.commit()?;
    Ok(ChangeOutcome {
        stats,
        changed_paths: touched_paths,
    })
}
