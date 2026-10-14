//! A process-wide, fair (FIFO) hand-off for the cache database's
//! single SQLite writer lock (R18 review finding).
//!
//! Before this fix, [`crate::scan`]'s chunked scan commits (R11/R16)
//! gave another connection a chance to grab the write lock purely by
//! sleeping a fixed duration after every chunk commit, whether or not
//! anything was actually waiting — paying that cost on every chunk
//! of every scan, even a completely uncontended one (R18's
//! measurement: a 100k-file scan ran 5.3x slower, 15.45s vs 2.86s,
//! purely from that unconditional sleep, worsened by macOS timer
//! coalescing turning each nominal 10ms sleep into 60-80ms). Every
//! writer against the cache lives inside this one `tephrad` process
//! (a full scan, a forced rescan, a debounced batch-apply, and the
//! registry's own quick writes — register/unregister/rescan status
//! transitions), so instead of blindly sleeping and hoping some other
//! connection's own busy-handler poll happens to land in the gap,
//! every one of those writers now acquires this gate immediately
//! before issuing its `BEGIN IMMEDIATE`, and releases it immediately
//! after committing. Acquiring it when nobody else is waiting is just
//! one ticket increment plus one mutex lock/unlock — no sleep, no
//! polling — and a waiter is woken the instant the current holder
//! releases it, deterministically, in the order it started waiting
//! (FIFO), rather than whichever side's own retry timer happens to
//! fire first.
//!
//! This does not replace `scan`'s own `SQLITE_BUSY` retry logic
//! ([`crate::scan::retry_on_busy`]) or the registry's
//! (`retry_cache_on_busy`): a connection truly external to this
//! process (or, in a handful of tests, a raw `rusqlite::Connection`
//! deliberately bypassing this gate to simulate exactly that) can
//! still hold the real SQLite write lock for longer than the
//! `busy_timeout` pragma, and that failure mode is unaffected by —
//! and still needs — the existing retry-with-backoff layered on top.
//! What this gate removes is the *self-inflicted* contention between
//! this process's own writers, and the fixed cost that was paid to
//! guard against it regardless of whether it was actually happening.
//!
//! Keyed per cache-database path, rather than one single global
//! instance, so unrelated `Cache`s opened against different database
//! files — as almost every test in this crate's `tests/` directory
//! does, each with its own temp directory — never contend with each
//! other's writers or serialize unrelated tests against one another.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// A FIFO-fair mutex guarding one cache database file's writers.
pub struct WriterGate {
    state: Mutex<GateState>,
    cv: Condvar,
}

struct GateState {
    next_ticket: u64,
    serving: u64,
    held: bool,
}

impl WriterGate {
    /// `pub(crate)` rather than fully private: besides
    /// [`for_path`]'s own internal use, a handful of this crate's
    /// unit tests (`registry.rs`'s `retry_cache_on_busy` tests) need
    /// a throwaway gate that isn't registered under any path, so they
    /// stay fast and deterministic without any real filesystem path
    /// or shared global state.
    pub(crate) fn new() -> Self {
        WriterGate {
            state: Mutex::new(GateState {
                next_ticket: 0,
                serving: 0,
                held: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Blocks until this caller is next in FIFO order and the gate is
    /// currently free, then marks it held and returns a guard that
    /// releases it (waking the next waiter, if any) on drop.
    ///
    /// Uncontended (no other ticket is currently waiting and nobody
    /// holds the gate), this never touches the condition variable at
    /// all: it's one ticket read-and-increment plus one mutex
    /// lock/unlock, independent of how many chunks a long scan ends
    /// up committing.
    pub fn acquire(&self) -> WriterGateGuard<'_> {
        let mut state = self.state.lock().unwrap();
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        while state.held || state.serving != ticket {
            state = self.cv.wait(state).unwrap();
        }
        state.held = true;
        WriterGateGuard { gate: self }
    }
}

/// Held for exactly as long as this writer's own transaction is open
/// against the cache database — acquired right before `BEGIN
/// IMMEDIATE`, dropped right after `COMMIT` (or on an early error
/// return, which simply hands the gate to the next waiter without
/// that writer ever having made it as far as a commit).
pub struct WriterGateGuard<'a> {
    gate: &'a WriterGate,
}

impl Drop for WriterGateGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        state.held = false;
        state.serving += 1;
        drop(state);
        // `notify_all` rather than `notify_one`: every waiter rechecks
        // its own ticket against `serving` before proceeding, so only
        // the one whose ticket now matches actually wakes up and goes
        // — the rest immediately re-wait. `notify_all` keeps the gate
        // itself simple and correct without needing to track which
        // specific thread is waiting on which ticket.
        self.gate.cv.notify_all();
    }
}

fn gates() -> &'static Mutex<HashMap<PathBuf, Arc<WriterGate>>> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Arc<WriterGate>>>> = OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared [`WriterGate`] for the cache database at `path`: every
/// caller passing an equivalent path (symlinks and relative spellings
/// included, since the key is canonicalized) gets back the same
/// `Arc`, so every writer against one database file — a scan's
/// chunked commits, a debounced batch, and the registry's own quick
/// writes — hands off through the same gate, while two unrelated
/// database files never contend with each other.
pub fn for_path(path: &Path) -> Arc<WriterGate> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut map = gates().lock().unwrap();
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(WriterGate::new())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn acquiring_an_uncontended_gate_is_essentially_free() {
        let gate = WriterGate::new();
        let started = Instant::now();
        for _ in 0..10_000 {
            let permit = gate.acquire();
            drop(permit);
        }
        // Generous bound: a tight loop of 10,000 uncontended
        // acquire/release cycles must not take anywhere near what
        // 10,000 chunk-boundary sleeps (even just 1ms each) would —
        // proving there is no sleep or poll on this path at all.
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "10,000 uncontended acquisitions took {:?}; acquiring an uncontended gate must not sleep",
            started.elapsed()
        );
    }

    #[test]
    fn two_waiters_are_served_in_the_order_they_started_waiting() {
        let gate = Arc::new(WriterGate::new());
        let order = Arc::new(Mutex::new(Vec::new()));

        // Hold the gate first, so both of the next two acquires below
        // are forced to queue up and wait.
        let first_permit = gate.acquire();

        // b starts queuing immediately; a deliberately waits a beat
        // before even calling `acquire`, so b is guaranteed to take
        // the earlier ticket and this test's expected order isn't a
        // race in itself.
        let gate_b = Arc::clone(&gate);
        let order_b = Arc::clone(&order);
        let b = std::thread::spawn(move || {
            let _permit = gate_b.acquire();
            order_b.lock().unwrap().push('b');
        });

        let gate_a = Arc::clone(&gate);
        let order_a = Arc::clone(&order);
        let a = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let _permit = gate_a.acquire();
            order_a.lock().unwrap().push('a');
        });

        // Give both threads time to actually be parked waiting on the
        // gate (b immediately, a after its own head-start delay)
        // before it's released.
        std::thread::sleep(Duration::from_millis(250));
        drop(first_permit);

        a.join().unwrap();
        b.join().unwrap();

        assert_eq!(
            *order.lock().unwrap(),
            vec!['b', 'a'],
            "b queued first (a deliberately slept before acquiring) and must be served first"
        );
    }

    #[test]
    fn a_waiter_is_woken_promptly_once_the_holder_releases() {
        let gate = Arc::new(WriterGate::new());
        let woken_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

        let held_permit = gate.acquire();
        let gate_for_waiter = Arc::clone(&gate);
        let woken_at_for_waiter = Arc::clone(&woken_at);
        let waiting = Arc::new(AtomicUsize::new(0));
        let waiting_for_waiter = Arc::clone(&waiting);
        let waiter = std::thread::spawn(move || {
            waiting_for_waiter.store(1, Ordering::SeqCst);
            let _permit = gate_for_waiter.acquire();
            *woken_at_for_waiter.lock().unwrap() = Some(Instant::now());
        });

        while waiting.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        // Give the waiter thread a moment to actually be parked in
        // `Condvar::wait` before releasing.
        std::thread::sleep(Duration::from_millis(50));
        let released_at = Instant::now();
        drop(held_permit);

        waiter.join().unwrap();
        let woken_at = woken_at
            .lock()
            .unwrap()
            .expect("waiter must have recorded a wake time");
        assert!(
            woken_at.duration_since(released_at) < Duration::from_millis(200),
            "a waiter must be woken promptly on release, not after some polling delay"
        );
    }

    #[test]
    fn different_paths_get_independent_gates() {
        let a = for_path(Path::new("/tmp/does-not-exist-a.db"));
        let b = for_path(Path::new("/tmp/does-not-exist-b.db"));
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different paths must not share a gate"
        );

        let a_again = for_path(Path::new("/tmp/does-not-exist-a.db"));
        assert!(
            Arc::ptr_eq(&a, &a_again),
            "the same path must return the same gate"
        );
    }
}
