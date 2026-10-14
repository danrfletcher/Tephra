//! Integration test: R16 — a chunked scan's post-commit sleep must
//! actually give another connection a chance to grab the write lock
//! between chunks, not just between the single final commit and
//! process exit.
//!
//! `walk_and_write` (`scan.rs`) commits every `SCAN_COMMIT_CHUNK_SIZE`
//! files and, before this fix, immediately reopened the next chunk's
//! `BEGIN IMMEDIATE` with no gap at all — a connection waiting on
//! SQLite's own busy-handler poll essentially never notices a lock
//! release that brief (the review's probe: a concurrent `register()`
//! issued partway through a long scan returned only once the scan
//! fully finished). This test proves the opposite directly: a racer
//! connection with `busy_timeout` set to zero (so each of its attempts
//! resolves instantly, either by acquiring the lock or failing with
//! `SQLITE_BUSY`, with none of `Cache`'s own 5-second busy_timeout
//! blurring the measurement) polls in a tight loop for the entire
//! duration of a real, multi-chunk scan. At least one of its attempts
//! must land *during* the scan — strictly before the scan's own
//! completion — which is only possible if the per-chunk yield opens a
//! real window on every chunk boundary, not just once at the very
//! end.

mod fixtures;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tephra_cache::{Cache, SourceStatus};

/// Several multiples of `scan::SCAN_COMMIT_CHUNK_SIZE` (500), so the
/// scan below commits (and, with the fix, sleeps) at least 5 times
/// before finishing.
const FILE_COUNT: usize = 2_500;

#[test]
fn a_concurrent_writer_can_land_mid_scan_not_only_after_it_finishes() {
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

    let scan_done = Arc::new(AtomicBool::new(false));
    let scan_done_at: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    let scan_done_for_scan = Arc::clone(&scan_done);
    let scan_done_at_for_scan = Arc::clone(&scan_done_at);
    let root = fixture.root().to_path_buf();
    let ignore_for_scan = ignore_rules.clone();

    let scan_handle = std::thread::spawn(move || {
        let stats = tephrad::scan::scan_source(&cache, source_id, &root, &ignore_for_scan)
            .expect("scan_source");
        *scan_done_at_for_scan.lock().unwrap() = Some(Instant::now());
        scan_done_for_scan.store(true, Ordering::Release);
        stats
    });

    // A racer with `busy_timeout` = 0: every attempt resolves
    // instantly (either it gets the lock, or it gets SQLITE_BUSY
    // right away), so there is no internal busy-handler wait to
    // blur whether a given success really landed *during* the scan.
    let racer_conn = Connection::open(&cache_path).expect("open racer connection");
    racer_conn
        .busy_timeout(Duration::from_millis(0))
        .expect("set zero busy_timeout");

    let mut first_success_during_scan: Option<Instant> = None;
    let mut attempts = 0u64;
    let mut successes = 0u64;
    while !scan_done.load(Ordering::Acquire) {
        attempts += 1;
        if racer_conn.execute_batch("BEGIN IMMEDIATE; COMMIT;").is_ok() {
            successes += 1;
            if first_success_during_scan.is_none() {
                first_success_during_scan = Some(Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let stats = scan_handle.join().expect("scan thread panicked");
    assert_eq!(stats.units_written, FILE_COUNT);

    let finished_at = scan_done_at
        .lock()
        .unwrap()
        .expect("scan recorded a finish time");
    let success_at = first_success_during_scan.expect(
        "the racer must have successfully acquired the write lock at least once while the \
         scan loop was still polling — if this never happens, the per-chunk yield isn't \
         giving any other connection a real window",
    );
    assert!(
        success_at <= finished_at,
        "the racer's first success must be timestamped no later than the scan's own \
         completion, proving it interleaved rather than only succeeding once the scan was \
         already done"
    );
    assert!(
        successes > 0 && attempts > 0,
        "sanity: the racer loop must have actually run (attempts={attempts}, successes={successes})"
    );
}
