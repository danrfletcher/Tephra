//! `tephrad`: federated graph and link engine daemon.
//!
//! This PR implements the startup skeleton: CLI flag parsing, XDG
//! path resolution, config load/default-write, token ensure, the
//! SQLite cache open and schema-version check (`tephra-cache`'s
//! `Cache::open`, G5), and a bare TCP listener bind. JSON-RPC dispatch
//! and HTTP/WebSocket framing land in PR-6.

mod cli;
mod config;
mod paths;
mod server;
mod token;

use std::process::ExitCode;

use cli::Action;

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

fn main() -> ExitCode {
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

    run_server(&cfg.listen_addr)
}

/// Stage 5 (listener bind) and stage 6 (ready log) run on a tokio runtime
/// built here, rather than via `#[tokio::main]`, so that stages 1-4 above
/// (which are synchronous and must run before any async runtime exists)
/// stay plain, directly-testable functions.
fn run_server(listen_addr: &str) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(stage::LISTENER, e),
    };

    runtime.block_on(async move {
        let listener = match server::bind(listen_addr).await {
            Ok(l) => l,
            Err(e) => return fail(stage::LISTENER, e),
        };

        let bound_addr = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => return fail(stage::LISTENER, e),
        };

        // Stage 5: ready log. This is the single unambiguous "listening"
        // line other tools wait on.
        tracing::info!("listening on {bound_addr}");

        wait_for_shutdown_signal().await;
        tracing::info!("received shutdown signal, stopping");
        ExitCode::SUCCESS
    })
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            // Fall back to Ctrl-C only if SIGTERM handling can't be
            // installed; still exits cleanly on any shutdown signal.
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
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
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
