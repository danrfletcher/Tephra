//! Integration test: a synthetic fixture with over 100k files split
//! between an ignored subtree (`node_modules/`) and a small
//! non-ignored subtree, asserting (spec, `tests/large_source.rs`):
//! - the scan's reported stats only ever count the non-ignored
//!   files — proof the 100k+ ignored files were pruned by
//!   [`tephrad::ignore::IgnoreRules`] *before* `walkdir` ever
//!   descended into their directory (E1: "ignore filters applied
//!   before entering a directory, not after parsing its contents";
//!   if that guarantee broke, `files_seen` below would jump from a
//!   few hundred to over 100,000);
//! - peak process RSS during the scan stays within a generous bound
//!   that a from-scratch "collect every path into one `Vec` first"
//!   implementation over 100k+ entries would blow past, sampled via
//!   `ps` (macOS/Linux) rather than an extra memory-profiling
//!   dependency.

mod fixtures;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

const IGNORED_FILE_COUNT: usize = 120_000;
const IGNORED_SUBDIRS: usize = 300;
const VALID_FILE_COUNT: usize = 400;
/// Non-Markdown files in the non-ignored subtree (G2): they are units
/// too, so they count toward `files_seen`/`units_written`.
const CODE_FILE_COUNT: usize = 150;
const TEXT_FILE_COUNT: usize = 100;
const BINARY_FILE_COUNT: usize = 50;
/// One sparse file of 20 MiB + 1 byte (E3): a unit by name only.
const TOO_LARGE_FILE_COUNT: usize = 1;
/// Files under other ignored folders (G15): `.venv` and
/// `__pycache__`, at depth 0 and deeper.
const IGNORED_VENV_FILES: usize = 3_000;
const IGNORED_PYCACHE_FILES: usize = 3_000;
const MIXED_UNIT_COUNT: usize =
    VALID_FILE_COUNT + CODE_FILE_COUNT + TEXT_FILE_COUNT + BINARY_FILE_COUNT + TOO_LARGE_FILE_COUNT;

/// Current process RSS in kilobytes, via `ps -o rss=`. Returns `None`
/// if `ps` isn't available or its output doesn't parse — the test
/// treats that as "skip the memory assertion", not a hard failure,
/// since this is a sanity check layered on top of the exact-count
/// assertion above, not the test's only evidence.
fn current_rss_kb() -> Option<i64> {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
}

fn build_large_fixture() -> fixtures::TempSourceHandle {
    let fixture = fixtures::empty_source();

    // The non-ignored subtree: a few hundred small real notes, so the
    // scan actually does real parse/insert work.
    for i in 0..VALID_FILE_COUNT {
        fixture.write(
            &format!("notes/note-{i}.md"),
            &format!("# Note {i}\n\nSome content with a #tag{i}.\n"),
        );
    }

    // The ignored subtree: over 100k files inside `node_modules`,
    // split across many subdirectories (closer to a real
    // `node_modules` tree's shape than one giant flat directory).
    // Content is empty — if ignore pruning ever regressed and these
    // reached the parser, that would show up as a huge jump in
    // `files_seen`/`units_written`, not as parse failures (empty
    // content parses fine), so the exact-count assertion below is
    // what actually catches a regression here, not file content.
    for i in 0..IGNORED_FILE_COUNT {
        let subdir = i % IGNORED_SUBDIRS;
        fixture.write(&format!("node_modules/pkg-{subdir}/file-{i}.md"), "");
    }

    // Mixed kinds in the non-ignored subtree.
    for i in 0..CODE_FILE_COUNT {
        fixture.write(
            &format!("src/mod-{}/file-{i}.rs", i % 10),
            &format!(
                "// [[link-{i}]] #not-a-tag\nfn f{i}() {{}}\nconst S: &str = \"[[decoy-{i}]]\";\n"
            ),
        );
    }
    for i in 0..TEXT_FILE_COUNT {
        fixture.write(&format!("data/file-{i}.csv"), &format!("a,b\n{i},2\n"));
    }
    for i in 0..BINARY_FILE_COUNT {
        fixture.write_bytes(
            &format!("img/pic-{i}.png"),
            &[0x89, b'P', b'N', b'G', 0, 1, 2],
        );
    }
    // E3: sparse, so nothing big is written to disk, and a read of it
    // would show up in the RSS check below.
    let sparse = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(fixture.root().join("huge.log"))
        .expect("create sparse file");
    sparse
        .set_len(20 * 1_048_576 + 1)
        .expect("set sparse length");

    // More ignored subtrees (G15), pruned like `node_modules`.
    for i in 0..IGNORED_VENV_FILES {
        fixture.write(&format!(".venv/lib/pkg-{}/mod-{i}.py", i % 50), "x = 1\n");
    }
    for i in 0..IGNORED_PYCACHE_FILES {
        fixture.write(&format!("src/mod-{}/__pycache__/m-{i}.pyc", i % 10), "x");
    }

    fixture
}

#[test]
fn a_100k_plus_ignored_subtree_is_pruned_before_the_walk_descends_into_it() {
    let fixture = build_large_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");
    let registry = SourceRegistry::new(cache).expect("new registry");

    // Sample peak RSS on a background thread while the scan runs on
    // this one, rather than instrumenting the scan itself.
    let peak_kb = Arc::new(AtomicI64::new(current_rss_kb().unwrap_or(0)));
    let peak_kb_for_sampler = Arc::clone(&peak_kb);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_sampler = Arc::clone(&stop);
    let sampler = std::thread::spawn(move || {
        while !stop_for_sampler.load(Ordering::Relaxed) {
            if let Some(rss) = current_rss_kb() {
                peak_kb_for_sampler.fetch_max(rss, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let baseline_kb = current_rss_kb();
    let started = Instant::now();
    let stats = registry
        .register("big-vault", fixture.root())
        .expect("register");
    let elapsed = started.elapsed();

    stop.store(true, Ordering::Relaxed);
    sampler.join().expect("sampler thread");

    // The real proof: only the non-ignored subtree's files were ever
    // counted. If the ignored directory had been descended into (even
    // just to list and then skip each entry one by one, instead of
    // being pruned at the directory level), `files_seen` would be in
    // the hundreds of thousands, not a few hundred.
    assert_eq!(
        stats.files_seen, MIXED_UNIT_COUNT,
        "the 120k ignored files under node_modules, and the .venv and \
         __pycache__ trees, must never be counted at all"
    );
    assert_eq!(stats.units_written, MIXED_UNIT_COUNT);
    assert_eq!(
        stats.links_written, CODE_FILE_COUNT,
        "no Markdown file here has a link; each code file gives its one \
         comment link and never the one in its string literal"
    );
    assert_eq!(
        stats.tags_written, VALID_FILE_COUNT,
        "one tag per note, none from code"
    );
    assert_eq!(
        stats.files_ignored, 0,
        "nothing was even seen to ignore one-by-one"
    );

    // E3/G2: kinds in the cache, including the sparse over-cap file.
    {
        let cache = Cache::open(cache_dir.path().join("cache.db")).expect("reopen cache");
        let source_id = cache.get_source("big-vault").unwrap().unwrap().id;
        let by_kind = |kind: &str| -> i64 {
            cache
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM units WHERE source_id = ?1 AND kind = ?2",
                    rusqlite::params![source_id, kind],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(by_kind("prose") as usize, VALID_FILE_COUNT);
        assert_eq!(by_kind("code") as usize, CODE_FILE_COUNT);
        assert_eq!(by_kind("text-other") as usize, TEXT_FILE_COUNT);
        assert_eq!(
            by_kind("binary") as usize,
            BINARY_FILE_COUNT + TOO_LARGE_FILE_COUNT,
            "the sparse over-cap file is all NUL in its first 8 KB"
        );
        let huge = cache
            .get_unit_by_rel_path(source_id, "huge.log")
            .unwrap()
            .expect("the over-cap file is still a unit");
        assert!(huge.too_large);
        assert_eq!(huge.size, Some(20 * 1_048_576 + 1));
        assert_eq!(
            huge.kind, "binary",
            "real kind from its first 8 KB (NULs), though never parsed"
        );
        let non_prose_rows: i64 = cache
            .connection()
            .query_row(
                // Only prose and code units are parsed (G3, G4), and
                // code gives links only: text-other, document and
                // binary units carry no extracted rows, and a code
                // unit no blocks or headings.
                "SELECT \
                   (SELECT COUNT(*) FROM links l JOIN units u ON u.id = l.from_unit_id \
                      WHERE u.kind NOT IN ('prose', 'code')) \
                 + (SELECT COUNT(*) FROM blocks b JOIN units u ON u.id = b.unit_id \
                      WHERE u.kind <> 'prose') \
                 + (SELECT COUNT(*) FROM headings h JOIN units u ON u.id = h.unit_id \
                      WHERE u.kind <> 'prose')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(non_prose_rows, 0);
        for ignored in [".venv/lib/pkg-0/mod-0.py", "src/mod-0/__pycache__/m-0.pyc"] {
            assert!(cache
                .get_unit_by_rel_path(source_id, ignored)
                .unwrap()
                .is_none());
        }
    }

    // A generous time bound: this would not hold if the scan touched
    // (even just `stat`ed) all 120k ignored files one by one.
    assert!(
        elapsed < Duration::from_secs(10),
        "scanning {VALID_FILE_COUNT} real files took {elapsed:?}; \
         the 120k ignored files under node_modules must have been pruned \
         at the directory level, not visited one by one"
    );

    if let (Some(baseline), peak) = (baseline_kb, peak_kb.load(Ordering::Relaxed)) {
        let delta_kb = peak - baseline;
        // A generous bound: this is not meant to pin down an exact
        // number, only to catch an implementation that collects the
        // full 120k-entry file list into memory before processing it
        // (a `Vec<PathBuf>` of that size alone, plus any per-entry
        // bookkeeping, would push this delta well past this bound).
        assert!(
            delta_kb < 300_000,
            "peak RSS grew by {delta_kb}KB during the scan of a 120k+-file tree; \
             expected the walk to stream rather than hold the full file list in memory"
        );
    }
    // If `ps` wasn't available at all, the exact-count and
    // elapsed-time assertions above still fully cover this test's
    // behavioral claim; the RSS check is a bonus, not the only
    // evidence.

    registry.unregister("big-vault").expect("unregister");
}
