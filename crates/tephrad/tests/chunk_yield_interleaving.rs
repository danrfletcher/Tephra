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

    // R19: the small source's file is rewritten with new content —
    // including a tag absent from what it was registered with —
    // *before* the writer loop below even starts firing batch-apply
    // attempts, so proving a later success actually indexed this new
    // content (rather than merely reporting `Ok` on an unindexed
    // no-op) requires checking the resulting tag row, not just the
    // source's `status` field.
    small_fixture.write(
        "small.md",
        "# Small\n\nEdited while the big scan was running. #during\n",
    );

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

    let sources = small_cache
        .list_sources()
        .expect("list_sources")
        .into_iter()
        .find(|s| s.name == "small")
        .expect("small source listed");
    assert_eq!(sources.status, SourceStatus::Active);

    // R19: the edit itself must actually have landed in the small
    // source's cache rows, not merely reported `Ok` without writing
    // anything — check the real content change (the new `#during`
    // tag) rather than just the source's status flag.
    let unit = small_cache
        .get_unit_by_rel_path(small_source_id, "small.md")
        .expect("get_unit_by_rel_path")
        .expect("small.md must have a unit row after a successful batch-apply");
    let tag_count: i64 = small_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1 AND tag = ?2",
            rusqlite::params![unit.id, "during"],
            |r| r.get(0),
        )
        .expect("query tag count");
    assert_eq!(
        tag_count, 1,
        "small.md's post-edit #during tag must be indexed, proving the batch-apply that \
         succeeded mid-scan actually parsed and wrote the new content rather than a stale \
         or no-op row"
    );
}

/// R18/R19: an uncontended multi-chunk scan must not pay a fixed,
/// per-chunk-count-scaled tax for a hand-off nothing is waiting on.
///
/// Before the R18 fix, `walk_and_write` slept a fixed duration after
/// every `SCAN_COMMIT_CHUNK_SIZE`th file regardless of contention, so
/// an N-chunk scan always cost at least `N * sleep` beyond its own
/// real work. The first version of this test (R18) only bounded the
/// *total* wall time against a generous absolute constant (3s); that
/// left so much headroom above both the sleep-free and
/// sleep-reintroduced cases that a scratch-copy probe confirmed the
/// test kept passing even with the fixed per-chunk sleep put back
/// (R19 review finding).
///
/// This version instead self-calibrates on the same run: it first
/// times two scans small enough that `walk_and_write` never hits an
/// interior chunk boundary at all (under `SCAN_COMMIT_CHUNK_SIZE`
/// files, so zero chunk hand-offs occur beyond the single initial and
/// final commit every scan pays regardless), and uses the *marginal*
/// per-file cost between those two points — which cancels out each
/// run's fixed one-time setup cost (opening a cache, registering a
/// source, loading ignore rules) rather than being thrown off by it —
/// to project what an uncontended, many-chunk scan of the same shape
/// *should* cost if chunk hand-offs are genuinely free. The real
/// many-chunk scan is then compared against that projection as a
/// ratio, not an absolute bound: a ratio close to 1 means chunking
/// itself added (almost) nothing beyond the real per-file work; a
/// fixed per-chunk sleep inflates it sharply regardless of how fast
/// or slow the underlying machine is, since the sleep scales with
/// chunk count while the projection does not.
///
/// Measured on this suite's own dev machine: the current (sleep-free)
/// implementation's ratio is consistently under 1.5; a scratch copy
/// with `std::thread::sleep(Duration::from_millis(10))` reinstated
/// right after the per-chunk permit drop in `scan.rs::walk_and_write`
/// consistently measured a ratio above 3.5 — confirmed by rerunning
/// this exact test against that scratch copy. [`TAX_RATIO_BOUND`]
/// sits well clear of both.
#[test]
fn an_uncontended_multi_chunk_scan_pays_no_fixed_per_chunk_tax() {
    /// Below `scan::SCAN_COMMIT_CHUNK_SIZE` (500): `walk_and_write`
    /// commits exactly once (the final commit after the walk loop
    /// ends), with no interior chunk-boundary hand-off at all, so a
    /// scan at or under this size can never include any per-chunk
    /// tax — making it a clean, tax-free calibration point.
    const CALIBRATION_LOW: usize = 50;
    const CALIBRATION_HIGH: usize = 450;

    /// 40 * `SCAN_COMMIT_CHUNK_SIZE` (500) = 20,000 files, so this
    /// scan commits (and hands the gate off) roughly 40 times —
    /// enough chunk boundaries that a fixed per-chunk sleep would
    /// show up clearly against the calibration's projection.
    const MANY_CHUNK_COUNT: usize = 20_000;

    /// How far the many-chunk scan's actual time may exceed the
    /// tax-free projection before this test treats it as a
    /// reintroduced per-chunk tax rather than ordinary variance.
    /// Chosen with real margin on both sides of this suite's own
    /// measurements (sleep-free ratios under 1.5, sleep-reintroduced
    /// ratios above 3.5 — see the function doc comment).
    const TAX_RATIO_BOUND: f64 = 2.2;

    fn timed_scan(file_count: usize) -> Duration {
        let fixture = fixtures::empty_source();
        for i in 0..file_count {
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

        assert_eq!(stats.units_written, file_count);
        elapsed
    }

    let low = timed_scan(CALIBRATION_LOW).as_secs_f64();
    let high = timed_scan(CALIBRATION_HIGH).as_secs_f64();
    // Marginal tax-free per-file cost, derived from two points that
    // both ran with zero interior chunk boundaries — any fixed setup
    // cost common to both runs cancels out of this slope.
    let marginal_per_file = (high - low) / (CALIBRATION_HIGH as f64 - CALIBRATION_LOW as f64);
    let projected = high + marginal_per_file * (MANY_CHUNK_COUNT as f64 - CALIBRATION_HIGH as f64);

    let actual = timed_scan(MANY_CHUNK_COUNT).as_secs_f64();
    let ratio = actual / projected;

    assert!(
        ratio < TAX_RATIO_BOUND,
        "a {MANY_CHUNK_COUNT}-file uncontended scan (~40 chunks) took {actual:.3}s against a \
         tax-free projection of {projected:.3}s (ratio {ratio:.2}, bound {TAX_RATIO_BOUND}); a \
         fixed per-chunk sleep reintroduced into walk_and_write would show up here as a ratio \
         scaled by chunk count, not as ordinary machine-speed variance"
    );
}
