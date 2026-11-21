//! G1 / G2 / E2 / E4 / E5 / E10 / E16 through the real path: a real
//! `SourceRegistry` with a real `notify` watcher over real temp
//! directories, a real cache, and the real `FixEngine` and write loop
//! fixing what the watcher sees. Nothing is called by hand: the test
//! renames files on disk and waits for the links to change.
//!
//! No fixed sleeps. Every wait polls a condition, up to a bound
//! (`wait_until`). The one mock is `AtomicFs`, in the E2 test, to make
//! one file permanently unwritable.

mod fixtures;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fixtures::{TempSource, GP1_FILES};
use tephra_cache::{Cache, STATE_NEEDS_MANUAL_FIX};
use tephrad::fixes::{FixEngine, FixEntryWire};
use tephrad::refactor::fix_write::{FixWriter, ThreadSleeper};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::SourceRegistry;

/// How long a wait for the daemon to react may take. A hash-only pair
/// waits `RENAME_HOLD` (2 s) first, so this is well above it. A wait
/// returns the moment its condition holds, so the bound only matters when
/// the OS is slow to hand the watcher its events (a loaded machine ran
/// this suite 25 s late), and it is set for that.
const WAIT: Duration = Duration::from_secs(90);

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct World {
    notes: TempSource,
    cache_path: PathBuf,
    registry: SourceRegistry,
    engine: Arc<FixEngine>,
    _tephra: TempSource,
    _cache_dir: tempfile::TempDir,
}

impl World {
    /// `@notes` holds `files` (and the vault fixture's own); `@tephra`
    /// is registered beside it. The engine writes through `fs`.
    fn new(files: &[(&str, &str)], fs: Arc<dyn AtomicFs>) -> World {
        let notes = fixtures::vault_fixture();
        for (rel, text) in files {
            notes.write(rel, text);
        }
        World::over(notes, fs)
    }

    fn over(notes: TempSource, fs: Arc<dyn AtomicFs>) -> World {
        let tephra = fixtures::repo_fixture();
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let cache_path = cache_dir.path().join("cache.db");
        let registry =
            SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
        let engine = Arc::new(FixEngine::new(
            &cache_path,
            Arc::new(FixWriter::with_parts(
                Arc::new(Mutex::new(())),
                fs,
                Arc::new(ThreadSleeper),
            )),
        ));
        registry.set_rename_fixer(engine.clone());
        registry.register("notes", notes.root()).expect("notes");
        registry.register("tephra", tephra.root()).expect("tephra");
        World {
            notes,
            cache_path,
            registry,
            engine,
            _tephra: tephra,
            _cache_dir: cache_dir,
        }
    }

    fn root(&self) -> &Path {
        self.notes.root()
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.root().join(rel)).expect("read file")
    }

    fn rename(&self, old: &str, new: &str) {
        let to = self.root().join(new);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.root().join(old), to).expect("rename on disk");
    }

    fn entries(&self) -> Vec<FixEntryWire> {
        self.engine.list(None, 100).expect("list the fix log")
    }

    fn renames(&self) -> Vec<FixEntryWire> {
        self.entries()
            .into_iter()
            .filter(|e| e.kind == "rename")
            .collect()
    }

    fn uncertain(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .entries()
            .into_iter()
            .filter(|e| e.kind == "uncertain")
            .filter_map(|e| e.old)
            .collect();
        v.sort();
        v
    }

    fn indexed(&self, rel: &str) -> bool {
        Cache::open(&self.cache_path)
            .unwrap()
            .get_source("notes")
            .unwrap()
            .and_then(|s| {
                Cache::open(&self.cache_path)
                    .unwrap()
                    .get_unit_by_rel_path(s.id, rel)
                    .unwrap()
            })
            .is_some()
    }

    fn finish(self) {
        self.registry.unregister("notes").unwrap();
        self.registry.unregister("tephra").unwrap();
    }
}

fn real() -> Arc<dyn AtomicFs> {
    Arc::new(RealFs)
}

// ---------------------------------------------------------------
// GP1 - GP3
// ---------------------------------------------------------------

#[test]
fn gp3_a_plain_rename_rewrites_the_three_links_and_logs_one_entry() {
    let w = World::new(&GP1_FILES, real());
    // GP1: the links are there before the rename.
    assert!(w.text("Weekly Review.md").contains("[[Training Plan]]"));

    // GP2: a plain filesystem rename.
    w.rename("Training Plan.md", "Plan.md");

    // GP3: all three files are rewritten, by the daemon alone.
    assert!(
        wait_until(WAIT, || {
            w.text("Weekly Review.md").contains("[[Plan]]")
                && w.text("Goals.md").contains("[[Plan|plan]]")
                && w.text("Ideas/Rename Notes.md").contains("![[Plan]]")
        }),
        "the links were not rewritten: {:?} {:?} {:?}",
        w.text("Weekly Review.md"),
        w.text("Goals.md"),
        w.text("Ideas/Rename Notes.md"),
    );
    assert!(
        wait_until(WAIT, || w.renames().len() == 1),
        "{:?}",
        w.entries()
    );
    let entries = w.entries();
    assert_eq!(entries.len(), 1, "one entry and nothing else: {entries:?}");
    let e = &entries[0];
    assert_eq!(e.kind, "rename");
    assert_eq!(e.old.as_deref(), Some("Training Plan.md"));
    assert_eq!(e.new.as_deref(), Some("Plan.md"));
    assert_eq!(e.files_changed, 3);
    assert_eq!(e.status, "applied");
    // Only link text changed: the rest of every file is as it was.
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[Plan]].\n"
    );
    assert_eq!(w.text("Unrelated.md"), GP1_FILES[4].1);
    w.finish();
}

// ---------------------------------------------------------------
// Added: multi-byte name next to the extension
// ---------------------------------------------------------------

#[test]
fn a_name_with_multibyte_characters_beside_the_extension_is_fixed_without_a_panic() {
    let w = World::new(
        &[
            ("Notes—v2.md", "# Notes\n"),
            ("Index.md", "# Index\n\nSee [[Notes—v2]].\n"),
        ],
        real(),
    );
    w.rename("Notes—v2.md", "Notes—v3.md");
    assert!(
        wait_until(WAIT, || w.text("Index.md").contains("[[Notes—v3]]")),
        "{:?}",
        w.text("Index.md")
    );
    assert!(wait_until(WAIT, || w.renames().len() == 1));
    w.finish();
}

// ---------------------------------------------------------------
// G1 folder rename, E2 partial failure
// ---------------------------------------------------------------

const FOLDER_FILES: [(&str, &str); 5] = [
    ("Training/Plan A.md", "# A\n"),
    ("Training/Plan B.md", "# B\n"),
    ("Training/Plan C.md", "# C\n"),
    (
        "Index.md",
        "# Index\n\n[[Training/Plan A]] and [[Training/Plan B]]\n",
    ),
    ("Other.md", "# Other\n\n[[Training/Plan C]]\n"),
];

#[test]
fn a_renamed_folder_is_one_entry_and_files_changed_counts_files_rewritten() {
    let w = World::new(&FOLDER_FILES, real());
    w.rename("Training", "Archive");

    assert!(
        wait_until(WAIT, || {
            !w.text("Index.md").contains("Training/") && !w.text("Other.md").contains("Training/")
        }),
        "{:?} {:?}",
        w.text("Index.md"),
        w.text("Other.md")
    );
    assert!(
        wait_until(WAIT, || w.renames().len() == 1),
        "{:?}",
        w.entries()
    );
    let entries = w.entries();
    assert_eq!(entries.len(), 1, "one Fix for the folder: {entries:?}");
    let e = &entries[0];
    assert_eq!(e.old.as_deref(), Some("Training"));
    assert_eq!(e.new.as_deref(), Some("Archive"));
    // Three files moved, two linking files rewritten.
    assert_eq!(e.files_changed, 2, "{e:?}");
    assert_eq!(e.status, "applied");

    // Every link resolves to the moved file again.
    w.registry.rescan("notes").unwrap();
    let cache = Cache::open(&w.cache_path).unwrap();
    let graph = tephra_graph::UnitGraph::build_from_cache(&cache).unwrap();
    for (file, target) in [
        ("@notes:Index.md", "@notes:Archive/Plan A.md"),
        ("@notes:Index.md", "@notes:Archive/Plan B.md"),
        ("@notes:Other.md", "@notes:Archive/Plan C.md"),
    ] {
        assert!(
            graph
                .get_forward_links(file)
                .iter()
                .any(|e| e.to == tephra_graph::EdgeTarget::Resolved(target.to_string())),
            "{file} no longer links to {target}"
        );
    }
    w.finish();
}

#[test]
fn a_folder_moved_into_another_parent_is_one_entry_with_the_folders_own_paths() {
    // `Notes` keeps a file of its own, so it is still there after the move.
    let w = World::new(
        &[
            ("Notes/2024/Jan.md", "# Jan\n"),
            ("Notes/2024/Feb.md", "# Feb\n"),
            ("Notes/Keep.md", "# Keep\n"),
            ("Archive/Old.md", "# Old\n"),
            (
                "Index.md",
                "# Index\n\n[[Notes/2024/Jan]] and [[Notes/2024/Feb]]\n",
            ),
        ],
        real(),
    );
    w.rename("Notes/2024", "Archive/2024");

    assert!(
        wait_until(WAIT, || !w.text("Index.md").contains("Notes/2024")),
        "{:?}",
        w.text("Index.md")
    );
    assert!(
        wait_until(WAIT, || !w.renames().is_empty()),
        "{:?}",
        w.entries()
    );
    let entries = w.entries();
    assert_eq!(entries.len(), 1, "one Fix for the folder: {entries:?}");
    let e = &entries[0];
    assert_eq!(e.kind, "rename");
    assert_eq!(e.old.as_deref(), Some("Notes/2024"));
    assert_eq!(e.new.as_deref(), Some("Archive/2024"));
    assert_eq!(e.files_changed, 1, "only Index.md was rewritten: {e:?}");
    assert_eq!(
        w.text("Index.md"),
        "# Index\n\n[[Archive/2024/Jan]] and [[Archive/2024/Feb]]\n"
    );
    w.finish();
}

#[test]
fn a_folder_of_identical_files_renamed_by_delete_and_create_is_paired_by_relative_path() {
    // Two copies of one template: the hash alone cannot say which new
    // file is which, the path under the folder can.
    let w = World::new(
        &[
            ("Old/Alpha.md", "# Template\n"),
            ("Old/Beta.md", "# Template\n"),
            ("Index.md", "# Index\n\n[[Old/Alpha]] and [[Old/Beta]]\n"),
        ],
        real(),
    );
    // What `git checkout` does: new files with new inodes, no rename flag.
    fs::remove_file(w.root().join("Old/Alpha.md")).unwrap();
    fs::remove_file(w.root().join("Old/Beta.md")).unwrap();
    fs::remove_dir(w.root().join("Old")).unwrap();
    fs::create_dir(w.root().join("New")).unwrap();
    fs::write(w.root().join("New/Alpha.md"), "# Template\n").unwrap();
    fs::write(w.root().join("New/Beta.md"), "# Template\n").unwrap();

    assert!(
        wait_until(WAIT, || w.text("Index.md").contains("[[New/Alpha]]")
            && w.text("Index.md").contains("[[New/Beta]]")),
        "{:?} {:?}",
        w.text("Index.md"),
        w.entries()
    );
    assert!(
        wait_until(WAIT, || !w.renames().is_empty()),
        "{:?}",
        w.entries()
    );
    let entries = w.entries();
    assert_eq!(entries.len(), 1, "one Fix for the folder: {entries:?}");
    assert_eq!(entries[0].old.as_deref(), Some("Old"));
    assert_eq!(entries[0].new.as_deref(), Some("New"));
    assert_eq!(
        entries[0].files_changed, 1,
        "only Index.md: {:?}",
        entries[0]
    );
    assert!(w.uncertain().is_empty(), "{:?}", w.entries());
    w.finish();
}

/// Makes every write for one linking file fail with a permanent error.
struct Unwritable(&'static str);

impl Unwritable {
    fn hits(&self, path: &Path) -> bool {
        path.file_name()
            .is_some_and(|n| n.to_string_lossy().contains(self.0))
    }
}

impl AtomicFs for Unwritable {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        if self.hits(path) {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        RealFs.sync(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

#[test]
fn e2_one_unwritable_file_leaves_the_others_rewritten_and_stays_as_pending_work() {
    let w = World::new(&FOLDER_FILES, Arc::new(Unwritable("Other.md")));
    let other_before = w.text("Other.md");
    w.rename("Training", "Archive");

    assert!(
        wait_until(WAIT, || !w.text("Index.md").contains("Training/")),
        "Index.md was not rewritten: {:?}",
        w.text("Index.md")
    );
    // Index.md's row is finished only after its rename lands, so under
    // load it can still be pending when the file already reads as
    // rewritten. Wait for the settled state: the only pending row is the
    // one for Other.md.
    let settled = |w: &World| {
        let rows = Cache::open(&w.cache_path)
            .unwrap()
            .pending_fixes(None, None)
            .unwrap();
        rows.len() == 1 && rows[0].file_path == "Other.md"
    };
    assert!(
        wait_until(WAIT, || settled(&w)),
        "the pending list never settled to the Other.md row: {:?}",
        Cache::open(&w.cache_path)
            .unwrap()
            .pending_fixes(None, None)
            .unwrap()
    );

    // The unwritable file is untouched: no half-written bytes, no temp
    // file left behind.
    assert_eq!(w.text("Other.md"), other_before);
    assert!(fixtures::tmp_files(w.root()).is_empty());

    let pending = Cache::open(&w.cache_path)
        .unwrap()
        .pending_fixes(None, None)
        .unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].state, STATE_NEEDS_MANUAL_FIX);
    assert_eq!(pending[0].old_path.as_deref(), Some("Training/Plan C.md"));
    assert_eq!(pending[0].new_path.as_deref(), Some("Archive/Plan C.md"));

    let entries = w.renames();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].old.as_deref(), Some("Training"));
    assert_eq!(entries[0].files_changed, 1, "only Index.md was written");
    assert_ne!(entries[0].status, "applied");
    w.finish();
}

// ---------------------------------------------------------------
// E5, E16, E4
// ---------------------------------------------------------------

#[test]
fn e5_a_copy_is_not_a_move() {
    let w = World::new(&GP1_FILES, real());
    fs::copy(
        w.root().join("Training Plan.md"),
        w.root().join("Training Plan copy.md"),
    )
    .unwrap();
    assert!(
        wait_until(WAIT, || w.indexed("Training Plan copy.md")),
        "the copy was never indexed"
    );
    // The original is still a unit and the links are as they were.
    assert!(w.indexed("Training Plan.md"));
    assert!(w.text("Weekly Review.md").contains("[[Training Plan]]"));
    assert!(w.entries().is_empty(), "{:?}", w.entries());
    w.finish();
}

#[test]
fn e16_an_ignored_path_is_not_tracked_and_a_move_into_one_is_a_plain_delete() {
    let notes = fixtures::vault_fixture();
    for (rel, text) in GP1_FILES {
        notes.write(rel, text);
    }
    notes.write(".tephraignore", "private/\n");
    notes.write("private/Secret.md", "# Secret\n");
    notes.write("Ideas/Loose.md", "# Loose\n");
    notes.write("Journal.md", "# Journal\n\n[[Loose]]\n");
    let w = World::over(notes, real());

    // Not tracked: renaming inside an ignored folder changes nothing.
    w.rename("private/Secret.md", "private/Secret 2.md");
    // A tracked file moved into it vanishes from the index.
    w.rename("Ideas/Loose.md", "private/Loose.md");
    // A sentinel rename, made after: when it is fixed the two above
    // have been through the watcher as well.
    w.rename("Training Plan.md", "Plan.md");
    assert!(
        wait_until(WAIT, || w.text("Weekly Review.md").contains("[[Plan]]")),
        "{:?}",
        w.text("Weekly Review.md")
    );

    // The only rename entry is the sentinel's.
    let renames = w.renames();
    assert_eq!(renames.len(), 1, "{:?}", w.entries());
    assert_eq!(renames[0].old.as_deref(), Some("Training Plan.md"));
    // The moved-in file is gone from the index and its link is not
    // rewritten to a path inside the ignored folder.
    assert!(wait_until(WAIT, || !w.indexed("Ideas/Loose.md")));
    assert!(!w.indexed("private/Loose.md"));
    assert_eq!(w.text("Journal.md"), "# Journal\n\n[[Loose]]\n");
    // Nothing was tracked for the ignored folder's own rename.
    assert!(!w.uncertain().iter().any(|p| p.starts_with("private/")));
    // The vanished tracked file is given up as a delete once its hold
    // ends, and logged.
    assert!(
        wait_until(WAIT, || w.uncertain() == vec!["Ideas/Loose.md".to_string()]),
        "{:?}",
        w.entries()
    );
    w.finish();
}

#[test]
fn e4_two_empty_files_that_vanish_while_two_appear_are_ambiguous_and_not_fixed() {
    let w = World::new(
        &[
            ("E1.md", ""),
            ("E2.md", ""),
            ("Index.md", "# Index\n\n[[E1]] [[E2]]\n"),
        ],
        real(),
    );
    // Deleted and created, not renamed: the OS names no pair, and the
    // two new files have the same (empty) hash as the two old ones.
    fs::remove_file(w.root().join("E1.md")).unwrap();
    fs::remove_file(w.root().join("E2.md")).unwrap();
    fs::write(w.root().join("F1.md"), "").unwrap();
    fs::write(w.root().join("F2.md"), "").unwrap();

    assert!(
        wait_until(WAIT, || w.uncertain()
            == vec!["E1.md".to_string(), "E2.md".to_string()]),
        "{:?}",
        w.entries()
    );
    assert!(w.renames().is_empty(), "{:?}", w.entries());
    assert_eq!(w.text("Index.md"), "# Index\n\n[[E1]] [[E2]]\n");
    assert!(w.indexed("F1.md") && w.indexed("F2.md"));
    w.finish();
}

#[test]
fn e3_a_new_file_with_a_new_inode_and_different_content_is_never_taken_for_a_rename() {
    let w = World::new(&GP1_FILES, real());
    // What `git pull` does: the old file goes, a new one with other
    // content (and a new inode) appears under another name.
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();
    fs::write(w.root().join("Plan.md"), "# Plan\n\nRewritten upstream.\n").unwrap();
    assert!(
        wait_until(WAIT, || w.uncertain()
            == vec!["Training Plan.md".to_string()]),
        "{:?}",
        w.entries()
    );
    assert!(w.renames().is_empty());
    assert!(w.text("Weekly Review.md").contains("[[Training Plan]]"));
    w.finish();
}
