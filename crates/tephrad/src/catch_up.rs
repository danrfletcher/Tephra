//! Startup catch-up (G5): what changed on disk while `tephrad` was not
//! running, worked out by comparing each online source with its cached
//! rows, so a rename made in the meantime is found and fixed.
//!
//! The pieces here are the facts and the rules; [`crate::registry`]
//! runs them in order (`SourceRegistry::start_all`). Nothing here
//! writes to the cache or to a file.
//!
//! - [`diff`]: the cached rows against a listing of the disk. A file in
//!   both with the same size and modification time is unchanged; in
//!   both and different, edited; only on disk, new; only cached, and
//!   really gone from disk, vanished. A cached file that is not listed
//!   but is still there (now ignored, now a link, in a folder the walk
//!   could not read) is none of these: it is left alone.
//! - [`size_matches`] and [`hash_new_files`]: a vanished row has its
//!   hash from when it was indexed. A new file is hashed only when its
//!   size equals the size of some vanished file with a hash, in any
//!   source. Everything else is not read, so a start with nothing
//!   changed reads no file at all.
//!
//! What the pairing rule does with these facts is
//! [`crate::rename_detect`], the same rule the live watcher uses: a
//! unique hash pair waits the hold, an ambiguous one is no pair, a hash
//! is unique only over ALL sources together (FR21: the sources share
//! one pool, so a file gone from one and new in another is a move), and
//! there is no similarity (F5). A catch-up has no OS rename flags, so
//! every pair here is a hash pair: a file renamed and edited while down
//! (E3) is uncertain.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::file_kind::MAX_INDEXED_BYTES;
use crate::ignore::IgnoreRules;
use crate::refactor;
use crate::rename_detect::{Appeared, Identity, Vanished};
use crate::scan::{metadata_mtime, to_rel_string};
use crate::watcher::{self, WatchError, WatcherTap};

/// What the cache says about one file of a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub path: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    /// `None` for a file over the size cap and for a row that was never
    /// hashed: it cannot be paired.
    pub hash: Option<String>,
    pub identity: Option<Identity>,
    /// A provisional row restored across a rebuild: its size and
    /// modification time are not known, so it is always indexed again.
    pub provisional: bool,
}

/// What a listing of the disk says about one file of a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFile {
    pub path: String,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// One source's cached rows against its disk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Files in both with the same size and modification time.
    pub unchanged: usize,
    /// Files in both whose size or modification time differs.
    pub edited: Vec<String>,
    /// Files on disk with no cached row.
    pub new: Vec<DiskFile>,
    /// Cached files that are gone from disk.
    pub vanished: Vec<CachedFile>,
    /// Cached files the listing does not hold but that are not gone
    /// either (a stat finds them, or cannot say): a path an edited
    /// `.tephraignore` now matches, or one under a folder that is no
    /// longer listed. Nothing in the batch indexer would ever touch
    /// them (an ignored path is skipped, not deleted), so the caller
    /// drops the ones a stat still finds. A row whose stat fails
    /// is left alone: that says nothing about the file.
    pub unlisted: Vec<String>,
}

impl Diff {
    /// Whether the disk is exactly what the cache describes.
    pub fn is_empty(&self) -> bool {
        self.edited.is_empty()
            && self.new.is_empty()
            && self.vanished.is_empty()
            && self.unlisted.is_empty()
    }

    /// Every path the cache has to be brought up to date for, as
    /// absolute paths under `root`.
    pub fn changed_paths(&self, root: &Path) -> HashSet<PathBuf> {
        self.edited
            .iter()
            .map(String::as_str)
            .chain(self.new.iter().map(|f| f.path.as_str()))
            .chain(self.vanished.iter().map(|f| f.path.as_str()))
            .map(|rel| root.join(rel))
            .collect()
    }
}

/// Compares `cached` with `disk`. `is_gone` says whether a cached path
/// that the listing does not hold is really no longer on disk; only
/// such a file is vanished. Every list in the result is in path order.
pub fn diff(cached: Vec<CachedFile>, disk: Vec<DiskFile>, is_gone: impl Fn(&str) -> bool) -> Diff {
    let mut on_disk: HashMap<String, DiskFile> =
        disk.into_iter().map(|f| (f.path.clone(), f)).collect();
    let mut out = Diff::default();
    for row in cached {
        match on_disk.remove(&row.path) {
            Some(file) => {
                let same =
                    !row.provisional && row.size == Some(file.size) && row.mtime == file.mtime;
                if same {
                    out.unchanged += 1;
                } else {
                    out.edited.push(row.path);
                }
            }
            None => {
                if is_gone(&row.path) {
                    out.vanished.push(row);
                } else {
                    out.unlisted.push(row.path);
                }
            }
        }
    }
    out.new = on_disk.into_values().collect();
    out.edited.sort();
    out.new.sort_by(|a, b| a.path.cmp(&b.path));
    out.vanished.sort_by(|a, b| a.path.cmp(&b.path));
    out.unlisted.sort();
    out
}

/// The sizes a new file must have to be worth hashing: the size of a
/// vanished file that has a hash, in any of `diffs`.
pub fn size_matches<'a>(diffs: impl IntoIterator<Item = &'a Diff>) -> HashSet<u64> {
    diffs
        .into_iter()
        .flat_map(|d| d.vanished.iter())
        .filter(|v| v.hash.is_some())
        .filter_map(|v| v.size)
        .collect()
}

/// Reads and hashes the file at `path` as the index does (SHA-256 of
/// its bytes). `None` for a file over the size cap: identity only, no
/// hash pairing (FR3).
pub fn hash_file(path: &Path) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    fs::File::open(path)?
        .take(MAX_INDEXED_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INDEXED_BYTES {
        return Ok(None);
    }
    Ok(Some(tephra_parser::content_hash_bytes(&buf)))
}

/// Hashes the new files of `diff` whose size is in `sizes` and returns
/// `path -> hash`. `hash` is the only thing that reads a file; a file
/// over the cap, or one that cannot be read, gets no entry.
pub fn hash_new_files(
    diff: &Diff,
    root: &Path,
    sizes: &HashSet<u64>,
    hash: &dyn Fn(&Path) -> io::Result<Option<String>>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for file in &diff.new {
        if file.size > MAX_INDEXED_BYTES || !sizes.contains(&file.size) {
            continue;
        }
        match hash(&root.join(&file.path)) {
            Ok(Some(h)) => {
                out.insert(file.path.clone(), h);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(path = %file.path, error = %e, "catch-up could not read a new file to hash it");
            }
        }
    }
    out
}

/// The vanished side of `diff` as the pairing rule takes it.
pub fn vanished_facts(diff: &Diff) -> Vec<Vanished> {
    diff.vanished
        .iter()
        .map(|v| Vanished {
            source: String::new(),
            path: v.path.clone(),
            identity: v.identity,
            hash: v.hash.clone(),
            os: None,
        })
        .collect()
}

/// The new side of `diff` as the pairing rule takes it: a hash only for
/// a file that was hashed (`hashes`).
pub fn appeared_facts(diff: &Diff, hashes: &HashMap<String, String>) -> Vec<Appeared> {
    diff.new
        .iter()
        .map(|f| Appeared {
            source: String::new(),
            path: f.path.clone(),
            identity: None,
            hash: hashes.get(&f.path).cloned(),
            os: None,
        })
        .collect()
}

// ---------------------------------------------------------------
// Reading the disk and the cache
// ---------------------------------------------------------------

/// The cached rows of `source` as [`CachedFile`]s.
pub fn cached_files(
    cache: &tephra_cache::Cache,
    source: &str,
) -> Result<Vec<CachedFile>, tephra_cache::CacheError> {
    Ok(cache
        .units_with_status_for_source(source)?
        .into_iter()
        .map(|u| {
            let u = u.unit;
            CachedFile {
                identity: match (u.device, u.inode) {
                    (Some(device), Some(inode)) => Some(Identity { device, inode }),
                    _ => None,
                },
                path: u.rel_path,
                size: u.size.and_then(|s| u64::try_from(s).ok()),
                mtime: u.mtime,
                hash: u.hash,
                provisional: u.needs_rescan,
            }
        })
        .collect())
}

/// Every file a scan of `root` would index, with its size and
/// modification time: the same walk as a full scan (ignored folders
/// pruned, links not followed, a stale refactor temp file not a file of
/// the source). A root that cannot be listed is an error: it must never
/// read as an empty folder.
pub fn list_disk(root: &Path, ignore: &IgnoreRules) -> io::Result<Vec<DiskFile>> {
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            match entry.path().strip_prefix(root) {
                Ok(rel) => !ignore.is_ignored(rel),
                Err(_) => true,
            }
        });
    let mut out = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if e.depth() == 0 => {
                return Err(e
                    .into_io_error()
                    .unwrap_or_else(|| io::Error::other("source root could not be walked")));
            }
            Err(e) => {
                tracing::warn!(error = %e, "catch-up could not list a directory entry; skipping");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name_is_temp = entry
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(refactor::is_stale_temp_file_name);
        if name_is_temp {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if ignore.is_ignored(rel) {
            continue;
        }
        let Some(path) = to_rel_string(root, entry.path()) else {
            continue;
        };
        let Ok(meta) = fs::metadata(entry.path()) else {
            continue;
        };
        out.push(DiskFile {
            path,
            size: meta.len(),
            mtime: metadata_mtime(&meta),
        });
    }
    Ok(out)
}

/// Whether the file `rel` of `root` is really not there: a stat that
/// says so. A stat that fails any other way (a folder that cannot be
/// read) says nothing, so the file is not called gone.
pub fn is_gone(root: &Path, rel: &str) -> bool {
    matches!(
        fs::symlink_metadata(root.join(rel)),
        Err(e) if e.kind() == io::ErrorKind::NotFound
    )
}

/// Drops the cached rows of `unlisted` paths that a stat still finds on
/// disk (see [`Diff::unlisted`]) and returns their absolute paths. A
/// path whose stat fails is left alone.
pub fn drop_unlisted(
    cache: &tephra_cache::Cache,
    source_id: i64,
    root: &Path,
    unlisted: &[String],
) -> Result<Vec<PathBuf>, tephra_cache::CacheError> {
    let mut dropped = Vec::new();
    for rel in unlisted {
        let abs = root.join(rel);
        if fs::symlink_metadata(&abs).is_err() {
            continue;
        }
        if let Some(row) = cache.get_unit_by_rel_path(source_id, rel)? {
            cache.delete_unit(row.id)?;
            dropped.push(abs);
        }
    }
    Ok(dropped)
}

/// How a boot starts a source's watcher; a test replaces it.
pub type StartTap<'a> =
    dyn Fn(&Path, Arc<RwLock<IgnoreRules>>) -> Result<WatcherTap, WatchError> + Sync + 'a;

/// What a boot hands the catch-up that a test replaces: how a new file
/// is hashed, and how a source's watcher is started.
pub struct BootHooks<'a> {
    pub hash_file: &'a (dyn Fn(&Path) -> io::Result<Option<String>> + Sync),
    pub start_tap: &'a StartTap<'a>,
}

fn real_hash(path: &Path) -> io::Result<Option<String>> {
    hash_file(path)
}

fn real_tap(root: &Path, ignore: Arc<RwLock<IgnoreRules>>) -> Result<WatcherTap, WatchError> {
    watcher::start(root.to_path_buf(), ignore)
}

impl Default for BootHooks<'_> {
    fn default() -> Self {
        BootHooks {
            hash_file: &real_hash,
            start_tap: &real_tap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(path: &str, size: u64, mtime: i64, hash: Option<&str>) -> CachedFile {
        CachedFile {
            path: path.to_string(),
            size: Some(size),
            mtime: Some(mtime),
            hash: hash.map(str::to_string),
            identity: None,
            provisional: false,
        }
    }

    fn disk(path: &str, size: u64, mtime: i64) -> DiskFile {
        DiskFile {
            path: path.to_string(),
            size,
            mtime: Some(mtime),
        }
    }

    fn gone_all(_: &str) -> bool {
        true
    }

    fn paths(files: &[CachedFile]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    // ---- diff ------------------------------------------------------

    #[test]
    fn a_cached_file_missing_from_the_listing_but_still_there_is_unlisted_not_vanished() {
        let d = diff(
            vec![
                cached("a.md", 1, 1, Some("h")),
                cached("b.md", 1, 1, Some("g")),
            ],
            vec![],
            |rel| rel == "b.md",
        );
        assert_eq!(d.unlisted, vec!["a.md"]);
        assert_eq!(paths(&d.vanished), vec!["b.md"]);
        assert!(!d.is_empty());
    }

    #[test]
    fn diff_sorts_each_file_into_one_class() {
        // (name, cached, disk, unchanged, edited, new, vanished)
        type Case = (
            &'static str,
            Vec<CachedFile>,
            Vec<DiskFile>,
            usize,
            Vec<&'static str>,
            Vec<&'static str>,
            Vec<&'static str>,
        );
        let cases: Vec<Case> = vec![
            (
                "nothing changed",
                vec![cached("a.md", 5, 10, Some("h"))],
                vec![disk("a.md", 5, 10)],
                1,
                vec![],
                vec![],
                vec![],
            ),
            (
                "size changed",
                vec![cached("a.md", 5, 10, Some("h"))],
                vec![disk("a.md", 6, 10)],
                0,
                vec!["a.md"],
                vec![],
                vec![],
            ),
            (
                "mtime changed",
                vec![cached("a.md", 5, 10, Some("h"))],
                vec![disk("a.md", 5, 11)],
                0,
                vec!["a.md"],
                vec![],
                vec![],
            ),
            (
                "only on disk",
                vec![],
                vec![disk("b.md", 1, 1)],
                0,
                vec![],
                vec!["b.md"],
                vec![],
            ),
            (
                "only cached and gone",
                vec![cached("c.md", 1, 1, Some("h"))],
                vec![],
                0,
                vec![],
                vec![],
                vec!["c.md"],
            ),
            (
                "renamed",
                vec![cached("old.md", 3, 7, Some("h"))],
                vec![disk("new.md", 3, 7)],
                0,
                vec![],
                vec!["new.md"],
                vec!["old.md"],
            ),
            (
                "a mix, in path order",
                vec![
                    cached("z.md", 1, 1, None),
                    cached("a.md", 1, 1, None),
                    cached("m.md", 1, 1, None),
                ],
                vec![
                    disk("a.md", 1, 1),
                    disk("m.md", 2, 1),
                    disk("y.md", 1, 1),
                    disk("b.md", 1, 1),
                ],
                1,
                vec!["m.md"],
                vec!["b.md", "y.md"],
                vec!["z.md"],
            ),
        ];
        for (name, cached, disk, unchanged, edited, new, vanished) in cases {
            let d = diff(cached, disk, gone_all);
            assert_eq!(d.unchanged, unchanged, "{name}");
            assert_eq!(d.edited, edited, "{name}");
            assert_eq!(
                d.new.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
                new,
                "{name}"
            );
            assert_eq!(paths(&d.vanished), vanished, "{name}");
        }
    }

    #[test]
    fn a_cached_file_that_is_still_there_but_not_listed_is_not_vanished() {
        // Now ignored, now a link, or in a folder the walk could not read.
        let d = diff(
            vec![
                cached("ignored.md", 1, 1, Some("h")),
                cached("gone.md", 1, 1, Some("g")),
            ],
            vec![],
            |p| p == "gone.md",
        );
        assert_eq!(paths(&d.vanished), vec!["gone.md"]);
        assert!(d.edited.is_empty() && d.new.is_empty());
    }

    #[test]
    fn a_provisional_row_is_always_edited() {
        let mut row = cached("a.md", 5, 10, None);
        row.provisional = true;
        let d = diff(vec![row], vec![disk("a.md", 5, 10)], gone_all);
        assert_eq!(d.edited, vec!["a.md"]);
        assert_eq!(d.unchanged, 0);
    }

    #[test]
    fn a_row_without_a_size_is_edited() {
        let mut row = cached("a.md", 5, 10, Some("h"));
        row.size = None;
        let d = diff(vec![row], vec![disk("a.md", 5, 10)], gone_all);
        assert_eq!(d.edited, vec!["a.md"]);
    }

    #[test]
    fn nothing_changed_is_an_empty_diff_with_no_paths() {
        let d = diff(
            vec![
                cached("a.md", 5, 10, Some("h")),
                cached("b/c.md", 1, 2, None),
            ],
            vec![disk("a.md", 5, 10), disk("b/c.md", 1, 2)],
            gone_all,
        );
        assert!(d.is_empty());
        assert_eq!(d.unchanged, 2);
        assert!(d.changed_paths(Path::new("/r")).is_empty());
    }

    #[test]
    fn changed_paths_are_edited_new_and_vanished_under_the_root() {
        let d = diff(
            vec![cached("e.md", 1, 1, None), cached("v/x.md", 1, 1, None)],
            vec![disk("e.md", 2, 1), disk("n.md", 1, 1)],
            gone_all,
        );
        let got = d.changed_paths(Path::new("/r"));
        let want: HashSet<PathBuf> = ["/r/e.md", "/r/n.md", "/r/v/x.md"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, want);
    }

    // ---- size-matched hash candidates ------------------------------

    #[test]
    fn a_new_file_is_hashed_only_when_its_size_equals_a_vanished_files() {
        let d = diff(
            vec![
                cached("old.md", 10, 1, Some("h1")),
                cached("nohash.md", 99, 1, None),
            ],
            vec![
                disk("same-size.md", 10, 5),
                disk("other-size.md", 11, 5),
                disk("size-of-unhashed.md", 99, 5),
            ],
            gone_all,
        );
        let sizes = size_matches([&d]);
        assert_eq!(
            sizes,
            HashSet::from([10]),
            "a vanished file with no hash adds no size"
        );

        let seen = std::sync::Mutex::new(Vec::new());
        let hashes = hash_new_files(&d, Path::new("/r"), &sizes, &|p: &Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            Ok(Some("h1".to_string()))
        });
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![PathBuf::from("/r/same-size.md")],
            "no other file was read"
        );
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes["same-size.md"], "h1");
    }

    #[test]
    fn sizes_come_from_every_source() {
        let a = diff(vec![cached("a.md", 7, 1, Some("h"))], vec![], gone_all);
        let b = diff(vec![cached("b.md", 8, 1, Some("g"))], vec![], gone_all);
        assert_eq!(size_matches([&a, &b]), HashSet::from([7, 8]));
    }

    #[test]
    fn a_file_over_the_cap_is_never_hashed() {
        let big = MAX_INDEXED_BYTES + 1;
        let d = diff(
            vec![],
            vec![DiskFile {
                path: "big.bin".into(),
                size: big,
                mtime: Some(1),
            }],
            gone_all,
        );
        let sizes = HashSet::from([big]);
        let hashes = hash_new_files(&d, Path::new("/r"), &sizes, &|_| {
            panic!("a file over the cap must not be read")
        });
        assert!(hashes.is_empty());
    }

    #[test]
    fn a_new_file_that_cannot_be_read_has_no_hash() {
        let d = diff(vec![], vec![disk("a.md", 4, 1)], gone_all);
        let hashes = hash_new_files(&d, Path::new("/r"), &HashSet::from([4]), &|_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(hashes.is_empty());
    }

    #[test]
    fn hash_file_matches_the_index_hash_and_skips_nothing_under_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.md");
        fs::write(&p, "hello\n").unwrap();
        assert_eq!(
            hash_file(&p).unwrap().as_deref(),
            Some("5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03")
        );
        assert!(hash_file(&dir.path().join("missing")).is_err());
    }

    // ---- facts and FR21 --------------------------------------------

    fn v(path: &str, hash: Option<&str>) -> Vanished {
        Vanished {
            source: String::new(),
            path: path.into(),
            identity: None,
            hash: hash.map(str::to_string),
            os: None,
        }
    }

    fn a(path: &str, hash: Option<&str>) -> Appeared {
        Appeared {
            source: String::new(),
            path: path.into(),
            identity: None,
            hash: hash.map(str::to_string),
            os: None,
        }
    }

    #[test]
    fn facts_carry_the_cached_hash_and_only_the_hashes_that_were_computed() {
        let d = diff(
            vec![cached("old.md", 3, 1, Some("h"))],
            vec![disk("new.md", 3, 1), disk("other.md", 9, 1)],
            gone_all,
        );
        let hashes = HashMap::from([("new.md".to_string(), "h".to_string())]);
        assert_eq!(vanished_facts(&d), vec![v("old.md", Some("h"))]);
        assert_eq!(
            appeared_facts(&d, &hashes),
            vec![a("new.md", Some("h")), a("other.md", None)]
        );
    }

    // ---- the disk --------------------------------------------------

    #[test]
    fn the_listing_matches_what_a_scan_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, text) in [
            ("a.md", "a"),
            ("sub/b.md", "bb"),
            (".git/HEAD", "ref"),
            ("node_modules/p/i.md", "x"),
            (".a.md.tephra-tmp-1", "stale"),
        ] {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.md"), root.join("link.md")).unwrap();

        let ignore = IgnoreRules::load_with_home(root, None).unwrap();
        let mut listed = list_disk(root, &ignore).unwrap();
        listed.sort_by(|x, y| x.path.cmp(&y.path));
        let names: Vec<&str> = listed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, vec!["a.md", "sub/b.md"]);
        assert_eq!(listed[1].size, 2);
        assert!(listed[1].mtime.is_some());
    }

    #[test]
    fn a_root_that_is_missing_is_an_error_not_an_empty_listing() {
        let dir = tempfile::tempdir().unwrap();
        let ignore = IgnoreRules::default();
        assert!(list_disk(&dir.path().join("nope"), &ignore).is_err());
    }

    #[test]
    fn is_gone_is_true_only_for_a_file_that_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("here.md"), "x").unwrap();
        assert!(!is_gone(dir.path(), "here.md"));
        assert!(is_gone(dir.path(), "away.md"));
        assert!(is_gone(dir.path(), "no/such/dir.md"));
    }
}
