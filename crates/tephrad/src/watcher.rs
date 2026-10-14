//! The per-source `notify`-backed watcher and its 250ms debounce
//! (G2), bounded by a maximum wait so a continuous stream of events
//! can't postpone indexing forever (R9).
//!
//! Two independently testable pieces:
//! - [`debounce_loop`]: a pure async coalescer over a channel of raw
//!   changed paths, with no `notify` dependency at all — this is what
//!   `#[cfg(test)] mod tests` below drives directly with a plain
//!   channel and `tokio::time`'s paused/advanceable virtual clock
//!   (`#[tokio::test(start_paused = true)]`), which is the "mocked
//!   clock" this crate's debounce test uses instead of a hand-rolled
//!   fake: a burst of sends with no real wall-clock time elapsing
//!   collapses into exactly one fired batch per 250ms of
//!   (virtual) quiescence, deterministically and without flaky
//!   real-time sleeps (the project has learned the hard way — see
//!   the fs.watch flake note — that fixed-timeout waits on real
//!   timers are not reliable in tests).
//! - [`start`]/[`spawn_debounce`]: wires a real
//!   `notify::RecommendedWatcher` on a source's root into
//!   [`debounce_loop`], filtering ignored paths (and the root path
//!   itself) out before they ever reach the debounce channel (R5),
//!   and is exercised only by the integration tests under `tests/`,
//!   which use a real filesystem and the real 250ms window.
//!
//! Split into two steps — [`start`] (subscribes the real OS-level
//! `notify` watcher and returns a [`WatcherTap`] holding its events
//! channel) and [`spawn_debounce`] (starts actually draining that
//! channel into debounced batches) — rather than one combined call,
//! so a caller can start observing filesystem events *before* running
//! a slow initial scan (R13 review finding): the channel between them
//! is unbounded, so any event that arrives in the gap between
//! [`start`] and [`spawn_debounce`] simply queues up rather than being
//! lost or requiring the watcher to already be fully wired up.
//! Without this split, an edit landing after the walk had already
//! passed a file, but before the watcher was started, was never
//! re-indexed until the next manual rescan.
//!
//! The ignore rules a running watcher filters against are shared via
//! `Arc<RwLock<IgnoreRules>>` (R12 review finding), not a plain
//! snapshot captured once at `start()` time: `source.rescan` writes a
//! freshly-reloaded `.tephraignore` into that same `RwLock` (see
//! `registry.rs`), so a path newly un-ignored (or newly ignored) by a
//! `.tephraignore` edit is filtered consistently by every subsequent
//! watcher event from that point on, not just by the rescan's own
//! one-off walk.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::ignore::IgnoreRules;

/// The hardcoded debounce window (G2): no per-source override in v1.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// The hardcoded maximum wait (R9): a pending batch fires at this
/// deadline (measured from its *first* still-pending event) even if
/// events keep arriving faster than [`DEBOUNCE_WINDOW`] apart and so
/// keep resetting the quiescence timer. Chosen as 4x the debounce
/// window — generous enough that the ordinary "one burst, one batch"
/// case (GP6/E5) is essentially never affected, but bounded enough
/// that a file written continuously (a dev build's `dist/` output, a
/// frequently-rewritten log) is still indexed at a predictable cadence
/// instead of being postponed indefinitely.
pub const DEBOUNCE_MAX_WAIT: Duration = Duration::from_millis(1000);

/// Coalesces a stream of changed-path events arriving on `rx` into
/// batches: a batch accumulates every path received, and fires (via
/// `on_fire`) once `window` elapses with no further event arriving
/// (quiescence-based debounce — a burst of events spaced closer
/// together than `window`, however long the burst runs, coalesces
/// into exactly one fired batch, per E5/GP6). The timer resets on
/// every new event; it only actually fires once the stream goes
/// quiet — *or* once `max_wait` has elapsed since the first event of
/// the current pending batch, whichever comes first (R9): without
/// this cap, a stream of events arriving faster than `window` apart
/// would reset the quiescence timer forever and the batch would never
/// fire.
///
/// Returns once `rx` is closed (the sender side dropped), after
/// firing one final batch if anything was still pending.
pub async fn debounce_loop<F>(
    mut rx: mpsc::UnboundedReceiver<PathBuf>,
    window: Duration,
    max_wait: Duration,
    mut on_fire: F,
) where
    F: FnMut(HashSet<PathBuf>),
{
    let mut pending: HashSet<PathBuf> = HashSet::new();

    loop {
        if pending.is_empty() {
            match rx.recv().await {
                Some(path) => {
                    pending.insert(path);
                }
                None => return,
            }
        }

        let first_pending_at = tokio::time::Instant::now();
        let quiesce = tokio::time::sleep(window);
        tokio::pin!(quiesce);
        let hard_deadline = tokio::time::sleep_until(first_pending_at + max_wait);
        tokio::pin!(hard_deadline);

        loop {
            tokio::select! {
                _ = &mut quiesce => {
                    on_fire(std::mem::take(&mut pending));
                    break;
                }
                _ = &mut hard_deadline => {
                    // R9: the bounded worst case — fire now rather
                    // than let a continuously-arriving stream keep
                    // resetting `quiesce` forever. This can split one
                    // logical burst that runs longer than `max_wait`
                    // into more than one batch; that trade-off is the
                    // point.
                    on_fire(std::mem::take(&mut pending));
                    break;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(path) => {
                            pending.insert(path);
                            quiesce.as_mut().reset(tokio::time::Instant::now() + window);
                        }
                        None => {
                            if !pending.is_empty() {
                                on_fire(std::mem::take(&mut pending));
                            }
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum WatchError {
    Notify(notify::Error),
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WatchError::Notify(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WatchError {}

impl From<notify::Error> for WatchError {
    fn from(e: notify::Error) -> Self {
        WatchError::Notify(e)
    }
}

/// The already-live half of a source's watcher (R13): the real
/// OS-level `notify::RecommendedWatcher` (subscribed to events from
/// the moment [`start`] returns) plus the still-undrained channel its
/// filtered events are queued into. Kept separate from
/// [`WatcherHandle`] so a caller can hold one of these across a slow
/// initial scan — nothing is lost while a `WatcherTap` sits idle,
/// since the channel is unbounded — and only build the actual
/// debounce task (via [`spawn_debounce`]) once the scan has committed.
pub struct WatcherTap {
    watcher: RecommendedWatcher,
    rx: mpsc::UnboundedReceiver<PathBuf>,
}

/// Subscribes a real recursive `notify` watch on `root`, filtering a
/// path that is the root itself, or that the current contents of
/// `ignore` report as ignored, out before it ever reaches the
/// returned tap's channel (R5): neither should ever appear in a
/// reported `source_changed` batch, and neither should be able to
/// reset the debounce timer indefinitely (e.g. continuous `.git`
/// activity in a repo source). This still leaves root-removal
/// detection intact in the realistic case: deleting or moving out a
/// populated root produces events for its non-ignored files too,
/// which keep arriving on the channel and so still trigger
/// [`spawn_debounce`]'s existence re-check.
///
/// `ignore` is read fresh on every single incoming event (R12 review
/// finding), not captured once as an owned snapshot: `source.rescan`
/// writes a freshly-reloaded `.tephraignore` into this very
/// `RwLock` (see `registry.rs`), so a path's ignored status always
/// reflects the most recently loaded rules, not whatever was true
/// when this watcher was first started.
pub fn start(root: PathBuf, ignore: Arc<RwLock<IgnoreRules>>) -> Result<WatcherTap, WatchError> {
    let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
    let root_for_watch = root.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        for path in event.paths {
            let rel = match path.strip_prefix(&root_for_watch) {
                Ok(r) => r,
                Err(_) => continue,
            };
            // See the doc comment above on the root-path exemption:
            // deliberately not filtered out here even though it never
            // ends up in a reported `source_changed` batch.
            let ignored = match ignore.read() {
                Ok(rules) => rules.is_ignored(rel),
                // A poisoned lock (a panic elsewhere while holding the
                // write half) must not silently swallow every future
                // event as "ignored" -- fail open so events keep
                // flowing rather than the watcher going silently inert.
                Err(poisoned) => poisoned.into_inner().is_ignored(rel),
            };
            if ignored {
                continue;
            }
            let _ = tx.send(path);
        }
    })?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    Ok(WatcherTap { watcher, rx })
}

/// A running source's watcher: the real OS-level `notify` watcher
/// (kept alive for as long as this handle exists) plus the tokio task
/// running its debounce loop. Dropping or calling [`WatcherHandle::stop`]
/// tears both down — the `notify` watcher stops receiving OS events as
/// soon as it's dropped, and the debounce task is aborted directly
/// rather than waiting for its channel to close.
pub struct WatcherHandle {
    _watcher: RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl WatcherHandle {
    /// Stops this source's watcher and debounce task. Independent of
    /// every other source's handle (GP4): stopping one never touches
    /// another's `notify::Watcher` instance or tokio task.
    pub fn stop(self) {
        self.task.abort();
        // `_watcher` is dropped here, unsubscribing it from the OS.
    }
}

/// Starts draining `tap`'s already-queued (and still-arriving) events
/// into [`debounce_loop`] on `runtime`, invoking `on_batch` once per
/// coalesced batch and `on_root_removed` if `root` itself is ever
/// found missing (the watched root was deleted or moved out from
/// under the watcher — handled here rather than crashing, consistent
/// with Linux inotify emitting no distinct "folder deleted" error of
/// its own: every incoming event opportunistically re-checks whether
/// `root` still resolves, and routes to `on_root_removed` instead of
/// the normal batch path the first time it doesn't).
///
/// Called separately from, and generally well after, [`start`] (R13):
/// an event that arrived on `tap`'s channel before this was called —
/// while a slow initial scan was still running, say — is still here,
/// queued, and is drained into this call's very first debounced batch
/// rather than having been silently missed.
pub fn spawn_debounce<B, R>(
    tap: WatcherTap,
    root: PathBuf,
    runtime: tokio::runtime::Handle,
    on_batch: B,
    on_root_removed: R,
) -> WatcherHandle
where
    B: Fn(HashSet<PathBuf>) + Send + Sync + 'static,
    R: Fn() + Send + Sync + 'static,
{
    let WatcherTap { watcher, rx } = tap;
    let task = runtime.spawn(async move {
        let root_check = root;
        let on_batch_checked = move |batch: HashSet<PathBuf>| {
            if !root_check.exists() {
                on_root_removed();
            } else {
                on_batch(batch);
            }
        };
        debounce_loop(rx, DEBOUNCE_WINDOW, DEBOUNCE_MAX_WAIT, on_batch_checked).await;
    });

    WatcherHandle {
        _watcher: watcher,
        task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn send_all(tx: &mpsc::UnboundedSender<PathBuf>, paths: &[&str]) {
        for p in paths {
            tx.send(PathBuf::from(p)).unwrap();
        }
    }

    /// The core debounce coalescing test (spec: "a burst of synthetic
    /// events collapses into one scan trigger per 250ms window"),
    /// using `tokio::time`'s paused virtual clock as the mocked clock
    /// and a plain `mpsc` channel as the mocked event source — no
    /// real `notify` watcher, no real filesystem, no real sleeping.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_events_coalesces_into_one_fired_batch() {
        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let fired: Arc<Mutex<Vec<HashSet<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_for_task = Arc::clone(&fired);

        let task = tokio::spawn(async move {
            debounce_loop(rx, DEBOUNCE_WINDOW, DEBOUNCE_MAX_WAIT, move |batch| {
                fired_for_task.lock().unwrap().push(batch);
            })
            .await;
        });

        // A burst of hundreds of synthetic events, as a `git
        // checkout` touching hundreds of files would produce (E5).
        // Sending on an unbounded channel with a paused clock takes
        // no virtual time, so every one of these arrives well inside
        // the same 250ms window.
        let paths: Vec<String> = (0..500).map(|i| format!("file-{i}.md")).collect();
        for p in &paths {
            tx.send(PathBuf::from(p)).unwrap();
        }

        // Give the task a chance to drain the channel before the
        // debounce window starts counting down from the *last* send.
        tokio::task::yield_now().await;
        // Advance virtual time past the debounce window: this must
        // fire exactly once, with every path from the burst present.
        tokio::time::advance(DEBOUNCE_WINDOW + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        drop(tx);
        task.await.unwrap();

        let batches = fired.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "a single burst inside the debounce window must fire exactly once, got {} batches",
            batches.len()
        );
        let fired_paths = &batches[0];
        assert_eq!(fired_paths.len(), paths.len());
        for p in &paths {
            assert!(fired_paths.contains(&PathBuf::from(p)));
        }
    }

    /// Two bursts separated by more than the debounce window fire as
    /// two independent batches, not coalesced into one.
    #[tokio::test(start_paused = true)]
    async fn two_bursts_separated_by_more_than_the_window_fire_separately() {
        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let fired: Arc<Mutex<Vec<HashSet<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_for_task = Arc::clone(&fired);

        let task = tokio::spawn(async move {
            debounce_loop(rx, DEBOUNCE_WINDOW, DEBOUNCE_MAX_WAIT, move |batch| {
                fired_for_task.lock().unwrap().push(batch);
            })
            .await;
        });

        send_all(&tx, &["a.md", "b.md"]);
        tokio::task::yield_now().await;
        tokio::time::advance(DEBOUNCE_WINDOW + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        send_all(&tx, &["c.md"]);
        tokio::task::yield_now().await;
        tokio::time::advance(DEBOUNCE_WINDOW + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        drop(tx);
        task.await.unwrap();

        let batches = fired.lock().unwrap();
        assert_eq!(
            batches.len(),
            2,
            "expected two separate batches, got {batches:?}"
        );
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 1);
    }

    /// An event arriving just before the window would have elapsed
    /// resets the timer, extending the same batch rather than firing
    /// early and starting a second one.
    #[tokio::test(start_paused = true)]
    async fn an_event_near_the_deadline_resets_the_window_instead_of_splitting_the_batch() {
        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let fired: Arc<Mutex<Vec<HashSet<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_for_task = Arc::clone(&fired);

        let task = tokio::spawn(async move {
            debounce_loop(rx, DEBOUNCE_WINDOW, DEBOUNCE_MAX_WAIT, move |batch| {
                fired_for_task.lock().unwrap().push(batch);
            })
            .await;
        });

        tx.send(PathBuf::from("a.md")).unwrap();
        tokio::task::yield_now().await;
        // Advance to just under the window, then send another event:
        // this must reset the timer rather than let it fire.
        tokio::time::advance(DEBOUNCE_WINDOW - Duration::from_millis(50)).await;
        tx.send(PathBuf::from("b.md")).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(DEBOUNCE_WINDOW + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        drop(tx);
        task.await.unwrap();

        let batches = fired.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "an event near the deadline must extend the same batch, got {batches:?}"
        );
        assert_eq!(batches[0].len(), 2);
    }

    /// Duplicate paths within one window collapse to a single entry
    /// (the batch is a set, not a list) — re-parsing the same file
    /// once per burst, not once per raw event (GP6: "re-parse only
    /// the changed file").
    #[tokio::test(start_paused = true)]
    async fn duplicate_paths_in_one_window_collapse_to_one_entry() {
        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let fired: Arc<Mutex<Vec<HashSet<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_for_task = Arc::clone(&fired);

        let task = tokio::spawn(async move {
            debounce_loop(rx, DEBOUNCE_WINDOW, DEBOUNCE_MAX_WAIT, move |batch| {
                fired_for_task.lock().unwrap().push(batch);
            })
            .await;
        });

        for _ in 0..20 {
            tx.send(PathBuf::from("same.md")).unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(DEBOUNCE_WINDOW + Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        drop(tx);
        task.await.unwrap();

        let batches = fired.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    /// R9: a stream of events arriving faster than the debounce
    /// window apart must still fire within the bounded `max_wait`
    /// cap, rather than postponing the batch indefinitely (the
    /// `quiesce` timer alone would never elapse here, since every new
    /// event resets it before it can). Uses a short `max_wait` so the
    /// virtual-clock test runs instantly while still covering several
    /// cap firings.
    #[tokio::test(start_paused = true)]
    async fn continuous_events_faster_than_the_window_still_fire_within_the_max_wait_cap() {
        let window = Duration::from_millis(250);
        let max_wait = Duration::from_millis(1000);
        let event_spacing = Duration::from_millis(100); // faster than `window`

        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let fired: Arc<Mutex<Vec<HashSet<PathBuf>>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_for_task = Arc::clone(&fired);

        let task = tokio::spawn(async move {
            debounce_loop(rx, window, max_wait, move |batch| {
                fired_for_task.lock().unwrap().push(batch);
            })
            .await;
        });

        let total_events = 20;
        let mut sent = Vec::new();
        for i in 0..total_events {
            let path = PathBuf::from(format!("f{i}.md"));
            tx.send(path.clone()).unwrap();
            sent.push(path);
            tokio::task::yield_now().await;
            tokio::time::advance(event_spacing).await;
            tokio::task::yield_now().await;
        }

        drop(tx);
        task.await.unwrap();

        let batches = fired.lock().unwrap();
        assert!(
            batches.len() >= 2,
            "a continuous stream longer than max_wait must fire more than once, got {} batches",
            batches.len()
        );

        let total_reported: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(
            total_reported,
            sent.len(),
            "every sent path must appear in exactly one fired batch"
        );
        for path in &sent {
            let appearances = batches.iter().filter(|b| b.contains(path)).count();
            assert_eq!(appearances, 1, "{path:?} must appear in exactly one batch");
        }
    }
}
