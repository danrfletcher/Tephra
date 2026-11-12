//! The walk-parse-insert pipeline shared by an initial full scan
//! (GP2/GP3), a forced rescan (`source.rescan`), and the batched
//! change-apply path used by the debounced watcher (GP6/E5).
//!
//! Both entry points dispatch on the file's kind: prose goes through
//! `tephra_parser::parse_unit` (or `parse_unit_bytes`), code through
//! `tephra_parser::parse_code_unit` (see [`parse_file`], the one
//! dispatch). Both write through `tephra_cache`'s typed row shapes
//! (`NewUnit`/`NewLink`/`NewTag`/`NewBlock`), so a unit's cache row
//! shape here is always in sync with what the parser crate produces
//! — there is no separate re-description of that schema in this
//! module.
//!
//! Every non-ignored file becomes a unit (G2), not just Markdown. Each
//! file is classified by [`crate::file_kind::classify`] into `prose`,
//! `code`, `document`, `text-other` or `binary` from its name and its
//! first 8 KB. Every file that classifies as `prose` (`.md`,
//! `.markdown`, `.txt`, `.org`, `.rst`, `.adoc`, any case) is fed
//! through the prose parser (G3), which picks its rules from the
//! extension: links, `^id` blocks, paragraphs and — for Markdown only
//! — tags, plus headings for Markdown, org, rst and adoc. Every file
//! that classifies as `code` (an extension with a bundled tree-sitter
//! grammar, or a recognised shebang) is fed through the code parser
//! (G4), which reads links only inside comments and docstrings: a
//! `[[wikilink]]` in a string literal, a heredoc or code is never a
//! link. The code parse also records the file's symbols (G4) in the
//! `symbols` table and each `^id` comment as a `blocks` row owned by
//! its symbol (G13), written in the same chunk transaction as the
//! unit row. Nothing here writes to the file (F4) or resolves a
//! `#name` anchor to a symbol (F3).
//! Every other file — `document`, `text-other`, `binary`, and so a
//! `.kt`, `.rb`, `.json` or `.ini` file, which has no grammar — is
//! written as a unit carrying name, kind, size and mtime and nothing
//! else — no links, headings or blocks. A `.txt` or `.py` with a NUL
//! byte (which classifies as `binary`) is never parsed. Parsing never
//! writes to the file (F4). The
//! walk ([`walk_and_write`]), the directory-upsert path and the
//! debounced batch-apply path ([`apply_change_batch`]) all go through
//! the same [`write_unit`], so they classify identically.
//!
//! Size cap (G7): a file over [`crate::file_kind::MAX_INDEXED_BYTES`]
//! (20 MiB) is indexed by name only. Its size comes from `metadata()`
//! before any read of the file. It keeps its real kind: the kind comes
//! from the same name-plus-first-8-KB classification as any other file
//! (one bounded `read_head`, never more than the sniff window), so an
//! unknown-extension file with a NUL in its head is `binary`, not
//! `text-other`. It is never parsed (no links, tags or blocks), gets
//! `too_large = 1`, and one `info` line names the path and size. A
//! file of exactly the cap is parsed normally.
//!
//! Interrupted scans (E11): [`scan_source`] and [`rescan_source`]
//! record "started" in the cache's `scan_state` table before they
//! touch any row, and "finished" only after the walk completes. A
//! process killed in between leaves the source "started, never
//! finished"; `tephrad` rescans such a source from the top on its next
//! start (see `main.rs`), with no resume logic.
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
//!
//! Chunk hand-off (R18 review finding): every chunk's `BEGIN
//! IMMEDIATE`/commit cycle — in a full scan, a forced rescan, and a
//! debounced batch-apply alike — is bracketed by
//! `crate::writer_gate::WriterGate::acquire` rather than an
//! unconditional sleep. Before this fix, `walk_and_write` slept a
//! fixed duration after every single chunk commit regardless of
//! whether anything else was waiting, to give another connection a
//! real chance at the lock between chunks (R16) — correct, but paid
//! on every chunk of every scan, even a completely uncontended one
//! (measured 5.3x slower on a 100k-file scan, worse on macOS due to
//! timer coalescing). The writer gate makes the hand-off itself
//! deterministic and free when uncontended instead: see
//! `crate::writer_gate`'s module doc comment for the full rationale.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Transaction, TransactionBehavior};
use tephra_cache::{Cache, NewUnit};

use crate::file_kind::{self, FileKind, MAX_INDEXED_BYTES};
use crate::ignore::IgnoreRules;
use crate::refactor;
use crate::writer_gate::{self, WriterGate, WriterGateGuard};

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
    /// The source's own root could not be listed because the OS
    /// refused (E13: macOS Full Disk Access not granted, or a mode
    /// that forbids it). The scan stops here and writes nothing: an
    /// unreadable root is never "an empty folder", so the cached
    /// rows must not be cleared. The caller marks the source
    /// `permission_denied`.
    PermissionDenied(PathBuf),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io(e) => write!(f, "{e}"),
            ScanError::Walk(e) => write!(f, "{e}"),
            ScanError::Cache(e) => write!(f, "{e}"),
            ScanError::Sqlite(e) => write!(f, "{e}"),
            ScanError::PermissionDenied(path) => {
                write!(f, "permission denied reading {}", path.display())
            }
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

/// Acquires `gate` (R18) and then opens a `BEGIN IMMEDIATE`
/// transaction against `cache`. The returned guard must be kept alive
/// for exactly as long as the transaction it opened is open, and
/// dropped only once that transaction has committed (or failed) —
/// dropping it releases the gate and wakes the next in-process writer
/// waiting on it, so releasing it before the commit lands would let
/// another writer in while this chunk's write lock is technically
/// still held, and releasing it long after the commit (e.g. only at
/// the end of a whole scan) would defeat the point of acquiring it
/// per chunk in the first place.
fn open_chunk<'c, 'g>(
    cache: &'c Cache,
    gate: &'g WriterGate,
) -> Result<(WriterGateGuard<'g>, Transaction<'c>), ScanError> {
    let permit = gate.acquire();
    let tx = begin_immediate(cache)?;
    Ok((permit, tx))
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

/// Seconds since the epoch of `metadata`'s modification time.
fn metadata_mtime(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs() as i64)
}

/// Reads at most `MAX_INDEXED_BYTES + 1` bytes of `path`: enough to
/// tell a file that grew past the cap after its `metadata()` check
/// without ever holding more than the cap in memory.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    fs::File::open(path)?
        .take(MAX_INDEXED_BYTES + 1)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// Whether `abs`, or any directory between `root` and it, is a symlink.
/// Checked component by component with `symlink_metadata`, so nothing
/// is followed. A component that doesn't exist (a path that was just
/// deleted, or lies below a deleted symlink) is not a symlink: there is
/// nothing there to read, and the caller treats it as a removal.
///
/// The root itself is not checked: it is whatever path the user
/// registered, and may legitimately be reached through a link.
fn passes_through_symlink(root: &Path, abs: &Path) -> bool {
    let Ok(rel) = abs.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Classifies one file and writes its unit row (and, for a prose or
/// code file, its link/tag/block/heading rows) under the given
/// `source_id`. Returns
/// `Ok(None)` (not an error) for a file whose metadata or leading
/// bytes couldn't be read at all (permission denied, deleted
/// mid-walk) — a single unreadable file must not abort the rest of the
/// scan or the whole rescan transaction (E1/E5: the walk and the
/// change-apply batch both keep going past one bad file), and is
/// skipped with a warning whatever its kind. A symlink is skipped the
/// same way, without a warning and without being read: a unit is a
/// regular file inside the source, so a link can never pull in content
/// from outside the root.
///
/// Order of operations (G7): `metadata().len()` first, before any read.
/// The first 8 KB are then read (one bounded read, whatever the size)
/// and classified ([`file_kind::classify`]), so a too-large file keeps
/// its real kind. Over [`MAX_INDEXED_BYTES`] that is all that is read:
/// the file is stored by name only, with that kind and `too_large = 1`,
/// and is never parsed. Otherwise only a file that classifies
/// as `prose` or `code` is read in full and parsed (a UTF-16 prose
/// file with a BOM is decoded first; a code file goes to the code
/// parser, which has its own time budget — a file it gives up on is
/// kept as a `code` unit with no links, and the timeout is logged).
/// A prose or code file whose later bytes turn out not to be valid
/// UTF-8 is stored as `binary`, by name only, with a warning — it is
/// still a file in the source. Every other kind is stored with name,
/// kind, size and mtime only.
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
/// pre-existing row, regardless of how it got there. (That also
/// flips a file's kind cleanly: the old row's links/tags/blocks go
/// with it, so a prose file overwritten with binary bytes keeps no
/// stale link rows.)
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
    let file_name = abs_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&rel_path);

    // G7: the size comes from metadata, before any read. A symlink is
    // never indexed (see the doc comment above), so look at the entry
    // itself first: `fs::metadata` would follow it.
    match fs::symlink_metadata(abs_path) {
        Ok(m) if m.file_type().is_symlink() => return Ok(None),
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), error = %e, "failed to stat file during scan; skipping");
            return Ok(None);
        }
    }
    let metadata = match fs::metadata(abs_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), error = %e, "failed to stat file during scan; skipping");
            return Ok(None);
        }
    };
    let mtime = metadata_mtime(&metadata);
    let mut size = metadata.len();
    let mut too_large = size > MAX_INDEXED_BYTES;

    // The kind comes from the name and the first 8 KB, for a too-large
    // file too (G7: it keeps its real kind). `read_head` is bounded, so
    // this never reads past the sniff window whatever the file's size.
    let head = match file_kind::read_head(abs_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), error = %e, "failed to read file during scan; skipping");
            return Ok(None);
        }
    };
    let mut kind = file_kind::classify(file_name, &head);
    let mut parsed = None;
    if !too_large && matches!(kind, FileKind::Prose | FileKind::Code) {
        let bytes = match read_capped(abs_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %abs_path.display(), error = %e, "failed to read file during scan; skipping");
                return Ok(None);
            }
        };
        size = bytes.len() as u64;
        if size > MAX_INDEXED_BYTES {
            // Grew past the cap between the stat and the read.
            too_large = true;
        } else {
            match parse_file(kind, abs_path, file_name, &head, &bytes) {
                Ok(p) => parsed = p,
                Err(e) => {
                    tracing::warn!(path = %abs_path.display(), error = %e, kind = kind.as_str(), "file is not valid UTF-8; indexed by name only as binary");
                    kind = FileKind::Binary;
                }
            }
        }
    }
    if too_large {
        tracing::info!(
            path = %abs_path.display(),
            size_bytes = size,
            limit_bytes = MAX_INDEXED_BYTES,
            "file is over the size limit; indexed by name only"
        );
    }

    // R15: delete any existing row for this path before inserting the
    // freshly built one, so this is always an upsert.
    delete_existing_unit(cache, source_id, &rel_path)?;

    let mut stats = ScanStats {
        files_seen: 0,
        files_ignored: 0,
        units_written: 1,
        links_written: 0,
        tags_written: 0,
        blocks_written: 0,
    };

    let Some(parsed) = parsed else {
        cache.insert_unit(&NewUnit {
            source_id,
            rel_path: &rel_path,
            mtime,
            frontmatter_json: None,
            kind: kind.as_str(),
            size: Some(size as i64),
            too_large,
        })?;
        return Ok(Some(stats));
    };

    // E7: an ID that appears twice is stored twice (below) and
    // reported here. E12: so is a code parse that was given up on.
    // The file itself is never touched by a scan.
    for diag in &parsed.diagnostics {
        match diag.kind {
            tephra_parser::DiagnosticKind::DuplicateBlockId => tracing::warn!(
                path = %abs_path.display(),
                line = diag.line,
                "{}",
                diag.message
            ),
            tephra_parser::DiagnosticKind::ParseTimeout
            | tephra_parser::DiagnosticKind::ParseSkippedTooLarge
            | tephra_parser::DiagnosticKind::ParseFailed
            | tephra_parser::DiagnosticKind::SyntaxErrors => tracing::warn!(
                path = %abs_path.display(),
                "{}",
                diag.message
            ),
            _ => {}
        }
    }

    let mut new_unit = parsed.to_new_unit(source_id, &rel_path, mtime);
    // `to_new_unit` says `prose`; a code unit says `code`.
    new_unit.kind = kind.as_str();
    new_unit.size = Some(size as i64);
    let unit_id = cache.insert_unit(&new_unit)?;

    for link in parsed.to_new_links(unit_id) {
        cache.insert_link(&link)?;
        stats.links_written += 1;
    }
    for tag in parsed.to_new_tags(unit_id) {
        cache.insert_tag(&tag)?;
        stats.tags_written += 1;
    }
    // A code file's symbols go in first: its `^id` block rows name the
    // symbol that owns them (G13). Prose has none.
    let mut symbol_ids = Vec::with_capacity(parsed.symbols.len());
    for symbol in parsed.to_new_symbols(unit_id) {
        symbol_ids.push(cache.insert_symbol(&symbol)?);
    }
    for block in parsed.to_new_blocks_with_symbols(unit_id, &symbol_ids) {
        cache.insert_block(&block)?;
        stats.blocks_written += 1;
    }
    // Headings feed the listing's prose title (first heading when the
    // frontmatter has none). `delete_unit` removes them with the row.
    for heading in parsed.to_new_headings(unit_id) {
        cache.insert_heading(&heading)?;
    }

    Ok(Some(stats))
}

/// The one dispatch from a file's kind to its parser. `bytes` is the
/// whole file, `head` its first bytes (what [`file_kind::classify`]
/// saw). `Ok(None)` is a kind this does not parse (or a code file with
/// no grammar, which `classify` never produces). `Err` carries the
/// reason the bytes are not text; the caller then stores the file as
/// `binary`.
///
/// - `Prose`: `parse_unit`, after decoding a UTF-16 file with a BOM,
///   else `parse_unit_bytes` (strict UTF-8).
/// - `Code`: `parse_code_unit` in the language the name or shebang
///   gives ([`file_kind::language_for`]), on strict UTF-8.
///
/// Used by every write path ([`write_unit`] is the only caller), so a
/// file is parsed the same way in a full scan, a rescan and the
/// watcher's batch.
fn parse_file(
    kind: FileKind,
    abs_path: &Path,
    file_name: &str,
    head: &[u8],
    bytes: &[u8],
) -> Result<Option<tephra_parser::ParsedUnit>, String> {
    match kind {
        FileKind::Prose => match file_kind::decode_utf16_with_bom(bytes) {
            Some(text) => Ok(Some(tephra_parser::parse_unit(abs_path, &text))),
            None => tephra_parser::parse_unit_bytes(abs_path, bytes)
                .map(Some)
                .map_err(|e| e.to_string()),
        },
        FileKind::Code => {
            let Some(lang) = file_kind::language_for(file_name, head) else {
                return Ok(None);
            };
            let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            Ok(Some(tephra_parser::parse_code_unit(abs_path, lang, text)))
        }
        FileKind::Document | FileKind::TextOther | FileKind::Binary => Ok(None),
    }
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
/// file is classified and written via `cache`; only prose files
/// (links, tags, blocks) and code files (comment/docstring links) are
/// parsed (G2, G4).
///
/// A single unreadable directory entry (e.g. a permission-denied
/// subdirectory) is logged and skipped rather than aborting the whole
/// walk (R2) — the same "log and keep going" treatment
/// [`write_unit`] already gives one unreadable file. The one exception
/// is the root itself: if it cannot be listed because of a permission
/// error the walk returns [`ScanError::PermissionDenied`] instead of
/// finishing "empty" (E13), so a forced rescan never wipes the cached
/// rows of a source the OS merely refuses to show it.
///
/// This never collects the full file list into memory at once: the
/// `walkdir::WalkDir` iterator yields one entry at a time, and each
/// file's rows are written to the cache before moving to the next —
/// peak memory is bounded by one file's content plus its parsed
/// structures, not by the size of the source tree.
///
/// `first_chunk` is the already-open gate permit and transaction this
/// walk's first chunk writes into (so a caller, e.g.
/// [`rescan_source`], can fold a preceding delete into that same
/// first chunk); every [`SCAN_COMMIT_CHUNK_SIZE`]th file commits it,
/// releases the gate permit, and reacquires a fresh one for the next
/// chunk (R11/R18 — see the module doc comment for the full
/// rationale), and the final (possibly partial) chunk is committed
/// (and its permit released) once the walk is exhausted.
fn walk_and_write(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
    gate: &WriterGate,
    first_chunk: (WriterGateGuard<'_>, Transaction<'_>),
) -> Result<ScanStats, ScanError> {
    let mut stats = ScanStats::default();
    let (mut permit, mut tx) = (Some(first_chunk.0), Some(first_chunk.1));
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
                // E13: the root itself refusing to be listed must not
                // read as "the folder is empty". Stop before anything
                // is committed past what the caller already holds in
                // its open first chunk (dropped, so rolled back, on
                // this return): the cached rows stay as they were.
                if walk_error_is_root_permission_denied(&e) {
                    return Err(ScanError::PermissionDenied(root.to_path_buf()));
                }
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

        if let Some(file_name) = entry.path().file_name().and_then(|n| n.to_str()) {
            if refactor::is_stale_temp_file_name(file_name) {
                // R3 review finding: a process killed between
                // `rewrite_links_atomic`'s `write_new` and `rename`
                // can leave one of these behind in the user's own
                // source tree forever, since nothing else ever
                // revisits it. Every full walk — `register`'s
                // initial scan and every `rescan` — already visits
                // every file here anyway, so sweep any stale one away
                // now rather than leaving it on disk indefinitely or
                // indexing it as if it were real note content.
                match fs::remove_file(entry.path()) {
                    Ok(()) => tracing::info!(
                        path = %entry.path().display(),
                        "removed a stale refactor temp file left behind by a killed apply_refactor"
                    ),
                    Err(e) => tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "failed to remove a stale refactor temp file"
                    ),
                }
                continue;
            }
        }

        stats.files_seen += 1;
        if ignore.is_ignored(rel) {
            stats.files_ignored += 1;
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
            // R18: drop this chunk's gate permit only after the
            // commit lands — releasing it wakes the next in-process
            // writer waiting on this database (if any) immediately
            // and deterministically, with no sleep or poll needed on
            // either side (see `crate::writer_gate`'s module doc
            // comment).
            drop(permit.take());
            let (next_permit, next_tx) = open_chunk(cache, gate)?;
            permit = Some(next_permit);
            tx = Some(next_tx);
            since_commit = 0;
        }
    }

    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(stats)
}

/// Whether a walk error is the walk's own root (depth 0) failing with a
/// permission error, as opposed to one unreadable subdirectory or file
/// further down (which is logged and skipped, E1/R2).
fn walk_error_is_root_permission_denied(err: &walkdir::Error) -> bool {
    err.depth() == 0
        && err
            .io_error()
            .is_some_and(|io| io.kind() == io::ErrorKind::PermissionDenied)
}

/// Sweeps every stale `.{original-file-name}.tephra-tmp-{n}` leftover
/// (see [`refactor::is_stale_temp_file_name`]) out of `root`, without
/// touching the cache at all — a plain filesystem walk, safe to call
/// with no open transaction and no `WriterGate` permit held.
///
/// R8 review finding: the only sweep that previously existed ran
/// inside [`walk_and_write`] above, so it only ever happened on
/// `source.register`'s initial scan or a `source.rescan` — neither of
/// which a plain daemon restart performs (`SourceRegistry::new` only
/// re-reads the existing cache rows; it never walks the filesystem,
/// and re-registering an already-registered source is refused). A
/// file left behind by a process killed between `write_new` and
/// `rename` in `crate::refactor::rewrite_links_atomic_with` therefore
/// survived a restart indefinitely, with no RPC call able to trigger
/// a sweep either. This is meant to be called once per registered
/// source at daemon startup (see `main.rs`), independent of
/// `register`/`rescan`.
///
/// Directory-level ignore rules ([`IgnoreRules::is_ignored`]) are
/// still honored for *pruning which directories are even walked*
/// (consistent with, and for the same performance reason as,
/// [`walk_and_write`]'s own walk) — but a stale temp file is removed
/// purely by its own name, never gated on whether the file itself
/// would be ignored, since it was never real note content to begin
/// with.
pub fn sweep_stale_temp_files(root: &Path) -> io::Result<usize> {
    let ignore = IgnoreRules::load(root)?;
    let mut removed = 0usize;

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
                tracing::warn!(error = %e, "failed to walk a directory entry while sweeping stale refactor temp files; skipping");
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let Some(file_name) = entry.path().file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !refactor::is_stale_temp_file_name(file_name) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {
                removed += 1;
                tracing::info!(
                    path = %entry.path().display(),
                    "removed a stale refactor temp file left behind by a killed apply_refactor"
                );
            }
            Err(e) => tracing::warn!(
                path = %entry.path().display(),
                error = %e,
                "failed to remove a stale refactor temp file"
            ),
        }
    }

    Ok(removed)
}

/// Records "scan started" for `source_id` (E11) in its own committed
/// write, *before* the scan's first chunk opens — so a process killed
/// at any point from here on, even before that first chunk commits,
/// leaves the source marked unfinished. Takes the writer gate like
/// every other write so it never races an in-process chunk.
fn record_scan_started(cache: &Cache, gate: &WriterGate, source_id: i64) -> Result<(), ScanError> {
    retry_on_busy(|| {
        let _permit = gate.acquire();
        cache.mark_scan_started(source_id).map_err(ScanError::from)
    })
}

/// Records "scan finished" for `source_id`, only reached once the walk
/// returned `Ok`. A failed or killed scan never gets here, which is
/// what leaves it unfinished.
fn record_scan_finished(cache: &Cache, gate: &WriterGate, source_id: i64) -> Result<(), ScanError> {
    retry_on_busy(|| {
        let _permit = gate.acquire();
        cache.mark_scan_finished(source_id).map_err(ScanError::from)
    })
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
    let gate = writer_gate::for_path(cache.path());
    record_scan_started(cache, &gate, source_id)?;
    let first_chunk = open_chunk(cache, &gate)?;
    let stats = walk_and_write(cache, source_id, root, ignore, &gate, first_chunk)?;
    record_scan_finished(cache, &gate, source_id)?;
    Ok(stats)
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
    let gate = writer_gate::for_path(cache.path());
    record_scan_started(cache, &gate, source_id)?;
    let first_chunk = open_chunk(cache, &gate)?;
    cache.delete_units_for_source(source_id)?;
    let stats = walk_and_write(cache, source_id, root, ignore, &gate, first_chunk)?;
    record_scan_finished(cache, &gate, source_id)?;
    Ok(stats)
}

/// Walks an existing directory that arrived as one changed path (a
/// create or a move-in: R4) and upserts every non-ignored
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
        if let Some(file_stats) = write_unit(cache, source_id, root, entry.path())? {
            stats.accumulate(file_stats);
            wrote_unit = true;
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
///   freshly classified one (and, for a prose or code file, a fresh
///   parse, through the same [`parse_file`] as a full scan), so a
///   file that changed kind leaves no stale rows of its old kind. A
///   file that can't be read is removed and skipped with a warning.
/// - An existing directory (a create or a move-in): walked and every
///   non-ignored file under it is upserted ([`upsert_directory`]) — a
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
    //
    // Acquired through the writer gate (R18), not held across
    // `retry_on_busy`'s retries: this function's own `?` returns drop
    // the permit immediately on any failure, before `retry_on_busy`
    // sleeps and tries again, so a batch stuck retrying past a
    // genuinely busy database never also blocks another source's
    // unrelated writes for the whole retry window.
    let gate = writer_gate::for_path(cache.path());
    let permit = gate.acquire();
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

        // T2: never read through a symlink. A changed path that is a
        // symlink, or lies below one, is not content of this source: a
        // link made inside the root to /etc must not pull /etc in
        // (`fs::metadata` and a walk's root both follow links). Drop
        // any rows already cached for it instead, so a link that was
        // indexed before this check existed is cleaned up too.
        if passes_through_symlink(root, abs_path) {
            let had_existing = delete_existing_unit(cache, source_id, &rel_path)?;
            let nested_deleted = cache.delete_units_under_prefix(source_id, &rel_path)?;
            if had_existing || nested_deleted > 0 {
                touched_paths.push(abs_path.clone());
            }
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
                if let Some(file_stats) = write_unit(cache, source_id, root, abs_path)? {
                    stats.accumulate(file_stats);
                    wrote_unit = true;
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
    drop(permit);
    Ok(ChangeOutcome {
        stats,
        changed_paths: touched_paths,
    })
}
