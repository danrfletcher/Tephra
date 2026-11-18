//! The `graph:changed` forwarder (GP6, G17): turns every [`DaemonEvent`]
//! the registry produces into one push to the connected WebSocket
//! clients, after refreshing the in-memory graph.
//!
//! It lives in the library, not in `main.rs`, so a test can run the very
//! task the daemon runs against a real registry, graph and WebSocket
//! client.
//!
//! The push is data only (F13): `source` and `paths` always, `fixes`
//! only when Tephra's own link fix has something to report, and
//! `sources_changed` only for a change to the source list (G16). A push
//! with neither is exactly what it was before they existed. Both go out
//! through [`push`], the only caller of `broadcast_graph_changed` in the
//! daemon. Nothing here waits for a client: the push is broadcast and the
//! loop goes on.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use tokio::sync::watch;

use crate::fixes::FixEngine;
use crate::registry::DaemonEvent;
use crate::rpc::server::AppState;

/// The `graph:changed` payload for `event`.
///
/// A file change keeps `source` and `paths` (and `fixes` when Tephra's own
/// fix has something to report). A source-list change keeps the same two
/// keys, `paths` empty (the whole-source form), `source` the first named
/// source, and adds `sources_changed` (G16). A client that ignores the new
/// key re-queries as before.
pub fn payload(event: &DaemonEvent) -> Value {
    let (source, paths, fixes, changes) = match event {
        DaemonEvent::SourceChanged {
            source,
            paths,
            fixes,
        } => (source.as_str(), path_strings(paths), fixes.as_slice(), None),
        DaemonEvent::SourceListChanged { changes, fixes } => (
            changes.first().map(|c| c.name.as_str()).unwrap_or(""),
            Vec::new(),
            fixes.as_slice(),
            Some(changes),
        ),
    };
    let mut body = json!({
        "source": source,
        "paths": paths,
    });
    if !fixes.is_empty() {
        body["fixes"] = json!(fixes);
    }
    if let Some(changes) = changes {
        body["sources_changed"] = Value::Array(changes.iter().map(|c| c.to_json()).collect());
    }
    body
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

/// Refreshes the in-memory graph, then broadcasts `event`'s push: so a
/// client that re-queries on receipt sees the new links (G17). The fix
/// log entry the event reports was written before the event was sent.
pub async fn push(state: &Arc<AppState>, event: &DaemonEvent) {
    if let Some(refresh) = state.graph_refresh.clone() {
        let refreshed = tokio::task::spawn_blocking(move || refresh.refresh()).await;
        match refreshed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to refresh the in-memory graph after a watcher event");
            }
            Err(e) => {
                tracing::warn!(error = %e, "graph refresh task panicked");
            }
        }
    }
    state.broadcast_graph_changed(payload(event));
}

/// Forwards events until the stop signal fires or the event stream
/// closes. The loop must end on the signal: `main.rs` waits for this
/// task before it hands the registry back to be dropped.
pub async fn run(
    state: Arc<AppState>,
    fixes: Arc<FixEngine>,
    mut events: Receiver<DaemonEvent>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            recv = events.recv() => match recv {
                Ok(event) => {
                    push(&state, &event).await;
                    // G9: a watcher batch retries the pending fixes of
                    // that source whose backoff has passed, and any that
                    // need a person whose file is in the batch. The
                    // fix's own event does not: its files are not the
                    // ones a person has to fix.
                    let DaemonEvent::SourceChanged {
                        source,
                        paths,
                        fixes: announced,
                    } = event
                    else {
                        // A source-list push has no files to retry fixes for.
                        continue;
                    };
                    if !announced.is_empty() {
                        continue;
                    }
                    let fixes = fixes.clone();
                    let retried =
                        tokio::task::spawn_blocking(move || fixes.after_batch(&source, &paths))
                            .await;
                    match retried {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "retrying pending fixes after a watcher event failed");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "pending-fixes retry task panicked");
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    }
}
