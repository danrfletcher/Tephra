//! The walk-parse-insert pipeline shared by an initial full scan
//! (GP2/GP3) and a forced rescan (`source.rescan`), plus the batched
//! change-apply path used by the debounced watcher (GP6/E5).
//!
//! Both entry points go through `tephra_parser::parse_unit_bytes` for
//! extraction and write through `tephra_cache`'s typed row shapes
//! (`NewUnit`/`NewLink`/`NewTag`/`NewBlock`), so a unit's cache row
//! shape here is always in sync with what the parser crate produces
//! — there is no separate re-description of that schema in this
//! module.

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

/// Parses one file and writes its unit/link/tag/block rows, under the
/// given `source_id`. Returns `Ok(None)` (not an error) for a file
/// that can't be read as UTF-8 text or whose bytes couldn't be read
/// at all — a single unreadable file must not abort the rest of the
/// scan or the whole rescan transaction (E1/E5: the walk and the
/// change-apply batch both keep going past one bad file).
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

/// Walks `root` (streaming — see the module doc and E1), applying
/// `ignore` before descending into any directory so an ignored
/// subtree's files are never even listed, let alone parsed. Every
/// non-ignored file is parsed and its rows written via `cache`.
///
/// This never collects the full file list into memory at once: the
/// `walkdir::WalkDir` iterator yields one entry at a time, and each
/// file's rows are written to the cache before moving to the next —
/// peak memory is bounded by one file's content plus its parsed
/// structures, not by the size of the source tree.
pub fn scan_source(
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
        let entry = entry?;
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

        if let Some(file_stats) = write_unit(cache, source_id, root, entry.path())? {
            stats.units_written += file_stats.units_written;
            stats.links_written += file_stats.links_written;
            stats.tags_written += file_stats.tags_written;
            stats.blocks_written += file_stats.blocks_written;
        }
    }

    Ok(stats)
}

/// Deletes the one unit row at `source_id`/`rel_path` (if any) and
/// its cascading links/tags/blocks, via `cache`'s typed helpers.
fn delete_existing_unit(cache: &Cache, source_id: i64, rel_path: &str) -> Result<(), ScanError> {
    if let Some(existing) = cache.get_unit_by_rel_path(source_id, rel_path)? {
        cache.delete_unit(existing.id)?;
    }
    Ok(())
}

/// Applies one debounce-coalesced batch of changed absolute paths
/// (GP6) as a single cache transaction (E5: "coalesces into a single
/// rescan transaction against the cache, never one write per
/// event") — re-parsing and writing only the file(s) that actually
/// changed, never the whole source.
///
/// For each changed path: its existing unit row (if any) is deleted
/// first, then — if the path still exists on disk, is a file, and is
/// not ignored — it is re-parsed and reinserted. A path that no
/// longer exists (deleted) simply has its row removed, with nothing
/// reinserted.
pub fn apply_change_batch(
    cache: &Cache,
    source_id: i64,
    root: &Path,
    changed: &HashSet<PathBuf>,
    ignore: &IgnoreRules,
) -> Result<ScanStats, ScanError> {
    let mut stats = ScanStats::default();

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

        delete_existing_unit(cache, source_id, &rel_path)?;

        let is_file = fs::metadata(abs_path).map(|m| m.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }

        stats.files_seen += 1;
        if let Some(file_stats) = write_unit(cache, source_id, root, abs_path)? {
            stats.units_written += file_stats.units_written;
            stats.links_written += file_stats.links_written;
            stats.tags_written += file_stats.tags_written;
            stats.blocks_written += file_stats.blocks_written;
        }
    }

    tx_guard.commit()?;
    Ok(stats)
}
