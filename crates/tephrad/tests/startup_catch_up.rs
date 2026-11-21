//! G5 through the real path: a rename (or any change) made while
//! `tephrad` was not running is found at the next start by comparing
//! the disk with the cache, and a certain one is fixed like a live
//! rename.
//!
//! "The daemon was down" is real: a first `SourceRegistry` registers
//! and scans the sources and is dropped (its watchers stop); the disk
//! is changed; a second `SourceRegistry` over the same cache file is
//! started with `start_all`, as the daemon boot does. The real
//! `FixEngine` and write loop do the fixing. No fixed sleeps: every
//! wait polls a condition, up to a bound. The only mocks are a counting
//! (or racing) hasher, a short hold, and, for E8, a filesystem that
//! lets a file change between the writer's two looks at it.

mod fixtures;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use fixtures::{TempSource, GP1_FILES};
use tephra_cache::{Cache, SourceStatus};
use tephrad::catch_up::{hash_file, BootHooks};
use tephrad::fixes::{FixEngine, FixEntryWire};
use tephrad::ignore::IgnoreRules;
use tephrad::refactor::fix_write::{FixWriter, ThreadSleeper};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::{BootOutcome, DaemonEvent, SourceRegistry};
use tephrad::rpc::backend::{FixBackend, FixService, SourceBackend};

/// A wait returns the moment its condition holds; the bound is for a
/// loaded machine whose OS hands the watcher its events late.
const WAIT: Duration = Duration::from_secs(90);
/// What a test shortens the two-second hold to.
const HOLD: Duration = Duration::from_millis(100);
/// How long a *negative* check gives the daemon to do the thing it must
/// not do (write again, push, index) before looking. The spec asks for
/// polling with `wait_until` and never a fixed sleep, and every check
/// that waits for something to happen does that. A check that nothing
/// happens has no state to poll for, so it waits this long and then
/// looks once. That is a stated deviation from the spec, and it can only
/// fail to see a late wrong action (a false pass); it never makes a
/// correct run fail, so it cannot flake on a loaded host. The wait is
/// several times the `HOLD` and watcher debounce, the longest any such
/// action takes to start.
const NEGATIVE_WAIT: Duration = Duration::from_millis(400);
/// The same for the paused-watchers check, which has to outlast one full
/// watcher debounce window plus the poll the watcher runs on.
const NEGATIVE_WAIT_PAUSED: Duration = Duration::from_millis(600);

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
    tephra: TempSource,
    cache_path: PathBuf,
    _cache_dir: tempfile::TempDir,
}

/// A booted second registry and its engine.
struct Boot {
    registry: Arc<SourceRegistry>,
    engine: Arc<FixEngine>,
    outcomes: Vec<(String, BootOutcome)>,
}

impl World {
    /// `@notes` holds `files` and the vault fixture's own, `@tephra` the
    /// repo fixture; both registered and scanned, then "the daemon
    /// stops".
    fn new(files: &[(&str, &str)]) -> World {
        World::with_tephra(files, &[])
    }

    /// [`World::new`] with `tephra_files` added to the repo fixture.
    fn with_tephra(files: &[(&str, &str)], tephra_files: &[(&str, &str)]) -> World {
        let notes = fixtures::vault_fixture();
        for (rel, text) in files {
            notes.write(rel, text);
        }
        let tephra = fixtures::repo_fixture();
        for (rel, text) in tephra_files {
            tephra.write(rel, text);
        }
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let cache_path = cache_dir.path().join("cache.db");
        {
            let registry = SourceRegistry::new(Cache::open(&cache_path).expect("open cache"))
                .expect("registry");
            registry.register("notes", notes.root()).expect("notes");
            registry.register("tephra", tephra.root()).expect("tephra");
        }
        World {
            notes,
            tephra,
            cache_path,
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

    fn registry(&self) -> (Arc<SourceRegistry>, Arc<FixEngine>) {
        self.registry_over(Arc::new(RealFs), None)
    }

    /// A new registry over the cache file: the fix engine writes through
    /// `fs`, and stops after `stop_after` writes if that is set (a kill).
    fn registry_over(
        &self,
        fs: Arc<dyn AtomicFs>,
        stop_after: Option<usize>,
    ) -> (Arc<SourceRegistry>, Arc<FixEngine>) {
        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&self.cache_path).expect("open cache")).expect("reg"),
        );
        let mut engine = FixEngine::new(
            &self.cache_path,
            Arc::new(FixWriter::with_parts(
                Arc::new(Mutex::new(())),
                fs,
                Arc::new(ThreadSleeper),
            )),
        );
        if let Some(n) = stop_after {
            engine = engine.with_stop_after_writes(n);
        }
        let engine = Arc::new(engine);
        registry.set_rename_fixer(engine.clone());
        // As the daemon wires it: what a fix writes is indexed again by
        // the registry, and the watcher does not see it as a change.
        engine.set_sink(registry.clone());
        registry.set_rename_hold(HOLD);
        (registry, engine)
    }

    fn boot(&self) -> Boot {
        let (registry, engine) = self.registry();
        let outcomes = registry.start_all(|| false);
        Boot {
            registry,
            engine,
            outcomes,
        }
    }

    fn unit_hash(&self, source: &str, rel: &str) -> Option<String> {
        let cache = Cache::open(&self.cache_path).unwrap();
        let s = cache.get_source(source).unwrap().unwrap();
        cache
            .get_unit_by_rel_path(s.id, rel)
            .unwrap()
            .and_then(|u| u.hash)
    }

    /// Whether the cache's hash of `rel` is the hash of the file as it is
    /// on disk now: the file has been indexed since it was last written.
    fn cache_is_current(&self, source: &str, rel: &str) -> bool {
        let root = if source == "notes" {
            self.notes.root()
        } else {
            self.tephra.root()
        };
        match (self.unit_hash(source, rel), fs::read(root.join(rel))) {
            (Some(cached), Ok(bytes)) => cached == tephra_parser::content_hash_bytes(&bytes),
            _ => false,
        }
    }

    fn indexed(&self, source: &str, rel: &str) -> bool {
        let cache = Cache::open(&self.cache_path).unwrap();
        let s = cache.get_source(source).unwrap().unwrap();
        cache.get_unit_by_rel_path(s.id, rel).unwrap().is_some()
    }
}

impl Boot {
    fn entries(&self) -> Vec<FixEntryWire> {
        self.engine.list(None, 100).expect("list the fix log")
    }

    fn kinds(&self, kind: &str) -> Vec<FixEntryWire> {
        self.entries()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    fn uncertain(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .kinds("uncertain")
            .into_iter()
            .filter_map(|e| e.old)
            .collect();
        v.sort();
        v
    }

    fn outcome(&self, source: &str) -> &BootOutcome {
        &self
            .outcomes
            .iter()
            .find(|(n, _)| n == source)
            .unwrap_or_else(|| panic!("no outcome for {source}: {:?}", self.outcomes))
            .1
    }
}

// ---------------------------------------------------------------
// The rename found at start is fixed
// ---------------------------------------------------------------

#[test]
fn a_rename_made_while_down_is_fixed_at_start() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    assert!(w.text("Weekly Review.md").contains("[[Training Plan]]"));

    let b = w.boot();

    assert!(
        wait_until(WAIT, || {
            w.text("Weekly Review.md").contains("[[Plan]]")
                && w.text("Goals.md").contains("[[Plan|plan]]")
                && w.text("Ideas/Rename Notes.md").contains("![[Plan]]")
        }),
        "the links were not rewritten: {:?}",
        w.text("Weekly Review.md")
    );
    assert!(
        wait_until(WAIT, || b.kinds("rename").len() == 1),
        "{:?}",
        b.entries()
    );
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "one entry and nothing else: {entries:?}");
    assert_eq!(entries[0].old.as_deref(), Some("Training Plan.md"));
    assert_eq!(entries[0].new.as_deref(), Some("Plan.md"));
    assert_eq!(entries[0].files_changed, 3);
    assert!(matches!(b.outcome("notes"), BootOutcome::CaughtUp { .. }));
    // The cache follows the disk: the new path is indexed, the old is gone.
    assert!(w.indexed("notes", "Plan.md"));
    assert!(!w.indexed("notes", "Training Plan.md"));
}

#[test]
fn a_folder_renamed_while_down_is_fixed_as_one_rename() {
    let w = World::new(&[
        ("Projects/Alpha.md", "# Alpha\n\nalpha body\n"),
        ("Projects/Beta.md", "# Beta\n\nbeta body, longer\n"),
        ("Index.md", "[[Alpha]] and [[Projects/Beta]]\n"),
    ]);
    fs::rename(w.root().join("Projects"), w.root().join("Work")).unwrap();

    let b = w.boot();

    assert!(
        wait_until(WAIT, || w.text("Index.md")
            == "[[Alpha]] and [[Work/Beta]]\n"),
        "{:?}",
        w.text("Index.md")
    );
    assert!(
        wait_until(WAIT, || !b.kinds("rename").is_empty()),
        "{:?}",
        b.entries()
    );
    assert!(b.uncertain().is_empty(), "{:?}", b.entries());
}

// ---------------------------------------------------------------
// Not certain: no fix, an `uncertain` entry
// ---------------------------------------------------------------

#[test]
fn a_file_renamed_and_edited_while_down_is_uncertain_and_nothing_is_rewritten() {
    let w = World::new(&GP1_FILES);
    let before = w.text("Weekly Review.md");
    w.rename("Training Plan.md", "Plan.md");
    fs::write(w.root().join("Plan.md"), "# Plan\n\ncompletely different\n").unwrap();

    let b = w.boot();

    assert!(
        wait_until(WAIT, || b.uncertain()
            == vec!["Training Plan.md".to_string()]),
        "{:?}",
        b.entries()
    );
    assert_eq!(w.text("Weekly Review.md"), before, "no link was touched");
    assert!(b.kinds("rename").is_empty());
    // The new file is still indexed under its new name.
    assert!(w.indexed("notes", "Plan.md"));
}

#[test]
fn two_vanished_files_with_the_same_content_make_no_pair() {
    let w = World::new(&[
        ("a.md", "same text\n"),
        ("b.md", "same text\n"),
        ("Index.md", "[[a]] [[b]]\n"),
    ]);
    w.rename("a.md", "c.md");
    w.rename("b.md", "d.md");

    let b = w.boot();

    assert!(
        wait_until(WAIT, || b.uncertain()
            == vec!["a.md".to_string(), "b.md".to_string()]),
        "{:?}",
        b.entries()
    );
    assert_eq!(w.text("Index.md"), "[[a]] [[b]]\n");
    assert!(b.kinds("rename").is_empty());
}

#[test]
fn a_file_deleted_while_down_leaves_the_cache_and_is_uncertain() {
    let w = World::new(&GP1_FILES);
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();

    let b = w.boot();

    assert!(
        wait_until(WAIT, || b.uncertain()
            == vec!["Training Plan.md".to_string()]),
        "{:?}",
        b.entries()
    );
    assert!(!w.indexed("notes", "Training Plan.md"));
}

// ---------------------------------------------------------------
// Across sources (FR21, G6): a move is fixed as one `move`
// ---------------------------------------------------------------

#[test]
fn a_file_moved_to_another_source_while_down_is_fixed_as_one_move() {
    let w = World::new(&GP1_FILES);
    let text = w.text("Training Plan.md");
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();
    w.tephra.write("Training Plan.md", &text);

    let b = w.boot();

    assert!(
        wait_until(WAIT, || b.kinds("move").len() == 1
            && w.text("Weekly Review.md").contains("@tephra:")),
        "{:?}",
        b.entries()
    );
    assert!(b.uncertain().is_empty(), "{:?}", b.entries());
    assert!(b.kinds("rename").is_empty());
    assert_eq!(
        w.text("Weekly Review.md"),
        "# Weekly Review\n\nThis week: [[@tephra:Training Plan]].\n"
    );
    assert!(!w.indexed("notes", "Training Plan.md"));
    assert!(w.indexed("tephra", "Training Plan.md"));
}

/// The hold a hash pair waits for a twin starts once EVERY source has been
/// brought up to date, not as each one finishes. Here the later source
/// (`tephra`, where the moved file is) has a long catch-up of its own: a
/// few thousand new files to index, far longer than the test's `HOLD`.
/// Were the first source's facts handed over as soon as it was done, its
/// hold would be over before the move's other half is known and the move
/// would be logged `uncertain` instead of fixed as one `move`. This is the
/// slow-host case that made the two cross-source tests above flaky under
/// load, made deterministic.
#[test]
fn a_slow_catch_up_in_a_later_source_does_not_end_an_earlier_sources_hold() {
    let w = World::new(&GP1_FILES);
    let text = w.text("Training Plan.md");
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();
    w.tephra.write("Training Plan.md", &text);
    for i in 0..3000 {
        w.tephra.write(
            &format!("bulk-{i}.md"),
            &format!("# Bulk {i}\n\nA file that is new in the later source, number {i}.\n"),
        );
    }

    // A short hold, so the later source's catch-up is certain to outlast
    // it on any host (3000 files take longer than this to index).
    let (registry, engine) = w.registry();
    registry.set_rename_hold(Duration::from_millis(5));
    let b = Boot {
        outcomes: registry.start_all(|| false),
        registry,
        engine,
    };

    assert!(
        wait_until(WAIT, || b.kinds("move").len() == 1
            && w.text("Weekly Review.md").contains("@tephra:")),
        "{:?}",
        b.entries()
    );
    assert!(b.uncertain().is_empty(), "{:?}", b.entries());
    assert!(b.kinds("rename").is_empty());
}

#[test]
fn a_hash_in_two_sources_is_not_used_to_pair_a_rename_inside_one() {
    let w = World::new(&GP1_FILES);
    // The same content also appears in the other source: two candidates
    // for the vanished file across all sources, so no pair (FR21).
    let text = w.text("Training Plan.md");
    w.tephra.write("copy.md", &text);
    w.rename("Training Plan.md", "Plan.md");
    let weekly = w.text("Weekly Review.md");

    let b = w.boot();

    assert!(
        wait_until(WAIT, || b.uncertain()
            == vec!["Training Plan.md".to_string()]),
        "{:?}",
        b.entries()
    );
    assert_eq!(w.text("Weekly Review.md"), weekly);
}

// ---------------------------------------------------------------
// Edits, nothing changed, reads
// ---------------------------------------------------------------

#[test]
fn an_edit_made_while_down_is_indexed_at_start() {
    let w = World::new(&GP1_FILES);
    let before = w.unit_hash("notes", "Unrelated.md");
    assert!(before.is_some());
    // A different size, so the change is certain even within one mtime tick.
    fs::write(
        w.root().join("Unrelated.md"),
        "# Unrelated\n\nedited while down\n",
    )
    .unwrap();
    w.notes.write("Fresh.md", "# Fresh\n");

    let b = w.boot();

    assert_eq!(b.outcome("notes"), &BootOutcome::CaughtUp { paths: 2 });
    assert_ne!(w.unit_hash("notes", "Unrelated.md"), before);
    assert!(w.indexed("notes", "Fresh.md"));
    assert!(b.kinds("rename").is_empty());
}

/// A path an edited `.tephraignore` matches while the daemon is down is
/// no longer listed, but it is still on disk: it is not "vanished" (no
/// rename pairing, no log entry), and its cached row goes at start.
#[test]
fn a_file_the_ignore_file_now_matches_loses_its_row_at_start_and_is_not_a_vanished_file() {
    let w = World::new(&GP1_FILES);
    assert!(w.indexed("notes", "Unrelated.md"));
    w.notes.write(".tephraignore", "Unrelated.md\n");

    let b = w.boot();

    assert!(!w.indexed("notes", "Unrelated.md"), "the stale row is gone");
    assert!(
        w.root().join("Unrelated.md").exists(),
        "the file is not touched"
    );
    assert!(b.kinds("uncertain").is_empty(), "{:?}", b.entries());
    assert!(b.kinds("rename").is_empty());
    assert!(matches!(b.outcome("notes"), BootOutcome::CaughtUp { paths } if *paths >= 1));
}

#[test]
fn a_start_with_nothing_changed_reads_no_file_and_announces_nothing() {
    let w = World::new(&GP1_FILES);
    let (registry, engine) = w.registry();
    let mut events = registry.subscribe();
    let reads = AtomicUsize::new(0);
    let counting = |p: &Path| -> io::Result<Option<String>> {
        reads.fetch_add(1, Ordering::SeqCst);
        hash_file(p)
    };
    let hooks = BootHooks {
        hash_file: &counting,
        ..BootHooks::default()
    };

    let outcomes = registry.start_all_with(|| false, &hooks);

    assert_eq!(reads.load(Ordering::SeqCst), 0, "no file was hashed");
    assert!(outcomes
        .iter()
        .all(|(_, o)| *o == BootOutcome::CaughtUp { paths: 0 }));
    assert!(events.try_recv().is_err(), "nothing changed, nothing sent");
    assert!(engine.list(None, 100).unwrap().is_empty());
}

#[test]
fn only_a_new_file_the_size_of_a_vanished_one_is_hashed() {
    let w = World::new(&[("Old.md", "0123456789\n"), ("Other.md", "x\n")]);
    w.rename("Old.md", "New.md");
    w.notes
        .write("Unrelated size.md", "this is not the same size at all\n");
    let (registry, _engine) = w.registry();
    let hashed: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let recording = |p: &Path| -> io::Result<Option<String>> {
        hashed
            .lock()
            .unwrap()
            .push(p.file_name().unwrap().to_string_lossy().into_owned());
        hash_file(p)
    };
    let hooks = BootHooks {
        hash_file: &recording,
        ..BootHooks::default()
    };

    registry.start_all_with(|| false, &hooks);

    assert_eq!(*hashed.lock().unwrap(), vec!["New.md".to_string()]);
}

// ---------------------------------------------------------------
// Sources that cannot be caught up
// ---------------------------------------------------------------

#[test]
fn a_source_whose_root_is_missing_is_marked_offline_and_keeps_its_rows() {
    let w = World::new(&GP1_FILES);
    let moved = w.tephra.root().with_extension("away");
    fs::rename(w.tephra.root(), &moved).unwrap();
    let units_before = w.indexed("tephra", "README.md");

    let b = w.boot();

    assert_eq!(b.outcome("tephra"), &BootOutcome::Offline);
    assert!(matches!(b.outcome("notes"), BootOutcome::CaughtUp { .. }));
    let cache = Cache::open(&w.cache_path).unwrap();
    assert_eq!(
        cache.get_source("tephra").unwrap().unwrap().status,
        SourceStatus::Offline
    );
    assert_eq!(w.indexed("tephra", "README.md"), units_before, "rows kept");
    assert!(
        b.uncertain().is_empty(),
        "an offline source is not a vanish"
    );
    // Put it back so the temp dir cleans up.
    fs::rename(&moved, w.tephra.root()).unwrap();
}

#[test]
fn an_offline_source_whose_root_is_back_is_caught_up_and_active() {
    let w = World::new(&GP1_FILES);
    let moved = w.tephra.root().with_extension("away");
    fs::rename(w.tephra.root(), &moved).unwrap();
    drop(w.boot());
    fs::rename(&moved, w.tephra.root()).unwrap();
    w.tephra.write("new-while-away.md", "# new\n");

    let b = w.boot();

    assert!(matches!(b.outcome("tephra"), BootOutcome::CaughtUp { .. }));
    let cache = Cache::open(&w.cache_path).unwrap();
    assert_eq!(
        cache.get_source("tephra").unwrap().unwrap().status,
        SourceStatus::Active
    );
    assert!(w.indexed("tephra", "new-while-away.md"));
}

/// The offline -> stale -> active round trip and the catch-up are ONE
/// push, sent once the status is final, so a client that rebuilds from
/// it sees `active` and the new file together.
#[test]
fn an_offline_source_coming_back_is_announced_once_with_its_final_status() {
    let w = World::new(&GP1_FILES);
    let moved = w.tephra.root().with_extension("away");
    fs::rename(w.tephra.root(), &moved).unwrap();
    drop(w.boot());
    fs::rename(&moved, w.tephra.root()).unwrap();
    w.tephra.write("new-while-away.md", "# new\n");

    let (registry, _engine) = w.registry();
    let mut events = registry.subscribe();
    registry.start_all(|| false);

    let pushes: Vec<DaemonEvent> = drain(&mut events)
        .into_iter()
        .filter(|e| matches!(e, DaemonEvent::SourceChanged { source, .. } if source == "tephra"))
        .collect();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    match &pushes[0] {
        DaemonEvent::SourceChanged { paths, .. } => assert!(
            paths.iter().any(|p| p.ends_with("new-while-away.md")),
            "{paths:?}"
        ),
        other => panic!("{other:?}"),
    }
    let cache = Cache::open(&w.cache_path).unwrap();
    assert_eq!(
        cache.get_source("tephra").unwrap().unwrap().status,
        SourceStatus::Active
    );
}

#[test]
fn a_shutdown_before_start_all_writes_nothing_and_starts_no_watcher() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let (registry, engine) = w.registry();

    let outcomes = registry.start_all(|| true);

    assert!(outcomes.iter().all(|(_, o)| *o == BootOutcome::Stopped));
    assert!(
        w.indexed("notes", "Training Plan.md"),
        "the cache is untouched"
    );
    assert!(!w.indexed("notes", "Plan.md"));
    assert!(engine.list(None, 100).unwrap().is_empty());
}

#[test]
fn a_source_with_no_finished_scan_is_only_watched() {
    let w = World::new(&GP1_FILES);
    {
        // A scan that started and never finished.
        let cache = Cache::open(&w.cache_path).unwrap();
        let s = cache.get_source("notes").unwrap().unwrap();
        cache.mark_scan_started(s.id).unwrap();
    }
    w.rename("Training Plan.md", "Plan.md");

    let b = w.boot();

    assert_eq!(b.outcome("notes"), &BootOutcome::Watching);
    assert!(b.entries().is_empty(), "no baseline, no comparison");
}

// ---------------------------------------------------------------
// After the catch-up the source is watched
// ---------------------------------------------------------------

#[test]
fn after_start_a_live_rename_is_fixed_by_the_watcher() {
    let w = World::new(&GP1_FILES);
    let b = w.boot();
    assert!(matches!(b.outcome("notes"), BootOutcome::CaughtUp { .. }));
    // The production hold, not `HOLD`: the OS can hand the watcher the
    // files written while the world was set up as a batch of its own after
    // the start, so the rename below can arrive as two batches a debounce
    // window (250 ms) or more apart. A hold shorter than that gives up on
    // the vanished file ("uncertain") before its twin turns up, which is
    // what a 100 ms hold did here about 1 run in 10.
    b.registry.set_rename_hold(tephrad::watcher::RENAME_HOLD);

    w.rename("Training Plan.md", "Plan.md");

    assert!(
        wait_until(WAIT, || w.text("Weekly Review.md").contains("[[Plan]]")),
        "{:?}",
        w.text("Weekly Review.md")
    );
    assert!(
        wait_until(WAIT, || b.kinds("rename").len() == 1),
        "{:?}",
        b.entries()
    );
    drop(b.registry);
}

// ---------------------------------------------------------------
// More helpers
// ---------------------------------------------------------------

/// Every file under `root` with its bytes and modification time.
fn tree(root: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>, SystemTime)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                let mtime = fs::metadata(&path).unwrap().modified().unwrap();
                out.push((rel, fs::read(&path).unwrap(), mtime));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).expect("read file")
}

/// Every event the registry sent so far.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>) -> Vec<DaemonEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

const ARCH: &str = "# Architecture\n\nThe daemon watches sources and fixes links.\n";

/// GP8's two sources: `@tephra` holds `docs/ARCHITECTURE.md` and a guide
/// that links it, `@notes` links it across sources.
fn gp8() -> World {
    World::with_tephra(
        &[("Design.md", "Design notes: [[@tephra:docs/ARCHITECTURE]]\n")],
        &[
            ("docs/ARCHITECTURE.md", ARCH),
            ("docs/Guide.md", "Read [[ARCHITECTURE]] first.\n"),
        ],
    )
}

// ---------------------------------------------------------------
// GP8, E18, E7
// ---------------------------------------------------------------

#[test]
fn gp8_a_pull_that_renamed_a_doc_while_down_fixes_both_link_spellings_once() {
    let w = gp8();
    let design_before = read(w.notes.root(), "Design.md");
    // The two files that link the doc before the rename, from the graph.
    let backlinks_of = |id: &str| -> Vec<String> {
        let graph = tephra_graph::UnitGraph::build_from_cache(&Cache::open(&w.cache_path).unwrap())
            .expect("graph");
        let mut from: Vec<String> = graph
            .get_backlinks(id)
            .into_iter()
            .map(|e| e.from)
            .collect();
        from.sort();
        from
    };
    let backlinks_before = backlinks_of("@tephra:docs/ARCHITECTURE.md");
    assert_eq!(
        backlinks_before,
        vec!["@notes:Design.md", "@tephra:docs/Guide.md"]
    );
    // `git pull`: a new inode, no rename flag.
    fs::create_dir_all(w.tephra.root().join("docs/architecture")).unwrap();
    fs::rename(
        w.tephra.root().join("docs/ARCHITECTURE.md"),
        w.tephra.root().join("docs/architecture/overview.md"),
    )
    .unwrap();
    let tephra_before = tree(w.tephra.root());

    let b = w.boot();

    assert!(
        wait_until(WAIT, || {
            read(w.tephra.root(), "docs/Guide.md") == "Read [[overview]] first.\n"
                && read(w.notes.root(), "Design.md")
                    == "Design notes: [[@tephra:docs/architecture/overview]]\n"
        }),
        "{:?} / {:?}",
        read(w.tephra.root(), "docs/Guide.md"),
        read(w.notes.root(), "Design.md")
    );
    assert_ne!(read(w.notes.root(), "Design.md"), design_before);
    assert!(
        wait_until(WAIT, || b.kinds("rename").len() == 1),
        "{:?}",
        b.entries()
    );
    let entries = b.entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].status, "applied");
    assert_eq!(entries[0].files_changed, 2);
    assert_eq!(entries[0].old.as_deref(), Some("docs/ARCHITECTURE.md"));
    assert_eq!(
        entries[0].new.as_deref(),
        Some("docs/architecture/overview.md")
    );

    // F3: the target file and everything not linking to it is untouched.
    let after = tree(w.tephra.root());
    let of = |t: &[(String, Vec<u8>, SystemTime)], rel: &str| {
        t.iter().find(|f| f.0 == rel).map(|f| (f.1.clone(), f.2))
    };
    assert_eq!(
        of(&after, "docs/architecture/overview.md"),
        of(&tephra_before, "docs/architecture/overview.md"),
        "the renamed file is not written"
    );
    assert_eq!(of(&after, "README.md"), of(&tephra_before, "README.md"));
    // Both linking files now resolve to the new file: the graph, built
    // from the cache the way the daemon builds it, gives the new unit the
    // same two backlinks the old one had (checked before the rename).
    assert!(
        wait_until(WAIT, || {
            w.cache_is_current("tephra", "docs/Guide.md")
                && w.cache_is_current("notes", "Design.md")
        }),
        "the fix's writes are indexed"
    );
    assert_eq!(
        backlinks_of("@tephra:docs/architecture/overview.md"),
        backlinks_before,
        "the new file has the backlinks the old one had"
    );
    assert!(backlinks_of("@tephra:docs/ARCHITECTURE.md").is_empty());
    assert!(w.indexed("tephra", "docs/architecture/overview.md"));
    assert!(!w.indexed("tephra", "docs/ARCHITECTURE.md"));
}

#[test]
fn e18_a_second_start_with_nothing_changed_writes_nothing_and_announces_nothing() {
    let w = gp8();
    fs::create_dir_all(w.tephra.root().join("docs/architecture")).unwrap();
    fs::rename(
        w.tephra.root().join("docs/ARCHITECTURE.md"),
        w.tephra.root().join("docs/architecture/overview.md"),
    )
    .unwrap();
    {
        let b = w.boot();
        assert!(
            wait_until(WAIT, || b.kinds("rename").len() == 1),
            "{:?}",
            b.entries()
        );
        assert!(wait_until(WAIT, || read(w.notes.root(), "Design.md")
            .contains("overview")));
        // The fix's own writes settle before the next start: both linking
        // files carry their new text, the cache holds that text (a row
        // that merely exists is the one from before the fix), and no
        // temp file of the write is left.
        assert!(
            wait_until(WAIT, || {
                w.cache_is_current("tephra", "docs/Guide.md")
                    && w.cache_is_current("notes", "Design.md")
                    && read(w.tephra.root(), "docs/Guide.md").contains("overview")
                    && [w.notes.root(), w.tephra.root()]
                        .iter()
                        .all(|root| tree(root).iter().all(|f| !f.0.contains(".tephra-tmp")))
            }),
            "{:?}",
            tree(w.tephra.root())
                .iter()
                .map(|f| &f.0)
                .collect::<Vec<_>>()
        );
    }
    let notes_before = tree(w.notes.root());
    let tephra_before = tree(w.tephra.root());

    let (registry, engine) = w.registry();
    let mut events = registry.subscribe();
    let outcomes = registry.start_all(|| false);
    // Nothing is to be pushed late: a negative check (see `NEGATIVE_WAIT`).
    std::thread::sleep(NEGATIVE_WAIT);

    assert!(
        outcomes
            .iter()
            .all(|(_, o)| *o == BootOutcome::CaughtUp { paths: 0 }),
        "{outcomes:?}"
    );
    assert_eq!(engine.list(None, 100).unwrap().len(), 1, "no new log row");
    assert!(
        drain(&mut events).is_empty(),
        "no graph:changed, no source push"
    );
    assert_eq!(tree(w.notes.root()), notes_before, "bytes and mtimes");
    assert_eq!(tree(w.tephra.root()), tephra_before, "bytes and mtimes");
}

#[test]
fn e7_an_exact_event_after_the_catch_up_fixed_the_rename_is_already_applied() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let b = w.boot();
    assert!(
        wait_until(WAIT, || b.kinds("rename").len() == 1),
        "{:?}",
        b.entries()
    );
    let service = FixService::new(b.engine.clone(), b.registry.clone());
    let root = w.root().canonicalize().unwrap();

    let reply = service.rename_event(
        &root.join("Training Plan.md").display().to_string(),
        &root.join("Plan.md").display().to_string(),
    );

    assert_eq!(
        reply.unwrap(),
        tephrad::fixes::event::RenameEventWire::AlreadyApplied
    );
    assert_eq!(
        b.kinds("rename").len(),
        1,
        "no second entry: {:?}",
        b.entries()
    );
}

// ---------------------------------------------------------------
// E12: sources that cannot be started
// ---------------------------------------------------------------

fn running_as_root() -> bool {
    unsafe {
        extern "C" {
            fn geteuid() -> u32;
        }
        geteuid() == 0
    }
}

/// Restores a directory's mode on drop, so a failed assertion never
/// leaves an unremovable tempdir behind.
struct RestoreMode<'a>(&'a Path);

impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn e12_a_root_the_os_will_not_list_is_skipped_and_stays_permission_denied() {
    if running_as_root() {
        eprintln!("skipped: running as root, file modes are ignored");
        return;
    }
    let w = World::new(&GP1_FILES);
    let root = w.tephra.root().to_path_buf();
    let _restore = RestoreMode(&root);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    w.rename("Training Plan.md", "Plan.md");

    let b = w.boot();

    assert_eq!(b.outcome("tephra"), &BootOutcome::PermissionDenied);
    assert!(!b.registry.is_watching("tephra"), "no watcher");
    let listed = SourceBackend::list(&*b.registry).unwrap();
    let status = |name: &str| {
        listed
            .iter()
            .find(|r| r.name == name)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status("tephra"), "permission_denied");
    // Its rows are kept and nothing was written for it.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(w.indexed("tephra", "README.md"));
    assert!(b.uncertain().is_empty(), "not a vanish: {:?}", b.entries());
    // The other source carried on.
    assert!(matches!(b.outcome("notes"), BootOutcome::CaughtUp { .. }));
    assert!(b.registry.is_watching("notes"));
}

#[test]
fn e12_an_offline_source_has_no_watcher_and_no_file_is_written() {
    let w = World::new(&GP1_FILES);
    let moved = w.tephra.root().with_extension("away");
    fs::rename(w.tephra.root(), &moved).unwrap();
    let before = tree(&moved);

    let b = w.boot();

    assert_eq!(b.outcome("tephra"), &BootOutcome::Offline);
    assert!(!b.registry.is_watching("tephra"));
    assert!(
        b.registry.is_watching("notes"),
        "the other source carries on"
    );
    assert_eq!(tree(&moved), before, "no file written");
    fs::rename(&moved, w.tephra.root()).unwrap();
}

#[test]
fn e12_a_watcher_that_fails_to_start_is_logged_and_the_other_sources_carry_on() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let tephra_root = w.tephra.root().canonicalize().unwrap();
    let failing = |root: &Path, ignore: Arc<RwLock<IgnoreRules>>| {
        if root.canonicalize().unwrap() == tephra_root {
            Err(tephrad::watcher::WatchError::Notify(
                notify::Error::generic("no watch for you"),
            ))
        } else {
            tephrad::watcher::start(root.to_path_buf(), ignore)
        }
    };
    let hooks = BootHooks {
        start_tap: &failing,
        ..BootHooks::default()
    };
    let (registry, _engine) = w.registry();

    let outcomes = registry.start_all_with(|| false, &hooks);

    let outcome = |name: &str| outcomes.iter().find(|(n, _)| n == name).unwrap().1.clone();
    assert!(
        matches!(outcome("tephra"), BootOutcome::Failed(_)),
        "{outcomes:?}"
    );
    assert!(matches!(outcome("notes"), BootOutcome::CaughtUp { .. }));
    assert!(!registry.is_watching("tephra"));
    assert!(registry.is_watching("notes"));
    assert!(
        wait_until(WAIT, || w.text("Weekly Review.md").contains("[[Plan]]")),
        "the other source was caught up and fixed"
    );
}

// ---------------------------------------------------------------
// E4/E5/E16/FR4/F5: what is not a rename
// ---------------------------------------------------------------

#[test]
fn e5_a_copy_with_the_original_still_there_is_no_fix_and_no_entry() {
    let w = World::new(&GP1_FILES);
    let weekly = w.text("Weekly Review.md");
    fs::copy(w.root().join("Training Plan.md"), w.root().join("Copy.md")).unwrap();

    let b = w.boot();

    assert_eq!(b.outcome("notes"), &BootOutcome::CaughtUp { paths: 1 });
    assert!(w.indexed("notes", "Copy.md"));
    assert!(w.indexed("notes", "Training Plan.md"));
    assert_eq!(w.text("Weekly Review.md"), weekly);
    assert!(b.entries().is_empty(), "{:?}", b.entries());
}

#[test]
fn e16_a_rename_inside_an_ignored_directory_is_not_tracked_and_no_fix_is_made() {
    let w = World::new(&[
        (".tephraignore", "Private/\n"),
        ("Private/Secret.md", "# Secret\n\nhidden\n"),
        ("Index.md", "[[Secret]]\n"),
    ]);
    assert!(!w.indexed("notes", "Private/Secret.md"));
    w.rename("Private/Secret.md", "Private/Open.md");
    // Also inside a default-ignored directory.
    w.notes.write("node_modules/p/a.md", "# a\n");
    fs::rename(
        w.root().join("node_modules/p/a.md"),
        w.root().join("node_modules/p/b.md"),
    )
    .unwrap();

    let b = w.boot();

    assert_eq!(b.outcome("notes"), &BootOutcome::CaughtUp { paths: 0 });
    assert!(b.entries().is_empty(), "{:?}", b.entries());
    assert_eq!(w.text("Index.md"), "[[Secret]]\n");
    assert!(!w.indexed("notes", "Private/Open.md"));
}

#[test]
fn fr4_after_a_schema_rebuild_nothing_is_paired_and_a_vanished_file_is_logged_once() {
    // `tephra` is offline when the schema is rebuilt: its rows come back
    // as provisional rows with no hash, so no pairing is possible. `notes`
    // is online: its rows are emptied and its scan flagged unfinished.
    let w = World::new(&GP1_FILES);
    let moved = w.tephra.root().with_extension("away");
    fs::rename(w.tephra.root(), &moved).unwrap();
    drop(w.boot());
    {
        let cache = Cache::open(&w.cache_path).unwrap();
        cache
            .connection()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [tephra_cache::CURRENT_SCHEMA_VERSION - 1],
            )
            .unwrap();
    }
    assert!(Cache::open(&w.cache_path)
        .unwrap()
        .rebuilt_from_version()
        .is_some());
    assert!(
        !w.indexed("notes", "Weekly Review.md"),
        "the rows were emptied"
    );
    // While down: a file of `tephra` is renamed, then the root is back.
    fs::rename(&moved, w.tephra.root()).unwrap();
    fs::rename(
        w.tephra.root().join("README.md"),
        w.tephra.root().join("Intro.md"),
    )
    .unwrap();
    w.rename("Training Plan.md", "Plan.md");
    let weekly = w.text("Weekly Review.md");

    let (registry, engine) = w.registry();
    registry.rescan_unfinished(|| false);
    let outcomes = registry.start_all(|| false);

    assert!(
        matches!(
            outcomes.iter().find(|(n, _)| n == "notes").unwrap().1,
            BootOutcome::CaughtUp { paths: 0 }
        ),
        "{outcomes:?}"
    );
    assert_eq!(w.text("Weekly Review.md"), weekly, "nothing written");
    assert!(w.indexed("notes", "Plan.md"), "rescanned from the top");
    assert!(
        wait_until(WAIT, || engine
            .list(None, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "uncertain")
            .count()
            == 1),
        "{:?}",
        engine.list(None, 100)
    );
    let entries = engine.list(None, 100).unwrap();
    assert!(
        entries.iter().all(|e| e.kind == "uncertain"),
        "no pairing: {entries:?}"
    );
    assert_eq!(entries[0].old.as_deref(), Some("README.md"));
    drop(registry);

    // A second start adds nothing: logged once.
    let (registry, engine) = w.registry();
    registry.start_all(|| false);
    std::thread::sleep(NEGATIVE_WAIT);
    assert_eq!(engine.list(None, 100).unwrap().len(), 1);
}

#[test]
fn an_unfinished_source_is_rescanned_first_and_never_paired() {
    let w = World::new(&GP1_FILES);
    {
        let cache = Cache::open(&w.cache_path).unwrap();
        let s = cache.get_source("notes").unwrap().unwrap();
        cache.mark_scan_started(s.id).unwrap();
    }
    w.rename("Training Plan.md", "Plan.md");
    let weekly = w.text("Weekly Review.md");
    let (registry, engine) = w.registry();

    let rescanned = registry.rescan_unfinished(|| false);
    let outcomes = registry.start_all(|| false);

    assert_eq!(rescanned.len(), 1, "only the unfinished source");
    assert!(w.indexed("notes", "Plan.md") && !w.indexed("notes", "Training Plan.md"));
    assert!(
        outcomes
            .iter()
            .all(|(_, o)| *o == BootOutcome::CaughtUp { paths: 0 }),
        "{outcomes:?}"
    );
    std::thread::sleep(NEGATIVE_WAIT);
    assert_eq!(
        w.text("Weekly Review.md"),
        weekly,
        "no baseline, no pairing"
    );
    assert!(engine.list(None, 100).unwrap().is_empty());
}

// ---------------------------------------------------------------
// E8, E9: the write loop under catch-up
// ---------------------------------------------------------------

/// The real filesystem, except that the first file the writer looks at
/// is changed by someone else between its two looks at it.
struct RacingFs {
    looks: AtomicUsize,
}

impl AtomicFs for RacingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
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
    fn fingerprint(&self, path: &Path) -> io::Result<(u64, Option<SystemTime>)> {
        if self.looks.fetch_add(1, Ordering::SeqCst) == 1 {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(path)?;
            f.write_all(b"\nlate edit\n")?;
            drop(f);
            filetime::set_file_mtime(
                path,
                filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(5)),
            )?;
        }
        RealFs.fingerprint(path)
    }
}

#[test]
fn e8_a_linking_file_changed_between_the_read_and_the_write_is_not_overwritten() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let (registry, engine) = w.registry_over(
        Arc::new(RacingFs {
            looks: AtomicUsize::new(0),
        }),
        None,
    );

    registry.start_all(|| false);

    // Every linking file ends fixed, and the one that was edited under the
    // writer keeps its edit: the write was refused and tried again on the
    // text as it then was.
    let linkers = ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"];
    assert!(
        wait_until(WAIT, || {
            linkers
                .iter()
                .all(|f| !w.text(f).contains("[[Training Plan"))
                && engine
                    .list(None, 100)
                    .unwrap()
                    .iter()
                    .any(|e| e.kind == "rename")
        }),
        "{:?}",
        linkers.map(|f| w.text(f))
    );
    let edited: Vec<&str> = linkers
        .iter()
        .copied()
        .filter(|f| w.text(f).contains("late edit"))
        .collect();
    assert_eq!(
        edited.len(),
        1,
        "the late edit was kept exactly once: {edited:?}"
    );
    assert!(w.text(edited[0]).contains("Plan"));
    assert_eq!(
        engine
            .list(None, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "rename")
            .count(),
        1
    );
}

#[test]
fn e9_a_kill_after_the_first_of_several_files_is_finished_by_the_next_boot_none_twice() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let linkers = ["Weekly Review.md", "Goals.md", "Ideas/Rename Notes.md"];
    let fixed = |f: &str| w.text(f).contains("[[Plan") && !w.text(f).contains("[[Training Plan");

    // Boot 1 is killed after one write.
    {
        let (registry, _engine) = w.registry_over(Arc::new(RealFs), Some(1));
        registry.start_all(|| false);
        assert!(
            wait_until(WAIT, || linkers.iter().filter(|f| fixed(f)).count() == 1),
            "{:?}",
            linkers.map(|f| w.text(f))
        );
        // Nothing more is written however long the hold task is given.
        std::thread::sleep(NEGATIVE_WAIT);
        assert_eq!(linkers.iter().filter(|f| fixed(f)).count(), 1);
    }
    let done: Vec<&str> = linkers.iter().copied().filter(|f| fixed(f)).collect();
    let first = tree(w.root()).into_iter().find(|f| f.0 == done[0]).unwrap();

    // Boot 2: nothing changed on disk since the kill, and the pending
    // rows are finished by `resume`, as the daemon does after start.
    let b = w.boot();
    assert_eq!(b.outcome("notes"), &BootOutcome::CaughtUp { paths: 0 });
    b.engine.resume().unwrap();

    assert!(
        linkers.iter().all(|f| fixed(f)),
        "{:?}",
        linkers.map(|f| w.text(f))
    );
    let again = tree(w.root()).into_iter().find(|f| f.0 == done[0]).unwrap();
    assert_eq!(
        again, first,
        "the file written before the kill is not written again"
    );
    let renames = b.kinds("rename");
    assert_eq!(renames.len(), 1, "{:?}", b.entries());
    assert_eq!(renames[0].status, "applied");
}

// ---------------------------------------------------------------
// A stop during the hold does not lose the rename (PR-9 R3, R8)
// ---------------------------------------------------------------

impl World {
    fn held(&self) -> usize {
        Cache::open(&self.cache_path)
            .unwrap()
            .held_renames()
            .unwrap()
            .len()
    }

    /// A boot whose hold is far longer than any test: nothing it holds is
    /// decided before it is dropped, as a stop during the hold.
    fn boot_then_stop_during_the_hold(&self) {
        let (registry, engine) = self.registry();
        registry.set_rename_hold(Duration::from_secs(600));
        let outcomes = registry.start_all(|| false);
        assert!(
            matches!(
                outcomes.iter().find(|(n, _)| n == "notes").map(|o| &o.1),
                Some(BootOutcome::CaughtUp { .. })
            ),
            "{outcomes:?}"
        );
        // The catch-up has indexed the new path and dropped the old row:
        // the next start has nothing to compare, only what was held.
        assert!(self.indexed("notes", "Plan.md"));
        assert!(!self.indexed("notes", "Training Plan.md"));
        drop((registry, engine));
    }
}

/// R8: the catch-up deletes the vanished rows and indexes the new ones at
/// once, but a hash pair is only applied after its hold. If the daemon
/// stops during the hold, the next start finds the cache already current,
/// so without a durable record nothing would pair the two files and the
/// links to the old name stay dangling. The held files are kept before
/// the rows go, and the next start hands them back to the detector.
#[test]
fn r8_a_stop_during_the_hold_of_a_hash_pair_does_not_lose_the_rename() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");

    w.boot_then_stop_during_the_hold();
    assert_eq!(w.held(), 2, "the vanished file and the new one are kept");
    assert!(
        w.text("Weekly Review.md").contains("[[Training Plan]]"),
        "nothing was decided before the stop"
    );

    let b = w.boot();
    assert_eq!(b.outcome("notes"), &BootOutcome::CaughtUp { paths: 0 });
    assert!(
        wait_until(WAIT, || {
            w.text("Weekly Review.md").contains("[[Plan]]")
                && w.text("Goals.md").contains("[[Plan|plan]]")
        }),
        "the rename was not fixed: {:?}",
        w.text("Weekly Review.md")
    );
    assert!(
        wait_until(WAIT, || b.kinds("rename").len() == 1 && w.held() == 0),
        "{:?} held {}",
        b.entries(),
        w.held()
    );
    let renames = b.kinds("rename");
    assert_eq!(renames[0].old.as_deref(), Some("Training Plan.md"));
    assert_eq!(renames[0].new.as_deref(), Some("Plan.md"));
    assert_eq!(renames[0].files_changed, 3);
    assert!(b.uncertain().is_empty(), "{:?}", b.entries());
}

/// R8: a vanished file with no twin, held when the daemon stopped, is
/// still logged `uncertain` at the next start rather than forgotten.
#[test]
fn r8_a_stop_during_the_hold_of_a_vanished_file_still_logs_it_uncertain() {
    let w = World::new(&GP1_FILES);
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();

    let (registry, engine) = w.registry();
    registry.set_rename_hold(Duration::from_secs(600));
    registry.start_all(|| false);
    drop((registry, engine));
    assert_eq!(w.held(), 1);

    let b = w.boot();
    assert!(
        wait_until(WAIT, || b.uncertain() == ["Training Plan.md"]
            && w.held() == 0),
        "{:?} held {}",
        b.entries(),
        w.held()
    );
}

/// A held file that is no longer true at the next start (the vanished
/// file is back) is dropped, not paired.
#[test]
fn r8_a_held_vanished_file_that_is_back_at_the_next_start_is_dropped_not_paired() {
    let w = World::new(&GP1_FILES);
    fs::remove_file(w.root().join("Training Plan.md")).unwrap();
    let (registry, engine) = w.registry();
    registry.set_rename_hold(Duration::from_secs(600));
    registry.start_all(|| false);
    drop((registry, engine));
    assert_eq!(w.held(), 1);
    // Put it back byte for byte (a restore).
    let original = GP1_FILES
        .iter()
        .find(|(p, _)| *p == "Training Plan.md")
        .expect("fixture file")
        .1;
    w.notes.write("Training Plan.md", original);

    let b = w.boot();
    assert!(wait_until(WAIT, || w.held() == 0), "held {}", w.held());
    std::thread::sleep(NEGATIVE_WAIT);
    assert!(b.kinds("rename").is_empty(), "{:?}", b.entries());
    assert!(b.uncertain().is_empty(), "{:?}", b.entries());
}

// ---------------------------------------------------------------
// The boot-started watcher is a watcher
// ---------------------------------------------------------------

#[test]
fn an_edit_made_while_the_catch_up_runs_is_indexed_afterwards_not_missed() {
    let w = World::new(&[("Old.md", "0123456789\n"), ("Other.md", "x\n")]);
    w.rename("Old.md", "New.md");
    let before = w.unit_hash("notes", "Other.md");
    let (registry, _engine) = w.registry();
    let racing = |p: &Path| -> io::Result<Option<String>> {
        // The catch-up is at its hashing step: the user saves a file.
        fs::write(p.with_file_name("Other.md"), "x, and a late edit\n").unwrap();
        hash_file(p)
    };
    let hooks = BootHooks {
        hash_file: &racing,
        ..BootHooks::default()
    };

    registry.start_all_with(|| false, &hooks);

    assert!(
        wait_until(WAIT, || w.unit_hash("notes", "Other.md") != before),
        "the edit made during the catch-up was missed"
    );
}

#[test]
fn a_boot_started_watcher_reindexes_edit_create_and_delete_and_is_paused_and_stopped_like_any() {
    let w = World::new(&GP1_FILES);
    let b = w.boot();
    assert!(b.registry.is_watching("notes") && b.registry.is_watching("tephra"));

    // Edit, create, delete.
    let before = w.unit_hash("notes", "Unrelated.md");
    fs::write(
        w.root().join("Unrelated.md"),
        "# Unrelated\n\nedited after boot\n",
    )
    .unwrap();
    w.notes.write("Born.md", "# Born\n");
    fs::remove_file(w.root().join("Goals.md")).unwrap();
    assert!(
        wait_until(WAIT, || {
            w.unit_hash("notes", "Unrelated.md") != before
                && w.indexed("notes", "Born.md")
                && !w.indexed("notes", "Goals.md")
        }),
        "the watcher did not reindex"
    );

    // Paused: an edit waits; resumed: it is indexed.
    b.registry.pause_all_watchers();
    assert!(b.registry.watchers_paused());
    w.notes.write("Held.md", "# Held\n");
    std::thread::sleep(NEGATIVE_WAIT_PAUSED);
    assert!(!w.indexed("notes", "Held.md"), "indexed while paused");
    b.registry.resume_all_watchers();
    assert!(wait_until(WAIT, || w.indexed("notes", "Held.md")));

    // Unregistered: the watcher stops and the rows go.
    b.registry.unregister("notes").unwrap();
    assert!(!b.registry.is_watching("notes"));
    assert!(b.registry.is_watching("tephra"));
}

// ---------------------------------------------------------------
// Announcements, shutdown
// ---------------------------------------------------------------

#[test]
fn one_source_changed_per_source_after_the_catch_up_and_none_for_an_untouched_one() {
    let w = World::new(&GP1_FILES);
    fs::write(
        w.root().join("Unrelated.md"),
        "# Unrelated\n\nedited while down\n",
    )
    .unwrap();
    w.notes.write("Fresh.md", "# Fresh\n");
    let (registry, _engine) = w.registry();
    let mut events = registry.subscribe();

    registry.start_all(|| false);

    let sent = drain(&mut events);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let DaemonEvent::SourceChanged {
        source,
        paths,
        fixes,
    } = &sent[0]
    else {
        panic!("a file change, not a source-list change: {:?}", sent[0]);
    };
    assert_eq!(source, "notes");
    assert_eq!(paths.len(), 2);
    assert!(fixes.is_empty());
}

#[test]
fn a_shutdown_during_the_catch_up_stops_before_the_next_source_and_leaves_nothing_half_written() {
    let w = World::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    w.tephra.write("new-in-tephra.md", "# new\n");
    let (registry, _engine) = w.registry();
    // `start_all` asks once per source in each of its three passes; the
    // shutdown arrives as the last pass reaches the second source.
    let asked = AtomicUsize::new(0);

    let outcomes = registry.start_all(|| asked.fetch_add(1, Ordering::SeqCst) >= 5);

    let outcome = |name: &str| outcomes.iter().find(|(n, _)| n == name).unwrap().1.clone();
    assert!(
        matches!(outcome("notes"), BootOutcome::CaughtUp { .. }),
        "{outcomes:?}"
    );
    assert_eq!(outcome("tephra"), BootOutcome::Stopped);
    assert!(
        w.indexed("notes", "Plan.md"),
        "the first source was finished"
    );
    assert!(
        !w.indexed("tephra", "new-in-tephra.md"),
        "the second was not started"
    );
    assert!(registry.is_watching("notes"));
    assert!(!registry.is_watching("tephra"));
}
