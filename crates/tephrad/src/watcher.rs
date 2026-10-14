//! The per-source `notify`-backed watcher and its 250ms debounce
//! (G2).
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
//! - [`spawn`]: wires a real `notify::RecommendedWatcher` on a
//!   source's root into [`debounce_loop`], and is exercised only by
//!   the integration tests under `tests/`, which use a real
//!   filesystem and the real 250ms window.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// The hardcoded debounce window (G2): no per-source override in v1.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// Coalesces a stream of changed-path events arriving on `rx` into
/// batches: a batch accumulates every path received, and fires (via
/// `on_fire`) once `window` elapses with no further event arriving
/// (quiescence-based debounce — a burst of events spaced closer
/// together than `window`, however long the burst runs, coalesces
/// into exactly one fired batch, per E5/GP6). The timer resets on
/// every new event; it only actually fires once the stream goes
/// quiet.
///
/// Returns once `rx` is closed (the sender side dropped), after
/// firing one final batch if anything was still pending.
pub async fn debounce_loop<F>(
    mut rx: mpsc::UnboundedReceiver<PathBuf>,
    window: Duration,
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

        let sleep = tokio::time::sleep(window);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                _ = &mut sleep => {
                    on_fire(std::mem::take(&mut pending));
                    break;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(path) => {
                            pending.insert(path);
                            sleep.as_mut().reset(tokio::time::Instant::now() + window);
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

/// Spawns a dedicated watcher for `root`: a real recursive `notify`
/// watch, forwarding every observed path through [`debounce_loop`] on
/// `runtime`, invoking `on_batch` once per coalesced batch and
/// `on_root_removed` if `root` itself is ever found missing (the
/// watched root was deleted or moved out from under the watcher —
/// handled here rather than crashing, consistent with Linux inotify
/// emitting no distinct "folder deleted" error of its own: every
/// incoming event opportunistically re-checks whether `root` still
/// resolves, and routes to `on_root_removed` instead of the normal
/// batch path the first time it doesn't).
pub fn spawn<B, R>(
    root: PathBuf,
    runtime: tokio::runtime::Handle,
    on_batch: B,
    on_root_removed: R,
) -> Result<WatcherHandle, WatchError>
where
    B: Fn(HashSet<PathBuf>) + Send + Sync + 'static,
    R: Fn() + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
    let root_for_watch = root.clone();
    let tx_for_events = tx.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        // Always forward every observed path, even once `root` no
        // longer resolves: the task-level `on_batch_checked` wrapper
        // below re-checks `root` itself and routes to
        // `on_root_removed` instead of the normal batch path once
        // it's missing. If this callback dropped events here
        // instead, a root deletion might never produce anything for
        // the debounce loop to fire on at all, and `on_root_removed`
        // would never run (Linux inotify in particular emits no
        // distinct "folder deleted" error of its own — the ordinary
        // remove event for the root path itself is the only signal).
        let _ = root_for_watch;
        for path in event.paths {
            let _ = tx_for_events.send(path);
        }
    })?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    let root_for_task = root.clone();
    let task = runtime.spawn(async move {
        let root_check = root_for_task.clone();
        let on_batch_checked = move |batch: HashSet<PathBuf>| {
            if !root_check.exists() {
                on_root_removed();
            } else {
                on_batch(batch);
            }
        };
        debounce_loop(rx, DEBOUNCE_WINDOW, on_batch_checked).await;
    });

    Ok(WatcherHandle {
        _watcher: watcher,
        task,
    })
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
            debounce_loop(rx, DEBOUNCE_WINDOW, move |batch| {
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
            debounce_loop(rx, DEBOUNCE_WINDOW, move |batch| {
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
            debounce_loop(rx, DEBOUNCE_WINDOW, move |batch| {
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
            debounce_loop(rx, DEBOUNCE_WINDOW, move |batch| {
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
}
