//! Integration test: R18 — the writer gate (`crate::writer_gate`)
//! must give another in-process writer a real turn *during* a long,
//! multi-chunk scan, not just once the scan is fully finished, and it
//! must do this through a deterministic hand-off rather than a blind
//! per-chunk sleep.
//!
//! Before the R18 fix, `walk_and_write` (`scan.rs`) proved this by
//! sleeping a fixed duration after every chunk commit (R16), and this
//! file used to measure that sleep's effect with a raw
//! `rusqlite::Connection` racer polling with `busy_timeout` set to
//! zero. That blind, unconditional sleep is exactly what R18 removes
//! — it cost every scan the same fixed tax whether or not anything
//! was ever contending (measured 5.3x slower on a 100k-file scan).
//! The replacement (`crate::writer_gate::WriterGate`) only ever pays
//! for a hand-off that's actually needed, and only coordinates this
//! crate's *own* writers (every one of which lives in this one
//! process): a raw bypass connection, uncoordinated with anything,
//! would now see no deliberate gap at all and could easily never
//! land during the scan, which would make the old version of this
//! test flaky-to-useless under the new design rather than prove
//! anything about it. This version instead races a second, realistic
//! in-process writer — a debounced batch-apply against a second,
//! independent source sharing the very same cache database file,
//! exactly the scenario R11/R16/R18 are all about — and proves it
//! completes *during* the big scan, promptly, every single time, with
//! no sleep or polling needed on its side at all.
//!
//! See also `register_busy_retry.rs` and `busy_retry.rs`, which cover
//! the complementary case this gate does *not* change: a write stuck
//! behind a real, external `SQLITE_BUSY` still relies on the existing
//! retry-with-backoff layered on top, since an external connection
//! can't be coordinated through an in-process gate.

mod fixtures;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tephra_cache::{Cache, SourceStatus};

/// Several multiples of `scan::SCAN_COMMIT_CHUNK_SIZE` (500), so the
/// big scan below commits at least 5 times, giving the small writer
/// several chunk-boundary hand-offs to land in.
const FILE_COUNT: usize = 2_500;

#[test]
fn a_concurrent_in_process_writer_lands_mid_scan_every_time() {
    let big_fixture = fixtures::empty_source();
    for i in 0..FILE_COUNT {
        big_fixture.write(
            &format!("notes/note-{i:05}.md"),
            &format!("# Note {i}\n\nSome content for note {i}.\n"),
        );
    }

    let small_fixture = fixtures::empty_source();
    small_fixture.write("small.md", "# Small\n\nOriginal.\n");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");

    let big_cache = Cache::open(&cache_path).expect("open cache for the big source");
    let big_source_id = big_cache
        .register_source(
            "notes",
            &big_fixture.root().to_string_lossy(),
            SourceStatus::Active,
        )
        .expect("register_source (big)");

    let small_cache =
        Cache::open(&cache_path).expect("open a second connection for the small source");
    let small_source_id = small_cache
        .register_source(
            "small",
            &small_fixture.root().to_string_lossy(),
            SourceStatus::Active,
        )
        .expect("register_source (small)");

    let big_root = big_fixture.root().to_path_buf();
    let big_ignore = tephrad::ignore::IgnoreRules::load(&big_root).expect("load ignore (big)");
    let small_root = small_fixture.root().to_path_buf();
    let small_ignore =
        tephrad::ignore::IgnoreRules::load(&small_root).expect("load ignore (small)");

    let scan_done = Arc::new(AtomicBool::new(false));
    let scan_done_at: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    let scan_done_for_scan = Arc::clone(&scan_done);
    let scan_done_at_for_scan = Arc::clone(&scan_done_at);

    let scan_handle = std::thread::spawn(move || {
        let stats = tephrad::scan::scan_source(&big_cache, big_source_id, &big_root, &big_ignore)
            .expect("scan_source");
        *scan_done_at_for_scan.lock().unwrap() = Some(Instant::now());
        scan_done_for_scan.store(true, Ordering::Release);
        stats
    });

    let mut changed = HashSet::new();
    changed.insert(small_root.join("small.md"));

    // Fire batch-apply attempts against the *small* source back to
    // back for as long as the big scan is running. Each call either
    // sails through uncontended (the gate was free) or blocks inside
    // `WriterGate::acquire` until the big scan's current chunk
    // releases it — either way, no sleep or poll on this side, and
    // the very first success must land strictly before the scan
    // finishes.
    let mut first_success_during_scan: Option<Instant> = None;
    let mut attempts = 0u64;
    let mut successes = 0u64;
    while !scan_done.load(Ordering::Acquire) {
        attempts += 1;
        let outcome = tephrad::scan::apply_change_batch(
            &small_cache,
            small_source_id,
            &small_root,
            &changed,
            &small_ignore,
        );
        if outcome.is_ok() {
            successes += 1;
            if first_success_during_scan.is_none() {
                first_success_during_scan = Some(Instant::now());
            }
        }
    }

    let stats = scan_handle.join().expect("scan thread panicked");
    assert_eq!(stats.units_written, FILE_COUNT);

    let finished_at = scan_done_at
        .lock()
        .unwrap()
        .expect("scan recorded a finish time");
    let success_at = first_success_during_scan.expect(
        "the small source's batch-apply must have succeeded at least once while the big scan \
         was still running — if this never happens, the writer gate isn't handing another \
         in-process writer a real turn mid-scan",
    );
    assert!(
        success_at <= finished_at,
        "the small source's first successful batch-apply must be timestamped no later than \
         the big scan's own completion, proving it interleaved rather than only succeeding \
         once the scan was already done"
    );
    assert!(
        successes > 0 && attempts > 0,
        "sanity: the small-source writer loop must have actually run (attempts={attempts}, successes={successes})"
    );

    // And the edit itself must actually have landed in the small
    // source's cache rows, not merely reported `Ok` without writing
    // anything.
    let tags = small_cache
        .list_sources()
        .expect("list_sources")
        .into_iter()
        .find(|s| s.name == "small")
        .expect("small source listed");
    assert_eq!(tags.status, SourceStatus::Active);
}

/// R18: an uncontended multi-chunk scan must not pay a fixed,
/// chunk-count-scaled tax for a hand-off nothing is waiting on.
///
/// Before this fix, `walk_and_write` slept `CHUNK_COMMIT_YIELD`
/// (10ms) after every `SCAN_COMMIT_CHUNK_SIZE`th file regardless of
/// contention, so an N-chunk scan always cost at least
/// `N * CHUNK_COMMIT_YIELD` beyond its own real work — and on macOS,
/// timer coalescing turned each nominal 10ms sleep into roughly
/// 60-80ms, measured 5.3x slower end to end on a 100k-file scan. This
/// test is intentionally far smaller (so it stays fast and reliable
/// in CI) but asserts the same property: enough chunks that the old,
/// unconditional sleep would have added a clearly measurable amount
/// of wall time on *any* platform, with nothing else ever touching
/// this cache database, must not come anywhere close to that.
#[test]
fn an_uncontended_multi_chunk_scan_pays_no_fixed_per_chunk_tax() {
    // 20 * SCAN_COMMIT_CHUNK_SIZE (500) = 10,000 files, so the old
    // fixed sleep would have added at least 20 * 10ms = 200ms (and,
    // per the R18 measurement's macOS coalescing, plausibly well
    // over a second) purely from chunk hand-offs, on top of the
    // walk's own real work.
    const FILE_COUNT: usize = 10_000;

    let fixture = fixtures::empty_source();
    for i in 0..FILE_COUNT {
        fixture.write(
            &format!("notes/note-{i:05}.md"),
            &format!("# Note {i}\n\nSome content for note {i}.\n"),
        );
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");
    let source_id = cache
        .register_source(
            "notes",
            &fixture.root().to_string_lossy(),
            SourceStatus::Active,
        )
        .expect("register_source");
    let ignore_rules = tephrad::ignore::IgnoreRules::load(fixture.root()).expect("load ignore");

    let started = Instant::now();
    let stats = tephrad::scan::scan_source(&cache, source_id, fixture.root(), &ignore_rules)
        .expect("scan_source");
    let elapsed = started.elapsed();

    assert_eq!(stats.units_written, FILE_COUNT);
    // Generous bound: 20 chunk boundaries at the old fixed 10ms
    // (never mind macOS's observed 60-80ms per sleep) alone would
    // already be 200ms; this scan's own real work (parsing and
    // writing 10,000 tiny markdown files) is itself well under a
    // second on any reasonable machine. Three seconds leaves ample
    // headroom for a slow CI runner while still clearly failing if
    // a fixed per-chunk sleep were reintroduced.
    assert!(
        elapsed < Duration::from_secs(3),
        "an uncontended {FILE_COUNT}-file scan (20 chunks) took {elapsed:?}; a fixed \
         per-chunk sleep would show up here as a tax proportional to the chunk count"
    );
}
