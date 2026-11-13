//! Integration test: R15 — a source's own `rescan()` must survive a
//! concurrent edit landing on a file its walk hasn't reached yet.
//!
//! `rescan_source` deletes a source's rows and commits that delete in
//! the walk's very first chunk (R11's chunked-commit strategy, see
//! `scan.rs`'s module doc comment). That delete+commit happening
//! before the rest of the walk is exactly the window the review's
//! probe exploited: the live watcher's own debounced batch, running
//! on a totally independent connection, can land on a file in a
//! *later* chunk — one the rescan's walk hasn't visited yet — between
//! one chunk's commit and the next chunk's `BEGIN IMMEDIATE`. Before
//! the fix, the walk's plain insert for that file then hit
//! `UNIQUE(source_id, rel_path)` and aborted the rest of the rescan
//! outright, leaving the source `stale` with most of its rows gone
//! (the review's repro: 1520 of 250000 rows left after a 0.58s
//! failure).
//!
//! Rather than trying to land a single watcher edit on one
//! specific, not-yet-walked file (which would require knowing
//! `walkdir`'s real traversal order — platform- and
//! filesystem-dependent, and not something this crate's tests rely on
//! anywhere else), this test races a background thread that
//! continuously applies real watcher-shaped batches
//! (`tephrad::scan::apply_change_batch`, the exact function the live
//! watcher calls) against random files throughout the rescan's
//! entire duration, on a separate connection to the same database
//! file. With enough files split across several of `scan.rs`'s
//! commit chunks, and enough racing attempts spread across the whole
//! rescan, the race is effectively guaranteed to land on a
//! not-yet-walked file at least once. After the fix (`write_unit`
//! always deletes any existing row before inserting — see its doc
//! comment), this is fully deterministic: every apply lands as an
//! upsert, and the rescan always completes with every file's row
//! intact, regardless of how the race actually interleaves.

mod fixtures;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tephra_cache::{Cache, SourceStatus};

const FILE_COUNT: usize = 1_500; // 3 commit chunks at SCAN_COMMIT_CHUNK_SIZE = 500

#[test]
fn rescan_survives_a_concurrent_edit_on_a_not_yet_walked_file() {
    let fixture = fixtures::empty_source();
    for i in 0..FILE_COUNT {
        fixture.write(
            &format!("notes/note-{i:05}.md"),
            &format!("# Note {i}\n\nOriginal content.\n"),
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
    let initial_stats =
        tephrad::scan::scan_source(&cache, source_id, fixture.root(), &ignore_rules)
            .expect("initial scan");
    assert_eq!(initial_stats.units_written, FILE_COUNT);
    drop(cache);

    let done = Arc::new(AtomicBool::new(false));
    let done_for_racer = Arc::clone(&done);
    let root_for_racer = fixture.root().to_path_buf();
    let ignore_for_racer = ignore_rules.clone();
    let cache_path_for_racer = cache_path.clone();

    // The racer: a completely independent connection applying real
    // watcher-shaped batches (one file at a time, like a real
    // debounced edit) against random files while the rescan below is
    // in progress — exactly what a live watcher's own debounce task
    // would do concurrently with this source's own forced rescan.
    let racer = std::thread::spawn(move || {
        let racer_cache = Cache::open(&cache_path_for_racer).expect("racer open cache");
        let mut applied = 0usize;
        let mut rng_state: u64 = 0x1234_5678_9abc_def0;
        while !done_for_racer.load(Ordering::Relaxed) {
            // A tiny xorshift PRNG — no extra dependency needed for
            // "pick a pseudo-random file index".
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            let idx = (rng_state as usize) % FILE_COUNT;

            let mut changed = HashSet::new();
            changed.insert(root_for_racer.join(format!("notes/note-{idx:05}.md")));
            // A real `apply_change_batch` call, on its own connection,
            // retried internally on SQLITE_BUSY (R11) exactly the way
            // the live watcher's batch-apply path is.
            if tephrad::scan::apply_change_batch(
                &racer_cache,
                source_id,
                &root_for_racer,
                &changed,
                &ignore_for_racer,
            )
            .is_ok()
            {
                applied += 1;
            }
        }
        applied
    });

    let rescan_cache = Cache::open(&cache_path).expect("rescan cache");
    let rescan_result =
        tephrad::scan::rescan_source(&rescan_cache, source_id, fixture.root(), &ignore_rules);

    done.store(true, Ordering::Relaxed);
    let applied = racer.join().expect("racer thread panicked");

    let stats = rescan_result.expect(
        "rescan must survive a concurrent watcher batch landing on a not-yet-walked file, \
         not abort with a UNIQUE constraint failure",
    );
    assert_eq!(
        stats.units_written, FILE_COUNT,
        "every file must still be walked and written, even one the racer touched mid-rescan"
    );
    assert!(
        applied > 0,
        "the racer must have actually landed at least one successful concurrent apply \
         during the rescan for this test to mean anything"
    );

    let final_count: i64 = rescan_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM units WHERE source_id = ?1",
            [source_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        final_count, FILE_COUNT as i64,
        "no file's row must be lost — the UNIQUE-constraint failure mode left only a \
         fraction of rows behind"
    );
}

/// A `^^` (all_blocks) query made while a source's own `rescan()` is
/// running (the rescan deletes the source's rows and rewrites them over
/// several commit chunks): the query must never fail, and what it sees
/// must be a consistent snapshot. Every hit is a whole file's own block
/// (the row's text carries the same number as its path, so a row left
/// over from another file or half written would show), no unit comes
/// twice, and once the rescan is over every file is found.
///
/// The searcher is a real `search_all_blocks` call on its own read-only
/// connection to the same database file, the function behind
/// `unit.search` with `scope: "all_blocks"`.
#[test]
fn a_block_search_during_a_rescan_never_errors_and_sees_only_whole_files() {
    let fixture = fixtures::empty_source();
    for i in 0..FILE_COUNT {
        fixture.write(
            &format!("notes/note-{i:05}.md"),
            &format!("# Note {i}\n\nrescanmarker paragraph {i:05}\n"),
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
    tephrad::scan::scan_source(&cache, source_id, fixture.root(), &ignore_rules)
        .expect("initial scan");
    drop(cache);

    let done = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let searcher = {
        let (done, ready, cache_path) = (Arc::clone(&done), Arc::clone(&ready), cache_path.clone());
        std::thread::spawn(move || {
            let mut during_rescan = 0usize;
            let mut most_seen = 0usize;
            loop {
                let rescan_was_over = done.load(Ordering::Relaxed);
                let hits = tephrad::search::all_blocks::search_all_blocks(
                    &cache_path,
                    "rescanmarker",
                    FILE_COUNT + 10,
                )
                .unwrap_or_else(|e| panic!("a search during the rescan failed: {e:?}"));
                ready.store(true, Ordering::Relaxed);

                let mut seen = HashSet::new();
                for hit in &hits {
                    assert!(seen.insert(hit.unit.clone()), "{} came twice", hit.unit);
                    let number = hit
                        .unit
                        .trim_start_matches("@notes:notes/note-")
                        .trim_end_matches(".md");
                    assert!(
                        hit.snippet
                            .contains(&format!("rescanmarker paragraph {number}")),
                        "{} came with another file's text: {}",
                        hit.unit,
                        hit.snippet
                    );
                }
                most_seen = most_seen.max(hits.len());
                if rescan_was_over {
                    return (during_rescan, most_seen);
                }
                during_rescan += 1;
            }
        })
    };

    // Start the rescan only once the searcher has made a query, so
    // that queries really do run alongside it.
    while !ready.load(Ordering::Relaxed) {
        std::thread::yield_now();
    }
    let rescan_cache = Cache::open(&cache_path).expect("rescan cache");
    let rescan_result =
        tephrad::scan::rescan_source(&rescan_cache, source_id, fixture.root(), &ignore_rules);
    done.store(true, Ordering::Relaxed);
    let (during_rescan, most_seen) = searcher.join().expect("searcher thread panicked");

    let stats = rescan_result.expect("the rescan must finish while it is being searched");
    assert_eq!(stats.units_written, FILE_COUNT);
    assert!(
        during_rescan >= 1,
        "at least one query must have run alongside the rescan"
    );
    assert!(most_seen <= FILE_COUNT);

    // The rescan is over: every file is found, once.
    let hits = tephrad::search::all_blocks::search_all_blocks(
        &cache_path,
        "rescanmarker",
        FILE_COUNT + 10,
    )
    .expect("search after the rescan");
    assert_eq!(hits.len(), FILE_COUNT);
    let units: HashSet<_> = hits.iter().map(|h| h.unit.as_str()).collect();
    assert_eq!(units.len(), FILE_COUNT);
}
