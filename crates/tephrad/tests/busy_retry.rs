//! Integration test: R11 — a debounced batch for one source must not
//! be permanently dropped just because another connection to the
//! same WAL-mode database file holds the write lock for longer than
//! [`tephra_cache::Cache`]'s 5-second `busy_timeout` pragma.
//!
//! Before this fix, `apply_change_batch` logged a warning and
//! discarded the whole batch on its very first `SQLITE_BUSY` (see the
//! review's probe: registering a small source, then a 60k-file large
//! source whose single giant scan transaction held the write lock for
//! the whole walk, produced `WARN failed to apply change batch
//! source=small error=database is locked`, and the small source's
//! edit stayed missing from the cache until a manual rescan). The fix
//! is two-layered: R11's chunked scan commits mean a *normal* scan no
//! longer holds the write lock anywhere near 5 seconds in the first
//! place, and `apply_change_batch`'s own retry loop on top of that
//! means even a genuinely sustained lock (slow disk, or — as here —
//! a connection that deliberately never lets go) is retried rather
//! than dropped.
//!
//! This test exercises the second, defensive layer directly: it holds
//! a real write lock on the same database file for longer than the
//! 5-second `busy_timeout`, so `apply_change_batch`'s underlying
//! SQLite call is guaranteed to see a genuine `SQLITE_BUSY` (not just
//! wait it out silently inside SQLite's own busy handler), and
//! asserts the batch still eventually succeeds rather than being
//! logged-and-discarded.

mod fixtures;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tephra_cache::{Cache, SourceStatus};

#[test]
fn a_batch_blocked_past_the_busy_timeout_is_retried_not_dropped() {
    let fixture = fixtures::vault_fixture();
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

    // The change `apply_change_batch` below needs to pick up once it
    // finally gets the write lock.
    fixture.write(
        "one.md",
        "# One\n\nEdited with a #during tag while the write lock was held elsewhere.\n",
    );

    // A second, independent connection to the exact same database
    // file acquires (and holds) the write lock for longer than
    // `Cache`'s 5-second `busy_timeout` — `BEGIN IMMEDIATE` alone
    // acquires it up front, with no write statement needed, even
    // under WAL mode.
    let blocker = Connection::open(&cache_path).expect("open blocker connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("blocker acquires the write lock");

    let hold_for = Duration::from_millis(5_500);
    let release_handle = std::thread::spawn(move || {
        std::thread::sleep(hold_for);
        blocker
            .execute_batch("COMMIT;")
            .expect("blocker releases the write lock");
    });

    let mut changed = HashSet::new();
    changed.insert(fixture.root().join("one.md"));

    let started = Instant::now();
    let outcome = tephrad::scan::apply_change_batch(
        &cache,
        source_id,
        fixture.root(),
        &changed,
        &ignore_rules,
    );
    let elapsed = started.elapsed();

    release_handle.join().expect("blocker thread panicked");

    let outcome = outcome.expect(
        "apply_change_batch must retry past a busy-timeout SQLITE_BUSY and eventually \
         succeed, not log-and-drop the batch the way the pre-fix code did",
    );
    assert_eq!(
        outcome.changed_paths.len(),
        1,
        "one.md's edit must be reported once the retried batch succeeds"
    );
    assert!(
        elapsed >= hold_for,
        "the batch must have actually waited out the blocker, not merely gotten lucky \
         before it ever contended for the lock (elapsed {elapsed:?} vs hold_for {hold_for:?})"
    );

    let unit = cache
        .get_unit_by_rel_path(source_id, "one.md")
        .expect("get_unit_by_rel_path")
        .expect("one.md's row must still exist");
    let tag_count: i64 = cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1 AND tag = ?2",
            rusqlite::params![unit.id, "during"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tag_count, 1,
        "the retried batch must have written the edited content, not silently discarded it"
    );
}
