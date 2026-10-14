//! Integration test: R16 — `SourceRegistry::register`'s own insert
//! (and the same `retry_cache_on_busy` helper's other call sites:
//! `rescan`'s status transitions, `unregister`'s delete, and
//! `on_root_removed`'s offline transition — see
//! `registry::tests::retry_cache_on_busy_*` for fast, deterministic
//! coverage of the helper's own retry/give-up logic in isolation)
//! must retry past a `SQLITE_BUSY` rather than fail outright.
//!
//! Before this fix, none of `SourceRegistry`'s own writes against
//! `self.cache` retried at all: `scan.rs`'s chunked-commit walk
//! (R11) already retries *its own* batch inserts, but a concurrent
//! `register()` racing against a long scan had no such protection
//! (the review's probe: a 250k-file scan ran long enough that a
//! concurrent `register("small2")` failed outright with "database is
//! locked" after 7.8s). This test exercises that specific code path
//! directly against a real, sustained lock: a second, independent
//! connection holds the database's write lock continuously for 20
//! seconds. Empirically (measured against this crate's actual
//! `Cache::open`, which sets a 5-second `busy_timeout` pragma), a
//! single unretried write against this database gives up with a
//! genuine `SQLITE_BUSY` well before the 20-second mark — SQLite's
//! own busy-handler backoff schedule does not, in practice, enforce
//! a hard cutoff at exactly the pragma's nominal value, but it does
//! give up with room to spare before 20 seconds. `register()` must
//! therefore still be waiting (and retrying) when the blocker
//! releases the lock at the 20-second mark, and succeed shortly
//! after — rather than ever having surfaced "database is locked" to
//! the caller.
//!
//! Slow by necessity (not a fixed-timeout flake): this directly
//! exercises wall-clock contention against SQLite's real busy
//! handler, which is the only way to produce a genuine, sustained
//! `SQLITE_BUSY` for `register()` to retry past.

mod fixtures;

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tephra_cache::{Cache, SourceStatus};
use tephrad::registry::SourceRegistry;

#[test]
fn register_survives_a_write_lock_held_past_the_busy_timeout() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");

    // Open (and close) the cache once first, so the database file and
    // its schema exist before the blocker connection below opens it.
    drop(Cache::open(&cache_path).expect("create cache"));

    // A second, independent connection to the exact same database
    // file acquires (and holds) the write lock continuously for the
    // whole `hold_for` window below.
    let blocker = Connection::open(&cache_path).expect("open blocker connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("blocker acquires the write lock");

    let hold_for = Duration::from_millis(20_000);
    let release_handle = std::thread::spawn(move || {
        std::thread::sleep(hold_for);
        blocker
            .execute_batch("COMMIT;")
            .expect("blocker releases the write lock");
    });

    let cache = Cache::open(&cache_path).expect("open cache under test");
    let registry = SourceRegistry::new(cache).expect("new registry");

    let started = Instant::now();
    let outcome = registry.register("notes", fixture.root());
    let elapsed = started.elapsed();

    release_handle.join().expect("blocker thread panicked");

    outcome.expect(
        "register() must retry its insert past a sustained SQLITE_BUSY and eventually \
         succeed, not surface \"database is locked\" to the caller",
    );
    assert!(
        elapsed >= hold_for,
        "register() must have actually waited out the blocker's full hold, not merely \
         gotten lucky before it ever contended for the lock \
         (elapsed {elapsed:?} vs hold_for {hold_for:?})"
    );

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name, "notes");
    assert_eq!(sources[0].status, SourceStatus::Active);

    registry.unregister("notes").expect("unregister");
}
