//! `unit.list` (G1): one folder of a source, or the whole tree below
//! it, as `{id, name, rel_path, kind, size, mtime, title?}` entries.
//!
//! Where the data comes from:
//!
//! - **Files** (`kind`, `size`, `mtime`, `title`) always come from the
//!   cache, through a read-only connection
//!   ([`tephra_cache::Cache::open_read_only`]): the listing can't write
//!   and, under WAL, never blocks a scan's writer. A file the scan or
//!   watcher hasn't indexed yet is simply absent, never half-filled.
//! - **Folders** of an *online* source come from the disk, so an empty
//!   folder, or one whose files are all ignored, still shows (size 0),
//!   a folder's mtime is its real one, and the ignore rules
//!   ([`crate::ignore::IgnoreRules`], read fresh from `.tephraignore`)
//!   hide ignored folders. A folder's size is its number of direct
//!   children that aren't ignored.
//! - An **offline** source (its root is missing, or the stored status
//!   says so) is listed from the cache alone: folders are derived from
//!   the cached `rel_path` prefixes, sized by their distinct direct
//!   children, with a null mtime. The response says `offline: true`.
//!
//! The requested `folder` is untrusted input. [`normalize_folder`]
//! rejects `..` and absolute paths before any filesystem access, and an
//! online listing then resolves the folder and refuses one that leaves
//! the source root through a symlink; symlinked directories below it
//! are never followed. A folder must also be spelled exactly as it is
//! on disk (case, Unicode normalization) and be reached without a
//! symlink, even one that stays inside the root: otherwise the id and
//! the cache lookup, both byte-exact, would disagree with the disk, so
//! it is "no such folder".

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;
use tephra_cache::{compare_names, Cache, FolderEntry, FolderState, SourceStatus, FOLDER_KIND};

use crate::ignore::IgnoreRules;
use crate::registry::{probe_path, PathProbe};

/// One `unit.list` entry. `title` is left out (not null) when absent so
/// a client falls back to `name`. `size` and `mtime` are `null` when
/// unknown: a folder of an offline source has no mtime, and a row kept
/// from a v1 cache has no size until the next rescan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitEntryWire {
    pub id: String,
    pub name: String,
    pub rel_path: String,
    pub kind: String,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The `unit.list` result. `offline` and `needs_rescan` are on the
/// response, not on each entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitListWire {
    pub source: String,
    /// The folder listed, normalized: `""` is the source root.
    pub folder: String,
    pub recursive: bool,
    pub offline: bool,
    /// Some listed file comes from a cache that was rebuilt from v1 and
    /// has not been rescanned, so its kind is a guess from the extension
    /// and its size is null.
    pub needs_rescan: bool,
    pub entries: Vec<UnitEntryWire>,
}

/// Why a `unit.list` call failed. The dispatcher maps `InvalidParams`
/// to `INVALID_PARAMS` and `Source` to `SOURCE_ERROR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitListError {
    InvalidParams(String),
    Source(String),
}

/// Normalizes a client-supplied `folder` to a source-relative path with
/// `/` separators and no leading, trailing, repeated or `.` components;
/// `""` is the source root.
///
/// - `""`, `"/"`, `"."` and a run of slashes all mean the root.
/// - A trailing slash is ignored (`Attachments/` is `Attachments`).
/// - Any other path starting with `/` is absolute: an error, as is any
///   `..` component or a NUL byte.
///
/// Bytes are kept exactly as given: NFC and NFD spellings stay
/// different, and names are never case-folded.
pub fn normalize_folder(raw: &str) -> Result<String, String> {
    if raw.contains('\0') {
        return Err("\"folder\" must not contain a NUL byte".to_string());
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(String::new());
    }
    if trimmed.starts_with('/') {
        return Err(
            "\"folder\" must be relative to the source root, not an absolute path".to_string(),
        );
    }
    let mut parts = Vec::new();
    for component in trimmed.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err("\"folder\" must not contain \"..\"".to_string()),
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

/// Lists `folder` of the named source (see the module doc comment).
/// `cache_path` is the daemon's `cache.db`; a fresh read-only
/// connection is opened for the call.
pub fn list_units(
    cache_path: &Path,
    source: &str,
    folder: &str,
    recursive: bool,
) -> Result<UnitListWire, UnitListError> {
    let folder = normalize_folder(folder).map_err(UnitListError::InvalidParams)?;

    let cache = Cache::open_read_only(cache_path).map_err(source_error)?;
    let row = cache
        .get_source(source)
        .map_err(source_error)?
        .ok_or_else(|| UnitListError::Source(format!("no such source: {source}")))?;
    let root = Path::new(&row.path);

    // `Missing` is an unmounted or moved root the watcher hasn't noticed
    // yet: the cache is all there is, same as for a stored-offline
    // source. A root the OS won't list (`permission_denied`) is read
    // from the cache too, but isn't called offline.
    let probe = probe_path(root);
    let offline = row.status == SourceStatus::Offline || probe == PathProbe::Missing;
    let from_disk = !offline && probe == PathProbe::Ok;

    let listing = cache
        .list_folder(source, &folder, recursive)
        .map_err(source_error)?;

    let entries = if from_disk {
        list_online(source, root, &folder, recursive, listing.entries)?
    } else {
        match listing.state {
            FolderState::Missing => return Err(no_such_folder(&folder)),
            FolderState::File => return Err(not_a_folder(&folder)),
            FolderState::Folder => {}
        }
        listing
            .entries
            .into_iter()
            .map(|e| wire_entry(source, e))
            .collect()
    };

    Ok(UnitListWire {
        source: source.to_string(),
        folder,
        recursive,
        offline,
        needs_rescan: listing.needs_rescan,
        entries,
    })
}

fn source_error(e: impl std::fmt::Display) -> UnitListError {
    UnitListError::Source(e.to_string())
}

fn no_such_folder(folder: &str) -> UnitListError {
    UnitListError::Source(format!("no such folder: {folder}"))
}

fn not_a_folder(folder: &str) -> UnitListError {
    UnitListError::Source(format!("not a folder: {folder}"))
}

fn wire_entry(source: &str, e: FolderEntry) -> UnitEntryWire {
    UnitEntryWire {
        id: format!("@{source}:{}", e.rel_path),
        name: e.name,
        rel_path: e.rel_path,
        kind: e.kind,
        size: e.size,
        mtime: e.mtime,
        title: e.title,
    }
}

/// An online source: validates `folder` on disk, then merges the disk's
/// folders with the cache's file entries.
fn list_online(
    source: &str,
    root: &Path,
    folder: &str,
    recursive: bool,
    cached: Vec<FolderEntry>,
) -> Result<Vec<UnitEntryWire>, UnitListError> {
    let ignore = IgnoreRules::load(root).map_err(source_error)?;

    // An ignored folder is as good as absent: nothing under it is ever
    // indexed, and it must never appear.
    if !folder.is_empty() && ignore.is_ignored(Path::new(folder)) {
        return Err(no_such_folder(folder));
    }

    let canonical_root = fs::canonicalize(root).map_err(source_error)?;
    let base = match fs::canonicalize(canonical_root.join(folder)) {
        Ok(p) => p,
        Err(e) if is_not_found(&e) => return Err(no_such_folder(folder)),
        Err(e) => return Err(source_error(e)),
    };
    // A symlink inside the source that points outside it.
    if !base.starts_with(&canonical_root) {
        return Err(UnitListError::InvalidParams(format!(
            "\"folder\" resolves outside the source root: {folder}"
        )));
    }
    // The disk may resolve a spelling that isn't the real name: a
    // different case or Unicode normalization (APFS), or an in-root
    // symlink. Ids, rel_paths and the cache lookup are keyed by exact
    // bytes, so only the real, symlink-free path counts as the folder.
    check_exact_path(&canonical_root, folder)?;
    match fs::metadata(&base) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(not_a_folder(folder)),
        Err(e) if is_not_found(&e) => return Err(no_such_folder(folder)),
        Err(e) => return Err(source_error(e)),
    }

    // The cache's file entries, grouped by the folder they sit in.
    let mut files_by_dir: HashMap<String, Vec<FolderEntry>> = HashMap::new();
    for entry in cached.into_iter().filter(|e| !e.is_folder()) {
        if ignore.is_ignored(Path::new(&entry.rel_path)) {
            continue;
        }
        let parent = entry
            .rel_path
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        files_by_dir.entry(parent).or_default().push(entry);
    }

    let mut walk = Walk {
        source,
        ignore: &ignore,
        recursive,
        files_by_dir,
        out: Vec::new(),
    };
    walk.level(&base, folder)
        .map_err(|e| UnitListError::Source(format!("failed to read folder {folder}: {e}")))?;
    Ok(walk.out)
}

/// Requires every component of the normalized, non-empty `folder` to be
/// an entry of its parent directory under exactly that name (byte for
/// byte), and not a symlink. A spelling the filesystem merely resolves
/// (another case or normalization, a symlink inside the root) is "no
/// such folder", same as a symlinked directory is never a folder entry
/// of a walk. A final component that is a file, or a symlink to one, is
/// "not a folder".
fn check_exact_path(canonical_root: &Path, folder: &str) -> Result<(), UnitListError> {
    let mut dir = canonical_root.to_path_buf();
    let mut parts = folder.split('/').filter(|p| !p.is_empty()).peekable();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        let wanted = std::ffi::OsStr::new(part);
        let found = fs::read_dir(&dir)
            .map_err(|e| {
                if is_not_found(&e) {
                    no_such_folder(folder)
                } else {
                    source_error(e)
                }
            })?
            .filter_map(Result::ok)
            .find(|e| e.file_name() == wanted);
        let Some(found) = found else {
            return Err(no_such_folder(folder));
        };
        let file_type = found.file_type().map_err(source_error)?;
        if file_type.is_dir() {
            dir.push(part);
        } else if last && !file_type.is_symlink() {
            return Err(not_a_folder(folder));
        } else if last && !fs::metadata(found.path()).is_ok_and(|m| m.is_dir()) {
            // A symlink to a file.
            return Err(not_a_folder(folder));
        } else {
            return Err(no_such_folder(folder));
        }
    }
    Ok(())
}

/// `ENOENT`, and the errors a path through a file or a symlink loop
/// gives: all of them mean "there is no such folder".
fn is_not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
        || matches!(e.raw_os_error(), Some(code) if code == libc::ENOTDIR || code == libc::ELOOP)
}

struct Walk<'a> {
    source: &'a str,
    ignore: &'a IgnoreRules,
    recursive: bool,
    files_by_dir: HashMap<String, Vec<FolderEntry>>,
    out: Vec<UnitEntryWire>,
}

/// A folder found on disk.
struct DiskFolder {
    name: String,
    mtime: Option<i64>,
}

impl Walk<'_> {
    /// Emits the entries directly in `abs_dir` (whose source-relative
    /// path is `rel_dir`) in listing order, descending after each
    /// folder entry when the listing is recursive.
    fn level(&mut self, abs_dir: &Path, rel_dir: &str) -> io::Result<()> {
        let mut folders = Vec::new();
        for entry in fs::read_dir(abs_dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if self.ignore.is_ignored(Path::new(&join(rel_dir, &name))) {
                continue;
            }
            // `file_type` doesn't follow symlinks: a symlinked directory
            // is not a folder here, so the walk can't leave the source.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let mtime = entry.metadata().ok().and_then(|m| mtime_secs(&m));
                folders.push(DiskFolder { name, mtime });
            }
        }

        let mut level: Vec<(bool, String, Level)> = folders
            .into_iter()
            .map(|f| (true, f.name.clone(), Level::Folder(f)))
            .collect();
        for file in self.files_by_dir.remove(rel_dir).unwrap_or_default() {
            level.push((false, file.name.clone(), Level::File(file)));
        }
        level.sort_by(|a, b| compare_names(a.0, &a.1, b.0, &b.1));

        for (_, _, item) in level {
            match item {
                Level::File(file) => self.out.push(wire_entry(self.source, file)),
                Level::Folder(folder) => {
                    let rel = join(rel_dir, &folder.name);
                    let abs = abs_dir.join(&folder.name);
                    let size = self.count_children(&abs, &rel);
                    self.out.push(UnitEntryWire {
                        id: format!("@{}:{rel}", self.source),
                        name: folder.name,
                        rel_path: rel.clone(),
                        kind: FOLDER_KIND.to_string(),
                        size: Some(size),
                        mtime: folder.mtime,
                        title: None,
                    });
                    if self.recursive {
                        // An unreadable subfolder lists as empty rather
                        // than failing the whole call.
                        if let Err(e) = self.level(&abs, &rel) {
                            tracing::warn!(path = %abs.display(), error = %e, "unit.list: skipping an unreadable folder");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The number of direct children of `abs_dir` that aren't ignored;
    /// 0 for a folder that can't be read.
    fn count_children(&self, abs_dir: &Path, rel_dir: &str) -> i64 {
        let Ok(read) = fs::read_dir(abs_dir) else {
            return 0;
        };
        read.filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| !self.ignore.is_ignored(Path::new(&join(rel_dir, name))))
            .count() as i64
    }
}

enum Level {
    Folder(DiskFolder),
    File(FolderEntry),
}

fn join(rel_dir: &str, name: &str) -> String {
    if rel_dir.is_empty() {
        name.to_string()
    } else {
        format!("{rel_dir}/{name}")
    }
}

fn mtime_secs(metadata: &fs::Metadata) -> Option<i64> {
    let since_epoch = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(since_epoch.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_has_several_spellings() {
        for raw in ["", "/", ".", "./", "//", "///", "./."] {
            assert_eq!(normalize_folder(raw).as_deref(), Ok(""), "{raw:?}");
        }
    }

    #[test]
    fn a_trailing_slash_and_dot_components_are_ignored() {
        assert_eq!(
            normalize_folder("Attachments/").as_deref(),
            Ok("Attachments")
        );
        assert_eq!(normalize_folder("a//b/./c//").as_deref(), Ok("a/b/c"));
        assert_eq!(normalize_folder("./a").as_deref(), Ok("a"));
    }

    #[test]
    fn dot_dot_and_absolute_paths_and_nul_are_rejected() {
        for raw in [
            "..",
            "../x",
            "a/../b",
            "a/..",
            "/etc",
            "/etc/passwd",
            "a\0b",
        ] {
            assert!(normalize_folder(raw).is_err(), "{raw:?} should be rejected");
        }
    }

    #[test]
    fn names_that_merely_contain_dots_are_fine() {
        assert_eq!(normalize_folder("..a").as_deref(), Ok("..a"));
        assert_eq!(normalize_folder("a..").as_deref(), Ok("a.."));
        assert_eq!(normalize_folder("...").as_deref(), Ok("..."));
    }

    #[test]
    fn unicode_and_spaces_are_kept_byte_exact() {
        let nfd = "cafe\u{301} notes";
        assert_eq!(normalize_folder(nfd).as_deref(), Ok(nfd));
        assert_eq!(
            normalize_folder("My Notes/x y").as_deref(),
            Ok("My Notes/x y")
        );
    }
}
