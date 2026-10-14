//! R6(a) review finding: "No crates/tephractl/tests/link_refactor_cli.rs
//! exists, and no test drives tephractl `link refactor` (preview,
//! --apply, staleness) against the real engine. Only PR-7's
//! mock-server fixtures in tests/cli/link_commands.rs exist, so the
//! real wire contract between daemon and CLI is never exercised by a
//! test."
//!
//! This file closes that gap: it starts a real `tephrad` daemon (the
//! actual `AppState`/router/`RefactorEngine`/`SourceRegistry` this
//! crate talks to in production, not a canned JSON fixture) on a
//! background thread bound to a real ephemeral TCP port, registers a
//! real temp-directory source, and drives it with `tephractl`'s own
//! `commands::execute`/`RpcClient` — exactly the two functions
//! `main.rs` calls in production. Mirrors
//! `tephrad/tests/link_refactor_integration.rs`'s `TestDaemon`
//! rationale ("a mocked backend can't prove the real engine's file
//! reads/writes ... actually happen") from the client side of the
//! same wire.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;
use tephrad::refactor::{RefactorEngine, WatcherPauseControl};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

/// A real `tephrad` daemon, started on a background OS thread with
/// its own Tokio runtime, bound to a real ephemeral port. Dropped
/// (via `stop`) at the end of each test so the thread and its
/// temp-directory cache don't leak into later tests.
struct RealDaemon {
    addr: std::net::SocketAddr,
    token: String,
    _cache_dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RealDaemon {
    fn start() -> Self {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let cache_path = cache_dir.path().join("cache.db");
        let token_path = cache_dir.path().join("token");
        let token = "e".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let thread_cache_path = cache_path.clone();
        let thread_token_path = token_path.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime for real daemon");
            rt.block_on(async move {
                let registry_cache =
                    Cache::open(&thread_cache_path).expect("open cache for registry");
                let registry = Arc::new(SourceRegistry::new(registry_cache).expect("new registry"));

                let graph_cache = Cache::open(&thread_cache_path).expect("open cache for graph");
                let initial_graph =
                    UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
                let shared_graph = Arc::new(SharedGraph::new(initial_graph));
                let graph_refresh = GraphRefresh::new(shared_graph.clone(), thread_cache_path);

                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind ephemeral port");
                let addr = listener.local_addr().expect("local_addr");

                let refactor_engine = RefactorEngine::new(
                    shared_graph.clone() as Arc<dyn GraphBackend>,
                    registry.clone() as Arc<dyn SourceBackend>,
                    registry.clone() as Arc<dyn WatcherPauseControl>,
                );

                let state = Arc::new(AppState::new(
                    thread_token_path,
                    shared_graph as Arc<dyn GraphBackend>,
                    registry as Arc<dyn SourceBackend>,
                    Arc::new(refactor_engine) as Arc<dyn RefactorBackend>,
                    Some(graph_refresh),
                ));
                let router = build_router(state);

                addr_tx.send(addr).expect("send real daemon addr");

                // Run the server as its own spawned task (mirroring
                // `tephrad`'s own `TestDaemon::stop`, which `abort()`s
                // its server task rather than awaiting it to natural
                // completion) and cancel that task on shutdown rather
                // than awaiting `axum::serve(...)` to completion
                // in-line. Awaiting it to completion here drops the
                // `Router`/`AppState` (and everything it owns —
                // `SourceRegistry`, its watcher threads, `GraphRefresh`)
                // synchronously as part of *this* future's own poll,
                // inside tokio's "blocking disallowed" context; some
                // teardown somewhere in that chain does a blocking
                // join, which panics there. Aborting the task instead
                // defers that same drop to tokio's own task-cleanup
                // machinery instead of this stack frame.
                let server_task = tokio::spawn(async move {
                    let _ = axum::serve(listener, router).await;
                });
                let _ = shutdown_rx.await;
                server_task.abort();
            });
            // Non-blocking: see the rationale above for why `rt` is
            // explicitly shut down this way rather than simply
            // dropped (which — like awaiting the server task above —
            // runs the same teardown chain synchronously and hits
            // the same "blocking disallowed" panic from `Runtime`'s
            // own `Drop` impl).
            rt.shutdown_background();
        });

        let addr = addr_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("real daemon failed to report its address");

        RealDaemon {
            addr,
            token,
            _cache_dir: cache_dir,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn client(&self) -> RpcClient {
        RpcClient::new(self.addr.to_string(), self.token.clone())
    }

    fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("real daemon thread panicked");
        }
    }
}

/// G7/E3/GP7 against the real engine, driven entirely through
/// `tephractl`'s own public API (`commands::execute` + `RpcClient`):
/// `link refactor` (preview), then `link refactor --apply --yes`,
/// then a staleness check on a second apply attempt against the same
/// (now-stale) preview. Proves the full daemon<->CLI wire contract
/// end to end, not a JSON fixture standing in for it.
#[test]
fn link_refactor_preview_then_apply_then_staleness_against_a_real_daemon() {
    let daemon = RealDaemon::start();
    let client = daemon.client();

    let source = tempfile::tempdir().expect("source tempdir");
    std::fs::write(source.path().join("OldDoc.md"), "# Old Doc\n").expect("write OldDoc.md");
    std::fs::write(
        source.path().join("Journal.md"),
        "See [[OldDoc]] for details.\n",
    )
    .expect("write Journal.md");

    let register_outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: source.path().to_string_lossy().to_string(),
        },
        true,
        &client,
        &mut std::io::empty(),
    );
    assert_eq!(
        register_outcome.exit_code, 0,
        "source.register failed: {}",
        register_outcome.stderr
    );

    // 1. Preview (R6a: real engine, not a mock) renders the one real
    // change and leaves the files untouched.
    let preview_outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: false,
            yes: false,
        },
        true,
        &client,
        &mut std::io::empty(),
    );
    assert_eq!(
        preview_outcome.exit_code, 0,
        "preview failed: {}",
        preview_outcome.stderr
    );
    let preview_json: serde_json::Value =
        serde_json::from_str(&preview_outcome.stdout).expect("preview stdout is valid JSON");
    assert_eq!(preview_json["changes"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(source.path().join("Journal.md")).unwrap(),
        "See [[OldDoc]] for details.\n",
        "preview must never write"
    );

    // 2. Apply (--yes, so no interactive prompt) actually rewrites
    // Journal.md on disk through the real atomic-rewrite path.
    let apply_outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        true,
        &client,
        &mut std::io::empty(),
    );
    assert_eq!(
        apply_outcome.exit_code, 0,
        "apply failed: {}",
        apply_outcome.stderr
    );
    let apply_json: serde_json::Value =
        serde_json::from_str(&apply_outcome.stdout).expect("apply stdout is valid JSON");
    assert_eq!(apply_json["status"], json!("applied"));
    assert_eq!(apply_json["files_changed"], json!(1));
    assert_eq!(
        std::fs::read_to_string(source.path().join("Journal.md")).unwrap(),
        "See [[NewDoc]] for details.\n",
        "the real daemon must have rewritten Journal.md through tephractl's own apply call"
    );

    // 3. Staleness (E3): the *first* preview (taken before the
    // rename above) is now stale. Applying it again — exactly as a
    // user re-running an old shell history entry would — must abort
    // cleanly rather than silently corrupt the now-renamed file.
    let stale_apply_outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        true,
        &client,
        &mut std::io::empty(),
    );
    assert_ne!(
        stale_apply_outcome.exit_code, 0,
        "re-applying a now-stale rename must exit non-zero, not succeed a second time"
    );
    assert!(
        !stale_apply_outcome
            .stdout
            .contains("\"status\":\"applied\""),
        "a second, now-stale apply of the same rename must not report success: {}",
        stale_apply_outcome.stdout
    );

    assert_eq!(
        std::fs::read_to_string(source.path().join("Journal.md")).unwrap(),
        "See [[NewDoc]] for details.\n",
        "Journal.md must be exactly as the first, real apply left it"
    );

    daemon.stop();
}
