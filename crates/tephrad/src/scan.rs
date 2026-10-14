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

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

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
fn walk_and_write(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let mut stats = ScanStats::default();

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
    }

    Ok(stats)
}

/// `source.register`'s initial full scan (GP2/GP3): one transaction
/// for the whole walk (R2/GP4: "its own scan transaction"), against a
/// source with no pre-existing rows.
pub fn scan_source(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let tx_guard = cache.connection().unchecked_transaction()?;
    let stats = walk_and_write(cache, source_id, root, ignore)?;
    tx_guard.commit()?;
    Ok(stats)
}

/// `source.rescan`'s forced full re-walk: clears this source's
/// existing rows and re-walks it inside **one** transaction (R2) —
/// unlike a naive "delete, then scan" pair run as two separate
/// operations, a walk error partway through (now rare, since
/// unreadable entries are skipped rather than propagated; see
/// [`walk_and_write`]) rolls back the delete too, instead of leaving
/// the source's rows wiped with nothing re-inserted.
pub fn rescan_source(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let tx_guard = cache.connection().unchecked_transaction()?;
    cache.delete_units_for_source(source_id)?;
    let stats = walk_and_write(cache, source_id, root, ignore)?;
    tx_guard.commit()?;
    Ok(stats)
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
pub fn apply_change_batch(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    changed: &HashSet<PathBuf>,
    ignore: &IgnoreRules,
) -> Result<ChangeOutcome, ScanError> {
    let mut stats = ScanStats::default();
    let mut touched_paths: Vec<PathBuf> = Vec::new();

    // Brackets the whole batch in one BEGIN/COMMIT (E5): `cache`'s
    // typed insert/delete helpers below still operate through
    // `cache`'s own `&self` methods, but since those execute against
    // the very same underlying SQLite connection this transaction
    // guard was opened on, every one of them runs inside it. Dropped
    // without `commit()` (e.g. via an early `?` return below) rolls
    // the whole batch back instead of leaving a partial write.
    let tx_guard = cache.connection().unchecked_transaction()?;

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
