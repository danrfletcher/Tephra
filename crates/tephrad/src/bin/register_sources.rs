//! `register_sources`: a small test-harness binary for this PR's
//! Checkpoints 1-3 (R6 review finding).
//!
//! Not a product surface — PR-6 is the real JSON-RPC entrypoint that
//! will expose `source.register`. This binary exists purely so a
//! human (or a container checkpoint script) has something to invoke
//! that drives [`tephrad::registry::SourceRegistry::register`]
//! end-to-end against a real cache file and real source roots, and so
//! the `source_registered`/`scan_complete`/`debounce_fired`/
//! `source_changed` tracing lines this PR's registry and watcher emit
//! are actually observable: it registers every `name=root` pair given
//! on argv, in order, then blocks (keeping every registered source's
//! watcher running) until it receives SIGTERM/SIGINT or (on
//! non-Unix) Ctrl-C.
//!
//! USAGE:
//!     register_sources <cache-db-path> <name>=<root> [<name>=<root> ...]
//!
//! Logs go to stderr as one structured JSON object per line (R14
//! review finding: the spec's container checkpoints "tail its
//! structured JSON logs", not human-readable `fmt` output), at `info`
//! level by default (set `RUST_LOG` to override, same as the
//! `tephrad` binary). Every `source_registered`/`scan_complete` event
//! carries the registered source's `source_id`, so a Checkpoint 2
//! `scan_complete` line can be matched back to the `source_registered`
//! line (Checkpoint 1) it belongs to.

use std::process::ExitCode;

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .json()
        .try_init();
}

fn main() -> ExitCode {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: register_sources <cache-db-path> <name>=<root> [<name>=<root> ...]");
        return ExitCode::FAILURE;
    }

    let cache_path = &args[0];
    let cache = match Cache::open(cache_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("register_sources: failed to open cache at {cache_path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let registry = match SourceRegistry::new(cache) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("register_sources: failed to start the registry: {e}");
            return ExitCode::FAILURE;
        }
    };

    for pair in &args[1..] {
        let (name, root) = match pair.split_once('=') {
            Some((n, r)) if !n.is_empty() && !r.is_empty() => (n, r),
            _ => {
                eprintln!("register_sources: expected <name>=<root>, got {pair:?}");
                return ExitCode::FAILURE;
            }
        };
        match registry.register(name, root) {
            Ok(stats) => {
                eprintln!(
                    "register_sources: registered {name:?} at {root:?} \
                     (files_seen={}, units_written={}, links_written={}, \
                     tags_written={}, blocks_written={})",
                    stats.files_seen,
                    stats.units_written,
                    stats.links_written,
                    stats.tags_written,
                    stats.blocks_written,
                );
            }
            Err(e) => {
                eprintln!("register_sources: failed to register {name:?} at {root:?}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    eprintln!(
        "register_sources: every source registered; watchers running, waiting for shutdown signal"
    );
    wait_for_shutdown_signal();
    ExitCode::SUCCESS
}

/// Blocks the current (synchronous) thread until a shutdown signal
/// arrives, reusing the same `tokio::signal` handling `tephrad`'s own
/// `main.rs` uses (SIGTERM-or-Ctrl-C on Unix, Ctrl-C elsewhere) rather
/// than a hand-rolled signal handler. Deliberately not async/
/// `#[tokio::main]` for this whole binary: `SourceRegistry` already
/// owns its own internal tokio runtime for watcher tasks (see
/// `registry.rs`), so this is a small, separate, single-purpose
/// runtime used only to await the one signal future, then dropped.
fn wait_for_shutdown_signal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build the shutdown-signal runtime");
    runtime.block_on(wait_for_shutdown_signal_async());
}

#[cfg(unix)]
async fn wait_for_shutdown_signal_async() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal_async() {
    let _ = tokio::signal::ctrl_c().await;
}
