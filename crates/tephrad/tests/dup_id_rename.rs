//! G19 and E7 end to end: a duplicate block ID made by an edit is renamed
//! on disk (the original keeps its ID), and nothing else is ever written.
//! Real temp-dir sources, the real cache and the real scan paths (a full
//! scan, a rescan, the watcher's change batch, and the real registry
//! with a live watcher for the "no loop" case). The write goes through
//! an injected `AtomicFs` where a test needs it to fail or to race.

mod fixtures;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use tephra_cache::{Cache, SourceStatus};
use tephrad::ignore::IgnoreRules;
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::{DaemonEvent, SourceRegistry};
use tephrad::scan;

struct Env {
    _cache_dir: tempfile::TempDir,
    cache_path: PathBuf,
    cache: Cache,
    source_id: i64,
    root: PathBuf,
}

fn env_for(fixture: &fixtures::TempSource) -> Env {
    install_logging();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");
    let root = fixture.root().canonicalize().expect("canonicalize");
    let source_id = cache
        .register_source("notes", &root.to_string_lossy(), SourceStatus::Active)
        .expect("register_source");
    Env {
        _cache_dir: cache_dir,
        cache_path,
        cache,
        source_id,
        root,
    }
}

fn scan_all(env: &Env) {
    scan::scan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .expect("scan");
}

fn batch_with(env: &Env, fs: &dyn AtomicFs, paths: &[&str]) -> scan::ChangeOutcome {
    let changed: HashSet<PathBuf> = paths.iter().map(|p| env.root.join(p)).collect();
    scan::apply_change_batch_with(
        &env.cache,
        env.source_id,
        &env.root,
        &changed,
        &IgnoreRules::default(),
        fs,
    )
    .expect("batch")
}

fn batch(env: &Env, paths: &[&str]) -> scan::ChangeOutcome {
    batch_with(env, &RealFs, paths)
}

/// The `(id, line)` of every block row of `rel`, in line order.
fn blocks_of(env: &Env, rel: &str) -> Vec<(String, i64)> {
    let unit = env
        .cache
        .get_unit_by_rel_path(env.source_id, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("no unit {rel}"));
    let mut rows: Vec<(String, i64)> = env
        .cache
        .blocks_for_unit(unit.id)
        .unwrap()
        .into_iter()
        .filter_map(|b| b.block_id.map(|id| (id, b.line)))
        .collect();
    rows.sort_by_key(|r| r.1);
    rows
}

fn read(env: &Env, rel: &str) -> String {
    std::fs::read_to_string(env.root.join(rel)).unwrap()
}

/// `(bytes, mtime)`: what "nothing was written" means.
fn snapshot(path: &Path) -> (Vec<u8>, SystemTime) {
    (
        std::fs::read(path).unwrap(),
        std::fs::metadata(path).unwrap().modified().unwrap(),
    )
}

fn temp_files(env: &Env) -> Vec<String> {
    walkdir::WalkDir::new(&env.root)
        .into_iter()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tephra-tmp-"))
        .collect()
}

fn six_char_id(id: &str) -> bool {
    id.len() == 6
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Where log lines go. One subscriber is installed for the whole test
/// binary, once, and stays: a subscriber set per test with
/// `with_default` races with the tests running in parallel threads
/// (tracing caches, per callsite, whether anyone is listening, and a
/// callsite first hit by a thread with no subscriber can stay silent for
/// a test that sets one a moment later), which left some captures empty.
/// With a global one every callsite is always on, and each log line
/// goes to two places: the capture of the thread that wrote it, if that
/// thread is inside [`logged`] (so one test never reads another's
/// lines), and a shared buffer of everything ([`all_logs`]) for a test
/// whose lines are written by the daemon's own threads.
#[derive(Clone, Copy)]
struct LogRouter;

static ALL_LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

impl io::Write for LogRouter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        ALL_LOGS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        CAPTURE.with(|c| {
            if let Some(own) = c.borrow_mut().as_mut() {
                own.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogRouter {
    type Writer = LogRouter;
    fn make_writer(&'a self) -> LogRouter {
        *self
    }
}

fn install_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(LogRouter)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in this test binary");
    });
}

/// Runs `f` and returns what it returned and every log line this thread
/// wrote meanwhile.
fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
    install_logging();
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let out = f();
    let bytes = CAPTURE
        .with(|c| c.borrow_mut().take())
        .expect("capture was set above");
    (out, String::from_utf8(bytes).unwrap())
}

/// Every log line any thread of this test binary has written so far
/// that mentions `needle` (a source name only one test uses).
fn all_logs(needle: &str) -> String {
    let bytes = ALL_LOGS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| l.contains(needle))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------
// G19: prose
// ---------------------------------------------------------------

#[test]
fn a_paragraph_copied_below_the_original_is_renamed_and_logged_and_the_cache_has_it() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);
    assert_eq!(blocks_of(&env, "Plan.md"), vec![("summary".into(), 3)]);

    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    let (outcome, log) = logged(|| batch(&env, &["Plan.md"]));

    // F4: a byte diff is exactly the one token.
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    // The cache records the new ID straight away, at the right lines.
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
    assert_eq!(outcome.changed_paths, vec![env.root.join("Plan.md")]);
    // One log line: unit id, old ID, new ID and line.
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("renamed a duplicate block ID"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    for want in [
        "unit=@notes:Plan.md",
        "old_id=summary",
        "new_id=summary-1",
        "line=5",
    ] {
        assert!(lines[0].contains(want), "{want} not in {}", lines[0]);
    }
    assert!(temp_files(&env).is_empty());

    // A batch for the renamed file again changes nothing (no loop).
    let before = snapshot(&env.root.join("Plan.md"));
    let (_, log) = logged(|| batch(&env, &["Plan.md"]));
    assert_eq!(snapshot(&env.root.join("Plan.md")), before);
    assert!(!log.contains("renamed a duplicate block ID"), "{log}");
}

#[test]
fn a_paragraph_copied_above_the_original_renames_the_copy() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    // The copy lands above the heading, which pushes the original down.
    fixture.write("Plan.md", "para ^summary\n\n# Plan\n\npara ^summary\n");
    batch(&env, &["Plan.md"]);

    assert_eq!(
        read(&env, "Plan.md"),
        "para ^summary-1\n\n# Plan\n\npara ^summary\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary-1".into(), 1), ("summary".into(), 5)]
    );
}

#[test]
fn a_tephra_format_id_gets_a_fresh_six_character_id() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "para ^k3f9a2\n");
    let env = env_for(&fixture);
    scan_all(&env);

    fixture.write("n.md", "para ^k3f9a2\n\nmore\n\npara ^k3f9a2\n");
    batch(&env, &["n.md"]);

    let after = read(&env, "n.md");
    let blocks = blocks_of(&env, "n.md");
    assert_eq!(blocks.len(), 2, "{after}");
    assert_eq!(blocks[0], ("k3f9a2".into(), 1));
    assert!(six_char_id(&blocks[1].0), "{blocks:?}");
    assert_ne!(blocks[1].0, "k3f9a2");
    assert_eq!(
        after,
        format!("para ^k3f9a2\n\nmore\n\npara ^{}\n", blocks[1].0)
    );
}

#[test]
fn three_copies_with_a_suffix_already_in_use_become_2_and_3() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "a ^summary\n\nother ^summary-1\n");
    let env = env_for(&fixture);
    scan_all(&env);

    fixture.write(
        "n.md",
        "a ^summary\n\nother ^summary-1\n\na ^summary\n\na ^summary\n",
    );
    batch(&env, &["n.md"]);

    assert_eq!(
        read(&env, "n.md"),
        "a ^summary\n\nother ^summary-1\n\na ^summary-2\n\na ^summary-3\n"
    );
}

#[test]
fn a_new_duplicate_is_fixed_while_an_old_one_beside_it_is_left() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "old ^dup\n\nold ^dup\n\nfresh ^new\n");
    let env = env_for(&fixture);
    scan_all(&env);
    let first = snapshot(&env.root.join("n.md"));
    assert_eq!(first.0, b"old ^dup\n\nold ^dup\n\nfresh ^new\n");

    fixture.write("n.md", "old ^dup\n\nold ^dup\n\nfresh ^new\n\nfresh ^new\n");
    batch(&env, &["n.md"]);
    assert_eq!(
        read(&env, "n.md"),
        "old ^dup\n\nold ^dup\n\nfresh ^new\n\nfresh ^new-1\n"
    );
}

// ---------------------------------------------------------------
// G19: code
// ---------------------------------------------------------------

#[test]
fn a_copied_rust_fn_with_its_id_comment_gets_a_fresh_id_in_the_copys_comment_only() {
    let fixture = fixtures::empty_source();
    let one = "// Parses.\n// ^k3f9a2\nfn parse() {}\n";
    fixture.write("src/lib.rs", one);
    let env = env_for(&fixture);
    scan_all(&env);

    let copied = format!("{one}\n{one}");
    fixture.write("src/lib.rs", &copied);
    batch(&env, &["src/lib.rs"]);

    let blocks = blocks_of(&env, "src/lib.rs");
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert_eq!(blocks[0], ("k3f9a2".into(), 3));
    let new = blocks[1].0.clone();
    assert!(six_char_id(&new), "{new}");
    assert_eq!(blocks[1].1, 7);
    let after = read(&env, "src/lib.rs");
    assert_eq!(
        after.replace(&new, "k3f9a2"),
        copied,
        "only the one token changed"
    );
    assert!(temp_files(&env).is_empty());
}

#[test]
fn a_trailing_id_comment_in_python_and_lua_is_renamed_in_place() {
    let fixture = fixtures::empty_source();
    fixture.write("m.py", "def a():  # why ^k3f9a2\n    pass\n");
    fixture.write("m.lua", "function a() end -- why ^k3f9a2\n");
    let env = env_for(&fixture);
    scan_all(&env);

    fixture.write(
        "m.py",
        "def a():  # why ^k3f9a2\n    pass\n\ndef a():  # why ^k3f9a2\n    pass\n",
    );
    fixture.write(
        "m.lua",
        "function a() end -- why ^k3f9a2\nfunction a() end -- why ^k3f9a2\n",
    );
    batch(&env, &["m.py", "m.lua"]);

    for (rel, original) in [
        (
            "m.py",
            "def a():  # why ^k3f9a2\n    pass\n\ndef a():  # why ^k3f9a2\n    pass\n",
        ),
        (
            "m.lua",
            "function a() end -- why ^k3f9a2\nfunction a() end -- why ^k3f9a2\n",
        ),
    ] {
        let blocks = blocks_of(&env, rel);
        assert_eq!(blocks.len(), 2, "{rel}: {blocks:?}");
        assert_eq!(blocks[0].0, "k3f9a2");
        assert!(six_char_id(&blocks[1].0), "{rel}: {blocks:?}");
        assert_eq!(
            read(&env, rel).replace(&blocks[1].0, "k3f9a2"),
            original,
            "{rel}"
        );
    }
}

// ---------------------------------------------------------------
// E7: what is never written
// ---------------------------------------------------------------

#[test]
fn a_file_scanned_with_duplicates_is_never_written_and_resolving_lists_every_line() {
    let fixture = fixtures::empty_source();
    fixture.write("Old.md", "para ^summary\n\nother\n\npara ^summary\n");
    let env = env_for(&fixture);
    let before = snapshot(&env.root.join("Old.md"));

    let (_, log) = logged(|| scan_all(&env));
    assert_eq!(snapshot(&env.root.join("Old.md")), before);
    assert_eq!(
        blocks_of(&env, "Old.md"),
        vec![("summary".into(), 1), ("summary".into(), 5)]
    );
    assert!(log.contains("duplicate block id"), "{log}");

    // The same duplicates, edited elsewhere in the file: the cache held
    // the ID twice, so it is still not rewritten.
    fixture.write(
        "Old.md",
        "para ^summary\n\nother, edited\n\npara ^summary\n",
    );
    batch(&env, &["Old.md"]);
    assert_eq!(
        read(&env, "Old.md"),
        "para ^summary\n\nother, edited\n\npara ^summary\n"
    );
    // The ambiguity lists every line.
    let found = env.cache.blocks_with_id(env.source_id, "summary").unwrap();
    assert_eq!(found.iter().map(|o| o.line).collect::<Vec<_>>(), vec![1, 5]);
}

#[test]
fn a_rebuild_from_a_v1_cache_counts_as_a_first_scan() {
    let fixture = fixtures::empty_source();
    fixture.write("Old.md", "para ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    // The file gains a duplicate while the daemon is off, and the cache
    // is from before schema v2.
    fixture.write("Old.md", "para ^summary\n\npara ^summary\n");
    let before = snapshot(&env.root.join("Old.md"));
    {
        let conn = rusqlite::Connection::open(&env.cache_path).unwrap();
        conn.execute("UPDATE schema_version SET version = 1", [])
            .unwrap();
    }
    let Env {
        cache_path,
        source_id,
        root,
        ..
    } = env;
    let cache = Cache::open(&cache_path).expect("reopen rebuilds");
    assert_eq!(cache.rebuilt_from_version(), Some(1));
    assert!(
        cache
            .get_unit_by_rel_path(source_id, "Old.md")
            .unwrap()
            .is_none(),
        "the rebuild emptied the core tables"
    );
    assert!(
        !cache.sources_with_unfinished_scan().unwrap().is_empty(),
        "the source is left to be rescanned"
    );

    scan::rescan_source(&cache, source_id, &root, &IgnoreRules::default()).unwrap();
    assert_eq!(snapshot(&root.join("Old.md")), before);
    assert_eq!(cache.blocks_with_id(source_id, "summary").unwrap().len(), 2);
}

// E7 variant (spec: "a duplicate added while tephrad was stopped is renamed
// on the startup rescan, because the unit already has cached rows"). What
// the daemon really does at start: it redoes, from the top, only a scan
// that never finished (`SourceRegistry::rescan_unfinished`); after a clean
// stop nothing is rescanned and no watcher is started for a registered
// source. So the case is the next *full rescan* of a source whose unit
// already has cached rows: an interrupted scan redone at start, or
// `source.rescan`. Both run `scan::rescan_source`, which this calls.
#[test]
fn a_duplicate_added_while_the_daemon_was_stopped_is_renamed_by_the_next_full_rescan_of_an_interrupted_scan(
) {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    // Stopped: nobody watches; the file is edited, and the scan that
    // was running when the daemon last stopped never finished.
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    env.cache.mark_scan_started(env.source_id).unwrap();
    assert_eq!(env.cache.sources_with_unfinished_scan().unwrap().len(), 1);

    let (stats, log) = logged(|| {
        scan::rescan_source(
            &env.cache,
            env.source_id,
            &env.root,
            &IgnoreRules::default(),
        )
        .unwrap()
    });
    assert_eq!(stats.units_written, 1);
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
    assert!(log.contains("new_id=summary-1"), "{log}");
}

#[test]
fn an_id_that_was_not_in_the_files_cache_and_shows_up_twice_is_not_written_but_is_logged() {
    let fixture = fixtures::empty_source();
    fixture.write("a.md", "# A\n\nmine ^mine1\n");
    let env = env_for(&fixture);
    scan_all(&env);

    // A block from another file is pasted twice.
    let edited = "# A\n\nmine ^mine1\n\npasted ^other\n\npasted ^other\n";
    fixture.write("a.md", edited);
    let before = snapshot(&env.root.join("a.md"));
    let (_, log) = logged(|| batch(&env, &["a.md"]));

    assert_eq!(snapshot(&env.root.join("a.md")), before);
    assert_eq!(read(&env, "a.md"), edited);
    assert_eq!(
        blocks_of(&env, "a.md"),
        vec![
            ("mine1".into(), 3),
            ("other".into(), 5),
            ("other".into(), 7)
        ]
    );
    assert!(
        log.contains("duplicate block ID left as it is") && log.contains("other"),
        "{log}"
    );
}

#[test]
fn the_same_id_in_two_files_is_never_rewritten_in_either() {
    let fixture = fixtures::empty_source();
    let drawing = "---\nexcalidraw-plugin: parsed\n---\n\n# Text Elements\ntext ^abcd1234\n";
    fixture.write("one.excalidraw.md", drawing);
    fixture.write("two.excalidraw.md", drawing);
    fixture.write("a.md", "para ^same\n");
    fixture.write("b.md", "para ^same\n");
    let env = env_for(&fixture);
    scan_all(&env);
    let snaps: Vec<_> = ["one.excalidraw.md", "two.excalidraw.md", "a.md", "b.md"]
        .iter()
        .map(|p| snapshot(&env.root.join(p)))
        .collect();

    // Both files are edited (touched) and batched.
    fixture.write("a.md", "para ^same\n\nmore\n");
    fixture.write("b.md", "para ^same\n\nmore\n");
    batch(
        &env,
        &["one.excalidraw.md", "two.excalidraw.md", "a.md", "b.md"],
    );

    assert_eq!(snapshot(&env.root.join("one.excalidraw.md")), snaps[0]);
    assert_eq!(snapshot(&env.root.join("two.excalidraw.md")), snaps[1]);
    assert_eq!(read(&env, "a.md"), "para ^same\n\nmore\n");
    assert_eq!(read(&env, "b.md"), "para ^same\n\nmore\n");
    assert_eq!(
        env.cache
            .blocks_with_id(env.source_id, "same")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn an_id_in_a_fence_a_code_span_frontmatter_or_a_string_literal_is_never_renamed() {
    let fixture = fixtures::empty_source();
    fixture.write("n.md", "---\nnote: x ^k\n---\npara ^k\n");
    fixture.write("s.rs", "// ^k3f9a2\nfn a() {}\n");
    let env = env_for(&fixture);
    scan_all(&env);

    let md =
        "---\nnote: x ^k\n---\npara ^k\n\n```\ncode ^k\n```\n\nuse `x ^k` here ^k2\n\nuse `y ^k`\n";
    fixture.write("n.md", md);
    let rs = "// ^k3f9a2\nfn a() {}\n\nconst S: &str = \"// ^k3f9a2\";\nconst T: &str = \"x // ^k3f9a2\";\n";
    fixture.write("s.rs", rs);
    batch(&env, &["n.md", "s.rs"]);

    assert_eq!(read(&env, "n.md"), md);
    assert_eq!(read(&env, "s.rs"), rs);
}

#[test]
fn documents_text_other_and_binary_files_are_never_written() {
    let fixture = fixtures::empty_source();
    fixture.write("data.log", "para ^x\n");
    fixture.write_bytes("blob.bin", b"para ^x\n\0\n");
    fixture.write_bytes("paper.pdf", b"%PDF-1.4 para ^x\n\npara ^x\n");
    let env = env_for(&fixture);
    scan_all(&env);

    fixture.write("data.log", "para ^x\n\npara ^x\n");
    fixture.write_bytes("blob.bin", b"para ^x\n\npara ^x\n\0\n");
    fixture.write_bytes("paper.pdf", b"%PDF-1.4 para ^x\n\npara ^x\n\npara ^x\n");
    let snaps: Vec<_> = ["data.log", "blob.bin", "paper.pdf"]
        .iter()
        .map(|p| snapshot(&env.root.join(p)))
        .collect();
    batch(&env, &["data.log", "blob.bin", "paper.pdf"]);
    for (p, snap) in ["data.log", "blob.bin", "paper.pdf"].iter().zip(snaps) {
        assert_eq!(snapshot(&env.root.join(p)), snap, "{p}");
    }
}

// ---------------------------------------------------------------
// Failures and races
// ---------------------------------------------------------------

/// The real filesystem, except that the write of the temp file fails.
struct FailingWrite {
    attempts: AtomicUsize,
}

impl AtomicFs for FailingWrite {
    fn write_new(&self, _path: &Path, _content: &[u8]) -> io::Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("No space left on device"))
    }
    fn set_permissions(&self, path: &Path, perms: std::fs::Permissions) -> io::Result<()> {
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
fn a_failed_write_leaves_the_file_intact_logs_and_keeps_the_duplicate_rows() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    fixture.write("other.md", "# Other\n");
    let env = env_for(&fixture);
    scan_all(&env);

    let edited = "# Plan\n\npara ^summary\n\npara ^summary\n";
    fixture.write("Plan.md", edited);
    fixture.write("other.md", "# Other, edited\n");
    let before = snapshot(&env.root.join("Plan.md"));
    let fs = FailingWrite {
        attempts: AtomicUsize::new(0),
    };
    let (outcome, log) = logged(|| batch_with(&env, &fs, &["Plan.md", "other.md"]));

    assert_eq!(fs.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot(&env.root.join("Plan.md")), before);
    assert_eq!(read(&env, "Plan.md"), edited);
    assert!(temp_files(&env).is_empty());
    // The cache keeps the duplicate rows, and the scan carried on.
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary".into(), 5)]
    );
    assert_eq!(outcome.changed_paths.len(), 2);
    assert!(
        log.contains("could not write the file") && log.contains("No space left"),
        "{log}"
    );
}

#[test]
fn a_read_only_file_is_not_renamed() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "para ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    let path = env.root.join("Plan.md");
    fixture.write("Plan.md", "para ^summary\n\npara ^summary\n");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    let before = snapshot(&path);
    let (_, log) = logged(|| batch(&env, &["Plan.md"]));

    assert_eq!(snapshot(&path), before);
    assert!(log.contains("read-only"), "{log}");
    assert_eq!(blocks_of(&env, "Plan.md").len(), 2);

    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// The real filesystem, except that the file is changed by someone else
/// between the two looks at it: the second [`AtomicFs::fingerprint`] call
/// appends a line to the file first.
struct RacingFs {
    looks: AtomicUsize,
}

impl AtomicFs for RacingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: std::fs::Permissions) -> io::Result<()> {
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
            let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
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
fn a_file_that_changes_between_the_read_and_the_write_is_not_written_and_the_next_batch_sees_it() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "para ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    fixture.write("Plan.md", "para ^summary\n\npara ^summary\n");
    let fs = RacingFs {
        looks: AtomicUsize::new(0),
    };
    let (outcome, log) = logged(|| batch_with(&env, &fs, &["Plan.md"]));

    // Nothing of ours was written: the file is the user's, with the late
    // edit, and no temp file is left.
    assert_eq!(
        read(&env, "Plan.md"),
        "para ^summary\n\npara ^summary\n\nlate edit\n"
    );
    assert!(temp_files(&env).is_empty());
    assert!(log.contains("the file changed since it was read"), "{log}");
    // The file is left for the next batch: its rows are as they were.
    assert!(outcome.changed_paths.is_empty());
    assert_eq!(outcome.deferred, vec![env.root.join("Plan.md")]);
    assert_eq!(blocks_of(&env, "Plan.md"), vec![("summary".into(), 1)]);

    // The next batch reads the new content and tries again.
    batch(&env, &["Plan.md"]);
    assert_eq!(
        read(&env, "Plan.md"),
        "para ^summary\n\npara ^summary-1\n\nlate edit\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 1), ("summary-1".into(), 3)]
    );
}

// ---------------------------------------------------------------
// A deferred file is tried again (a lock holder, a rescan)
// ---------------------------------------------------------------

/// A source with `Plan.md` indexed with one `^summary`, then edited to
/// hold it twice, with another writer (a `block.ensure_id`) holding the
/// file's lock.
fn locked_duplicate(
    fixture: &fixtures::TempSource,
) -> (Env, PathBuf, tephrad::file_lock::FileLockGuard) {
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let env = env_for(fixture);
    scan_all(&env);
    let path = env.root.join("Plan.md");
    let guard = tephrad::file_lock::lock(&path);
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    (env, path, guard)
}

fn settle(
    env: &Env,
    paths: &[&str],
    fs: &dyn AtomicFs,
    mut on_round: impl FnMut(&scan::ChangeOutcome),
) -> (scan::ChangeOutcome, Vec<bool>) {
    let changed: HashSet<PathBuf> = paths.iter().map(|p| env.root.join(p)).collect();
    let mut asked_to_defer = Vec::new();
    let outcome = scan::settle_deferred(
        changed,
        Duration::from_millis(1),
        |paths, can_defer| {
            asked_to_defer.push(can_defer);
            scan::apply_change_batch_deferring(
                &env.cache,
                env.source_id,
                &env.root,
                paths,
                &IgnoreRules::default(),
                fs,
                can_defer,
            )
        },
        |o| on_round(o),
    )
    .expect("settle");
    (outcome, asked_to_defer)
}

#[test]
fn a_file_another_writer_holds_is_handed_back_with_its_rows_as_they_were() {
    let fixture = fixtures::empty_source();
    let (env, path, guard) = locked_duplicate(&fixture);
    let edited = snapshot(&path);

    // One pass, as a watcher batch: nothing written, nothing re-indexed,
    // and the caller is told which file to try again.
    let (outcome, log) = logged(|| batch(&env, &["Plan.md"]));
    assert_eq!(snapshot(&path), edited);
    assert!(outcome.changed_paths.is_empty());
    assert_eq!(outcome.deferred, vec![path.clone()]);
    assert_eq!(blocks_of(&env, "Plan.md"), vec![("summary".into(), 3)]);
    assert!(log.contains("another writer has the file"), "{log}");

    // Once the holder lets go, the next try renames the copy.
    drop(guard);
    let outcome = batch(&env, &["Plan.md"]);
    assert!(outcome.deferred.is_empty());
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
}

#[test]
fn a_file_that_is_held_at_first_is_renamed_once_the_holder_lets_go() {
    let fixture = fixtures::empty_source();
    let (env, _path, guard) = locked_duplicate(&fixture);

    // The holder lets go after the first round came back deferred: no
    // file event is involved, only the retry.
    let mut guard = Some(guard);
    let (outcome, asked) = settle(&env, &["Plan.md"], &RealFs, |round| {
        if !round.deferred.is_empty() {
            guard.take();
        }
    });
    assert_eq!(asked, vec![true, true], "one deferred round, one that ran");
    assert!(outcome.deferred.is_empty());
    assert_eq!(outcome.changed_paths, vec![env.root.join("Plan.md")]);
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
}

#[test]
fn a_file_that_stays_held_is_indexed_as_it_is_after_the_last_try() {
    let fixture = fixtures::empty_source();
    let (env, path, _guard) = locked_duplicate(&fixture);
    let edited = snapshot(&path);

    let (outcome, asked) = settle(&env, &["Plan.md"], &RealFs, |_| {});

    // Eight rounds that may defer, then one that may not.
    assert_eq!(asked.len(), scan::DEFER_RETRIES as usize + 1);
    assert!(asked[..asked.len() - 1].iter().all(|d| *d));
    assert!(!asked[asked.len() - 1]);
    assert!(outcome.deferred.is_empty());
    // Nothing was written, and the cache is no longer behind the file
    // (a `block.ensure_id` would otherwise refuse against old rows).
    assert_eq!(snapshot(&path), edited);
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary".into(), 5)]
    );
    let unit = env
        .cache
        .get_unit_by_rel_path(env.source_id, "Plan.md")
        .unwrap()
        .unwrap();
    assert_eq!(
        unit.mtime,
        Some(
            edited
                .1
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        )
    );
}

#[test]
fn a_later_edit_renames_the_copy_of_a_file_that_stayed_held_through_every_try() {
    let fixture = fixtures::empty_source();
    let (env, _path, guard) = locked_duplicate(&fixture);

    // Every try is refused, and the last one indexes both copies.
    settle(&env, &["Plan.md"], &RealFs, |_| {});
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary".into(), 5)]
    );

    // The holder lets go and the file is edited again: the next batch
    // plans against the IDs the cache held before the first edit, not
    // against the two rows it has now, and renames the copy.
    drop(guard);
    fixture.write(
        "Plan.md",
        "# Plan\n\npara ^summary\n\npara ^summary\n\nmore\n",
    );
    let (outcome, log) = logged(|| batch(&env, &["Plan.md"]));
    assert!(outcome.deferred.is_empty());
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n\nmore\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
    assert!(log.contains("renamed a duplicate block ID"), "{log}");
    assert!(temp_files(&env).is_empty());

    // Done with: a third batch has nothing left to rename.
    let renamed = snapshot(&env.root.join("Plan.md"));
    batch(&env, &["Plan.md"]);
    assert_eq!(snapshot(&env.root.join("Plan.md")), renamed);
}

#[test]
fn a_rescan_renames_the_copy_of_a_file_whose_rename_was_given_up_on() {
    let fixture = fixtures::empty_source();
    let (env, _path, guard) = locked_duplicate(&fixture);
    settle(&env, &["Plan.md"], &RealFs, |_| {});
    drop(guard);

    // The cache holds `^summary` twice, but the rescan reads the IDs the
    // file had before the edit that could not be renamed.
    scan::rescan_source(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
    )
    .unwrap();
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 3), ("summary-1".into(), 5)]
    );
}

#[test]
fn a_file_that_gave_up_and_then_lost_its_copy_is_judged_by_the_cache_again() {
    let fixture = fixtures::empty_source();
    let (env, _path, guard) = locked_duplicate(&fixture);
    settle(&env, &["Plan.md"], &RealFs, |_| {});
    drop(guard);

    // The user removes the copy: nothing to rename, and the memory of the
    // given-up rename goes with it.
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    batch(&env, &["Plan.md"]);
    assert_eq!(blocks_of(&env, "Plan.md"), vec![("summary".into(), 3)]);

    // A new copy is an ordinary one: the cache held the ID once.
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    batch(&env, &["Plan.md"]);
    assert_eq!(
        read(&env, "Plan.md"),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
}

#[test]
fn a_rescan_retries_a_file_that_changed_under_it_instead_of_indexing_both_copies() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "para ^summary\n");
    let env = env_for(&fixture);
    scan_all(&env);

    // The daemon was stopped and the file gained a copy. The rescan reads
    // it, and someone else edits it before the rename (the mock appends
    // a line the second time the file is looked at).
    fixture.write("Plan.md", "para ^summary\n\npara ^summary\n");
    let fs = RacingFs {
        looks: AtomicUsize::new(0),
    };
    let stats = scan::rescan_source_with(
        &env.cache,
        env.source_id,
        &env.root,
        &IgnoreRules::default(),
        &fs,
    )
    .unwrap();

    // Not indexed with `^summary` twice (which would leave the copy
    // never renamed): it was tried again with the IDs the cache had, saw
    // the new content, and renamed the copy.
    assert_eq!(
        read(&env, "Plan.md"),
        "para ^summary\n\npara ^summary-1\n\nlate edit\n"
    );
    assert_eq!(
        blocks_of(&env, "Plan.md"),
        vec![("summary".into(), 1), ("summary-1".into(), 3)]
    );
    assert_eq!(stats.units_written, 1);
    assert!(temp_files(&env).is_empty());
    assert!(env.cache.sources_with_unfinished_scan().unwrap().is_empty());
}

#[test]
fn a_watcher_batch_that_finds_the_file_locked_renames_it_when_the_holder_lets_go() {
    // A live watcher; no other test in this file uses this source name,
    // so its log lines (written on the daemon's own threads) can be told
    // apart in the shared buffer.
    const SOURCE: &str = "lockwatch";
    install_logging();
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register(SOURCE, fixture.root()).unwrap();
    let plan = fixture.root().join("Plan.md");
    let cache = Cache::open(&cache_path).unwrap();
    let source_id = cache.get_source(SOURCE).unwrap().unwrap().id;
    let ids = |id: &str| cache.blocks_with_id(source_id, id).unwrap().len();

    // `block.ensure_id` holds the file while the user pastes a copy.
    let guard = tephrad::file_lock::lock(&plan);
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    let deferred = wait_until(Duration::from_secs(10), || {
        all_logs(&format!("@{SOURCE}:")).contains("another writer has the file")
    });
    assert!(deferred, "the batch never met the lock");
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# Plan\n\npara ^summary\n\npara ^summary\n",
        "nothing is written while the lock is held"
    );
    assert_eq!((ids("summary"), ids("summary-1")), (1, 0));

    // The holder lets go. No further file event comes; the batch tries
    // again by itself.
    drop(guard);
    let renamed = wait_until(Duration::from_secs(10), || {
        std::fs::read_to_string(&plan)
            .map(|c| c.contains("^summary-1"))
            .unwrap_or(false)
    });
    assert!(renamed, "the batch never tried the locked file again");
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    assert!(wait_until(Duration::from_secs(10), || {
        (ids("summary"), ids("summary-1")) == (1, 1)
    }));
    registry.unregister(SOURCE).unwrap();
}

// ---------------------------------------------------------------
// No loop: the live watcher
// ---------------------------------------------------------------

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

/// Counts `SourceChanged` events (one per applied watcher batch that
/// changed something) until the stream has been quiet for `quiet`.
fn count_batches(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    first_within: Duration,
    quiet: Duration,
) -> usize {
    let mut count = 0;
    let mut deadline = Instant::now() + first_within;
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(DaemonEvent::SourceChanged { .. }) => {
                count += 1;
                deadline = Instant::now() + quiet;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    count
}

#[test]
fn a_rename_through_the_live_watcher_causes_exactly_one_batch_and_no_further_rename() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    let mut events = registry.subscribe();
    registry.register("notes", fixture.root()).unwrap();
    let plan = fixture.root().join("Plan.md");

    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    let renamed = wait_until(Duration::from_secs(10), || {
        std::fs::read_to_string(&plan)
            .map(|c| c.contains("^summary-1"))
            .unwrap_or(false)
    });
    assert!(renamed, "the watcher never renamed the copy");
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );
    let after_rename = snapshot(&plan);

    // The one batch of the user's edit; then quiet: Tephra's own write
    // is not a batch of its own, and not a second rename.
    let batches = count_batches(&mut events, Duration::from_secs(5), Duration::from_secs(3));
    assert_eq!(batches, 1, "the rename started another batch");
    assert_eq!(snapshot(&plan), after_rename);

    // The cache has the new ID.
    let cache = Cache::open(&cache_path).unwrap();
    let source = cache.get_source("notes").unwrap().unwrap();
    assert_eq!(
        cache.blocks_with_id(source.id, "summary-1").unwrap().len(),
        1
    );
    assert_eq!(cache.blocks_with_id(source.id, "summary").unwrap().len(), 1);
    registry.unregister("notes").unwrap();
}
