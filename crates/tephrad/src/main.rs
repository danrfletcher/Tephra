//! `tephrad`: federated graph and link engine daemon.
//!
//! Startup runs: CLI flag parsing, XDG path resolution, config
//! load/default-write, token ensure, the SQLite cache open and
//! schema-version check (`tephra-cache`'s `Cache::open`, G5), a bare
//! TCP listener bind, and then (PR-6) builds the live
//! [`tephrad::registry::SourceRegistry`] and an initial
//! [`tephra_graph::UnitGraph`] snapshot and serves JSON-RPC/WebSocket
//! requests against them (`tephrad::rpc`) over that listener until a
//! shutdown signal arrives.

mod cli;
mod config;
mod paths;
mod server;
mod token;

use std::process::ExitCode;
use std::sync::Arc;

use cli::Action;
use serde_json::json;
use tephrad::registry::{DaemonEvent, SourceRegistry};
use tephrad::rpc::backend::{GraphBackend, RefactorBackend, SourceBackend};
use tephrad::rpc::server::{build_router, AppState};
use tokio::sync::broadcast::error::RecvError;

/// Startup stage names used in `tephrad: <stage> failed: <reason>`
/// messages, so failures are unambiguous about where they occurred.
mod stage {
    pub const CONFIG: &str = "config";
    pub const DIRECTORY: &str = "directory";
    pub const TOKEN: &str = "token";
    pub const CACHE: &str = "cache";
    pub const LISTENER: &str = "listener";
}

fn fail(stage: &str, reason: impl std::fmt::Display) -> ExitCode {
    eprintln!("tephrad: {stage} failed: {reason}");
    ExitCode::FAILURE
}

#[cfg(unix)]
fn block_shutdown_signals() {
    // Block SIGTERM/SIGINT before doing any other work in `main`, so a
    // signal arriving during the plain, synchronous startup prelude
    // (config/directory/token/cache stages, all of which run before
    // any tokio runtime or signal handler exists) is held *pending* by
    // the kernel rather than invoking the OS's default
    // "terminate immediately, no cleanup" disposition for it.
    //
    // Every worker thread the tokio runtime later spawns inherits
    // this blocked mask at thread-creation time (POSIX), so it's in
    // effect everywhere in the process, not just on the thread that
    // calls this — this one call is enough. See
    // `wait_for_shutdown_signal`'s matching `unblock_shutdown_signals`
    // call for where/why it's lifted again, once our own
    // graceful-shutdown handler is actually armed.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
fn block_shutdown_signals() {}

#[cfg(unix)]
fn unblock_shutdown_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
fn unblock_shutdown_signals() {}

fn main() -> ExitCode {
    block_shutdown_signals();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let default_config_path = paths::default_config_path();

    let action = match cli::parse_args(&args, default_config_path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("tephrad: {e}");
            eprint!("{}", cli::USAGE);
            return ExitCode::FAILURE;
        }
    };

    let config_path = match action {
        Action::Help => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Action::Run { config_path } => config_path,
    };

    // Stage 1: config load (defaults are written here if the file is
    // absent, but we don't log yet — logging isn't initialized until we
    // know the configured log_level).
    let loaded = match config::load_or_create(&config_path, paths::data_dir()) {
        Ok(loaded) => loaded,
        Err(e) => return fail(stage::CONFIG, e),
    };
    let cfg = loaded.config;

    init_tracing(&cfg.log_level);

    if loaded.defaults_written {
        tracing::info!(path = %config_path.display(), "wrote default config (file was absent)");
    } else {
        tracing::info!(path = %config_path.display(), "loaded config");
    }

    // Stage 2: directory creation. Delegates to `token::ensure_data_dir`
    // (rather than a bare `create_dir_all`) so the directory is created
    // mode 0700 regardless of which startup stage creates it first —
    // otherwise the token-ensure stage below would see the directory
    // already existing and skip hardening its permissions.
    if let Err(e) = token::ensure_data_dir(&cfg.data_dir) {
        return fail(stage::DIRECTORY, e);
    }
    tracing::info!(path = %cfg.data_dir.display(), "data directory ready");

    // Stage 3: token ensure.
    let token_outcome = match token::ensure_token(&cfg.data_dir) {
        Ok(outcome) => outcome,
        Err(e) => return fail(stage::TOKEN, e),
    };
    if token_outcome.generated {
        tracing::info!("generated new auth token");
    } else if token_outcome.permissions_repaired {
        tracing::warn!("token file permissions were loosened; tightened back to 0600");
    } else {
        tracing::info!("reusing existing auth token");
    }
    // The token value itself is never logged, at any level, above.

    // Stage 4: cache open. Creates `<data_dir>/cache.db` if absent
    // (parent directory included), applies the WAL pragmas, and runs
    // the schema-version check/rebuild (G5) — see `tephra_cache::Cache::open`.
    // A failure here is a hard startup failure, same as every other
    // stage; the daemon has nothing useful to do without its cache.
    let cache_path = cfg.data_dir.join("cache.db");
    match tephra_cache::Cache::open(&cache_path) {
        Ok(cache) => {
            tracing::info!(path = %cache.path().display(), "cache ready");
        }
        Err(e) => return fail(stage::CACHE, e),
    }

    run_server(&cfg.listen_addr, &cache_path, &cfg.data_dir.join("token"))
}

/// Stage 5 (listener bind), stage 5b (registry + initial graph build,
/// PR-6) and stage 6 (ready log, then serving JSON-RPC/WebSocket
/// requests until shutdown) run on a tokio runtime built here, rather
/// than via `#[tokio::main]`, so that stages 1-4 above (which are
/// synchronous and must run before any async runtime exists) stay
/// plain, directly-testable functions.
///
/// Four ordering/thread details inside the async block below are
/// load-bearing, not incidental — each one was found by directly
/// reproducing an intermittent clean-shutdown test failure, not by
/// inspection, so they're recorded here in full:
///
/// 1. `main` blocks SIGTERM/SIGINT (see `block_shutdown_signals`)
///    before doing any other work, so a signal arriving during the
///    synchronous startup prelude (stages 1-4, before any tokio
///    runtime or signal handler exists) is held *pending* by the
///    kernel rather than invoking the OS's default
///    "terminate immediately, no cleanup" disposition for it.
/// 2. The `tokio::signal::unix::signal(...)` registration calls and
///    the matching `unblock_shutdown_signals()` call below run
///    *inline*, synchronously, as the literal first lines of the
///    top-level future passed to `block_on` — not inside a separately
///    `tokio::spawn`-ed task. This is deliberate and was confirmed by
///    direct reproduction (not just reasoning about it): on this
///    platform, a signal that is pending while blocked on one thread
///    is *not* delivered when a *different* thread later unblocks it
///    — only the thread that actually performed the block (here, the
///    one `main` block_on's caller, which is the same thread that ran
///    `block_shutdown_signals()`) reliably receives it once it
///    unblocks. `Runtime::block_on`'s top-level future is polled on
///    the calling thread for this first, non-yielding poll, which is
///    exactly why the block/register/unblock sequence must stay
///    inline here rather than move into a spawned task (which tokio's
///    multi-thread scheduler can and does run on a different worker
///    thread). Once unblocked here, later signals are delivered fine
///    regardless of which thread happens to be running at the time —
///    only this initial block/unblock handoff is thread-sensitive.
/// 3. `SourceRegistry::new` builds and owns its own dedicated internal
///    `tokio::runtime::Runtime` (for its watcher tasks), and dropping
///    a `Runtime` while the current thread is inside another
///    runtime's `block_on` panics ("Cannot drop a runtime in a
///    context where blocking is not allowed"). The async block below
///    therefore hands its `Arc<SourceRegistry>` back out as part of
///    its return value, so the final, refcount-zero drop of it always
///    happens in this function's plain synchronous frame, strictly
///    after `runtime.block_on` has already returned — never while
///    still inside it.
/// 4. The `graph:changed` forwarder task spawned below holds its own
///    `Arc<AppState>` (and, through it, another reference to the same
///    `Arc<SourceRegistry>` from point 3). Before this was fixed
///    (R8), that task's loop only ended on `RecvError::Closed`, which
///    can never happen while the task itself keeps the registry's
///    event sender alive — so the task ran forever, and `drop(runtime)`
///    below tore it down by force from a tokio worker thread, which
///    (by point 3's own rule) panics. The forwarder now selects on the
///    same shutdown signal `axum::serve`'s graceful shutdown uses, and
///    this function awaits its `JoinHandle` *inside* the async block,
///    before constructing the tuple handed back out — so its
///    `Arc<AppState>` clone, and the registry reference it carries,
///    are already gone well before `registry` itself is returned.
/// 5. Every upgraded WebSocket connection's `handle_socket` task
///    (`rpc::server`) also holds an `Arc<AppState>` clone — and
///    `axum::serve`'s own graceful shutdown has no visibility into
///    upgraded connections at all, so it returns as soon as every
///    in-flight *plain HTTP* request finishes, regardless of whether
///    any WebSocket task is still running (R10's live repro: a single
///    open, authenticated WS connection across SIGTERM panicked on
///    shutdown exactly like point 4's pre-fix forwarder did). Each
///    `handle_socket` task now also selects on `shutdown_rx` and
///    returns promptly on it (after sending a `Close` frame), and
///    every such task registers itself in `AppState::socket_tasks`
///    for its exact lifetime; this function awaits
///    `socket_tasks.wait_until_idle(..)` (bounded, in case some client
///    never acknowledges the close) immediately after joining the
///    forwarder above and before constructing the tuple handed back
///    out, for the same reason as point 4.
fn run_server(
    listen_addr: &str,
    cache_path: &std::path::Path,
    token_path: &std::path::Path,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(stage::LISTENER, e),
    };

    let (exit, registry_to_drop_after_runtime_shutdown) = runtime.block_on(async move {
        // A `watch` channel rather than a one-shot: both `axum::serve`'s
        // own graceful-shutdown future *and* the graph:changed forwarder
        // task below need to independently observe the same shutdown
        // signal (R8), and `watch::Receiver` is `Clone` where
        // `oneshot::Receiver` is not.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Inline, no `.await` yet: see point 2 in this function's doc
        // comment for why this exact sequence must run synchronously,
        // right here, on the thread that called `block_on`.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let sigterm = signal(SignalKind::terminate());
            let sigint = signal(SignalKind::interrupt());
            unblock_shutdown_signals();

            match (sigterm, sigint) {
                (Ok(mut sigterm), Ok(mut sigint)) => {
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = sigterm.recv() => {}
                            _ = sigint.recv() => {}
                        }
                        tracing::info!("received shutdown signal, stopping");
                        let _ = shutdown_tx.send(true);
                    });
                }
                _ => {
                    // Could not install either handler: fall back to
                    // Ctrl-C so the daemon still exits cleanly on some
                    // shutdown signal rather than hanging forever.
                    tokio::spawn(async move {
                        let _ = tokio::signal::ctrl_c().await;
                        tracing::info!("received shutdown signal, stopping");
                        let _ = shutdown_tx.send(true);
                    });
                }
            }
        };

        #[cfg(not(unix))]
        {
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("received shutdown signal, stopping");
                let _ = shutdown_tx.send(true);
            });
        };

        let listener = match server::bind(listen_addr).await {
            Ok(l) => l,
            Err(e) => return (fail(stage::LISTENER, e), None),
        };

        let bound_addr = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => return (fail(stage::LISTENER, e), None),
        };

        // Stage 5b: build the live source registry (its own cache
        // handle) and an initial `UnitGraph` snapshot behind a
        // [`tephrad::rpc::backend::SharedGraph`] (R1): a successful
        // mutating `source.*` RPC call, and every watcher-driven
        // `DaemonEvent` below, rebuilds this snapshot from the cache
        // and swaps it in via `GraphRefresh::refresh`, so
        // `unit.resolve_link`/`graph.get_*` reflect a source
        // registered/rescanned over RPC on this same running daemon —
        // no restart required.
        let registry_cache = match tephra_cache::Cache::open(cache_path) {
            Ok(c) => c,
            Err(e) => return (fail(stage::CACHE, e), None),
        };
        let registry = match SourceRegistry::new(registry_cache) {
            Ok(r) => Arc::new(r),
            Err(e) => return (fail(stage::CACHE, e), None),
        };

        // R8 fix: sweep every registered source's root for stale
        // `.{name}.tephra-tmp-{n}` leftovers right here, at startup —
        // see `tephrad::scan::sweep_stale_temp_files`'s doc comment
        // for why a plain restart otherwise never does this (neither
        // `SourceRegistry::new` above nor re-registering an
        // already-registered source walk the filesystem at all, and
        // `tephractl` has no rescan command either). Runs via
        // `spawn_blocking`, like every other blocking disk operation
        // in this file, and a failure here is logged but never a hard
        // startup failure — a stray temp file left behind by a failed
        // sweep is a cleanliness issue, not a reason to refuse to
        // serve.
        let registry_for_sweep = registry.clone();
        let sweep_outcome = tokio::task::spawn_blocking(move || {
            let rows = registry_for_sweep.list().map_err(|e| e.to_string())?;
            for row in rows {
                match tephrad::scan::sweep_stale_temp_files(std::path::Path::new(&row.path)) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(
                            source = %row.name,
                            removed,
                            "swept stale refactor temp files at startup"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            source = %row.name,
                            error = %e,
                            "failed to sweep stale refactor temp files at startup"
                        );
                    }
                }
            }
            Ok::<(), String>(())
        })
        .await;
        match sweep_outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to list sources for the startup temp-file sweep");
            }
            Err(e) => {
                tracing::warn!(error = %e, "startup temp-file sweep task panicked");
            }
        }

        let shared_graph = {
            let graph_cache = match tephra_cache::Cache::open(cache_path) {
                Ok(c) => c,
                Err(e) => return (fail(stage::CACHE, e), None),
            };
            match tephra_graph::UnitGraph::build_from_cache(&graph_cache) {
                Ok(g) => Arc::new(tephrad::rpc::backend::SharedGraph::new(g)),
                Err(e) => return (fail(stage::CACHE, e), None),
            }
        };
        let graph_refresh = tephrad::rpc::backend::GraphRefresh::new(
            shared_graph.clone(),
            cache_path.to_path_buf(),
        );

        let state = Arc::new(AppState::with_shutdown(
            token_path.to_path_buf(),
            shared_graph.clone() as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            Arc::new(tephrad::refactor::RefactorEngine::new(
                shared_graph as Arc<dyn GraphBackend>,
                registry.clone() as Arc<dyn SourceBackend>,
                registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
            )) as Arc<dyn RefactorBackend>,
            Some(graph_refresh),
            Some(shutdown_rx.clone()),
        ));

        // Forwards every `DaemonEvent` the registry's watchers/rescans
        // produce (GP6) to connected WebSocket clients as a
        // `graph:changed` notification, after first refreshing the
        // in-memory graph (R1) so the push actually corresponds to a
        // graph that has already changed by the time a client
        // re-queries it.
        //
        // R8 fix: this task owns `forward_state` (an `Arc<AppState>`,
        // which in turn holds the `Arc<dyn SourceBackend>` wrapping
        // this same `registry`), so it must not outlive the point
        // where this function hands `registry` back out to be dropped
        // on the plain synchronous thread (see this function's doc
        // comment, point 3). Before this fix, the loop only ended on
        // `RecvError::Closed` — which can never happen while the task
        // itself keeps `registry`'s event sender alive — so the task
        // ran forever and was only ever torn down by force when
        // `drop(runtime)` below cancelled every still-running task.
        // That cancellation (and the resulting drop of `forward_state`,
        // and transitively of the registry's own internal runtime) ran
        // on a tokio worker thread, which is exactly the "context where
        // blocking is not allowed" that panics on drop. Selecting on
        // `shutdown_rx` here, and awaiting this task's `JoinHandle`
        // below *before* `(ExitCode::SUCCESS, Some(registry))` is
        // constructed, guarantees `forward_state` is already dropped,
        // and `registry`'s refcount already back down to the single
        // reference this block is about to hand out, while still
        // inside `block_on` — never during `runtime`'s own shutdown.
        let forward_state = state.clone();
        let mut events = registry.subscribe();
        let mut forwarder_shutdown_rx = shutdown_rx.clone();
        let forwarder_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = forwarder_shutdown_rx.changed() => {
                        break;
                    }
                    recv = events.recv() => {
                        match recv {
                            Ok(DaemonEvent::SourceChanged { source, paths }) => {
                                if let Some(refresh) = forward_state.graph_refresh.clone() {
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
                                let payload = json!({
                                    "source": source,
                                    "paths": paths
                                        .iter()
                                        .map(|p| p.display().to_string())
                                        .collect::<Vec<_>>(),
                                });
                                forward_state.broadcast_graph_changed(payload);
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                }
            }
        });

        let socket_tasks = state.socket_tasks.clone();
        let app = build_router(state);

        // Stage 5: ready log. This is the single unambiguous "listening"
        // line other tools wait on.
        tracing::info!("listening on {bound_addr}");

        let mut shutdown_rx_for_serve = shutdown_rx.clone();
        let shutdown = async move {
            let _ = shutdown_rx_for_serve.changed().await;
        };

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::error!("server error: {e}");
        }

        // R8: join the forwarder task (it will already have observed
        // `shutdown_rx` by now, since `axum::serve` above only returned
        // after the same signal fired) so `forward_state` is guaranteed
        // dropped before `registry` is handed back out below.
        let _ = forwarder_handle.await;

        // R10: `axum::serve`'s own graceful shutdown above only waits
        // on in-flight plain HTTP requests — it has no idea an
        // upgraded WebSocket connection exists at all, so a client
        // holding one open is not guaranteed to have been torn down
        // yet just because `axum::serve` already returned. Each
        // `handle_socket` task does watch the same `shutdown_rx` (see
        // `rpc::server::run_socket_loop`) and sends a `Close` frame
        // promptly on it, so this wait is normally immediate; the
        // bound below exists only so a client that never acknowledges
        // the close (e.g. a frozen/unresponsive peer) cannot hang
        // shutdown forever.
        socket_tasks
            .wait_until_idle(std::time::Duration::from_secs(5))
            .await;

        (ExitCode::SUCCESS, Some(registry))
    });

    drop(registry_to_drop_after_runtime_shutdown);
    drop(runtime);
    exit
}

/// Initializes `tracing` exactly once, human-readable to stderr, at the
/// configured `log_level`. `log_level` is validated against the five
/// documented values at the config stage (`config::validate_config`)
/// before this is ever called, so it should always parse; the `info`
/// fallback below is defense-in-depth only, not the primary guard
/// against an invalid level silently disabling all output.
fn init_tracing(log_level: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
