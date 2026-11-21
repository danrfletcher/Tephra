//! `fix_helper`: a test-harness binary for the fix log and the
//! pending-fixes engine (atlas-25 PR-5), for container checkpoints that
//! need a fix to happen and a kill to land in the middle of it.
//!
//! Not a product surface: finding renames is PR-6's job, so until then
//! nothing in the daemon starts a fix. This runs the real
//! [`tephrad::fixes::FixEngine`] over a real cache file and the real
//! write loop, once, and exits. It needs no running daemon, but it may
//! run beside one (they share only the cache file).
//!
//! Built only with the `test-harness` feature (`cargo build --bin
//! fix_helper --features test-harness`), so `cargo install tephrad` does
//! not put a tool that rewrites files directly on a machine; `cargo test`
//! turns the feature on itself.
//!
//! USAGE:
//!     fix_helper <cache-db> rename <source> <old-path> <new-path> [--stop-after <n>]
//!     fix_helper <cache-db> resume [--stop-after <n>]
//!     fix_helper <cache-db> uncertain <source> <path>
//!
//! `rename` is for a rename already made on disk: it rescans the source
//! (so the cache has the new name), then logs one entry and fixes the
//! links to the old name. `--stop-after <n>` is the engine's test hook:
//! the process exits with code 3 right after the n-th file is written
//! and recorded, with nothing finished or rolled back, which leaves the
//! cache and the disk as a `kill -9` at that moment would. `resume`
//! retries every pending fix, as the daemon does at start. `uncertain`
//! logs a vanished file with no certain pair.
//!
//! Prints the entry (`fix.list`'s shape) as one JSON line on stdout.

use std::process::ExitCode;
use std::sync::Arc;

use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::fixes::{FixEngine, FixError};
use tephrad::refactor::fix_write::FixWriter;
use tephrad::refactor::{RefactorEngine, WatcherPauseControl};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{GraphBackend, SharedGraph, SourceBackend};

const USAGE: &str = "usage:\n  fix_helper <cache-db> rename <source> <old-path> <new-path> [--stop-after <n>]\n  fix_helper <cache-db> resume [--stop-after <n>]\n  fix_helper <cache-db> uncertain <source> <path>";

fn fail(message: &str) -> ExitCode {
    eprintln!("fix_helper: {message}");
    ExitCode::FAILURE
}

/// Pulls `--stop-after <n>` out of `args`.
fn stop_after(args: &mut Vec<String>) -> Result<Option<usize>, String> {
    let Some(at) = args.iter().position(|a| a == "--stop-after") else {
        return Ok(None);
    };
    let value = args
        .get(at + 1)
        .ok_or("--stop-after needs a number")?
        .parse::<usize>()
        .map_err(|_| "--stop-after needs a whole number".to_string())?;
    args.drain(at..=at + 1);
    Ok(Some(value))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let stop = match stop_after(&mut args) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    if args.len() < 2 {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    let cache_path = std::path::PathBuf::from(&args[0]);

    let registry = match Cache::open(&cache_path)
        .map_err(|e| e.to_string())
        .and_then(|c| SourceRegistry::new(c).map_err(|e| e.to_string()))
    {
        Ok(r) => Arc::new(r),
        Err(e) => return fail(&format!("cannot open {cache_path:?}: {e}")),
    };
    let graph = match Cache::open(&cache_path)
        .map_err(|e| e.to_string())
        .and_then(|c| UnitGraph::build_from_cache(&c).map_err(|e| e.to_string()))
    {
        Ok(g) => Arc::new(SharedGraph::new(g)),
        Err(e) => return fail(&format!("cannot build the graph: {e}")),
    };
    let refactor = RefactorEngine::new(
        graph as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        registry.clone() as Arc<dyn WatcherPauseControl>,
    );
    let mut engine = FixEngine::new(&cache_path, Arc::new(FixWriter::new(&refactor)));
    if let Some(n) = stop {
        engine = engine.with_stop_after_writes(n);
    }

    let outcome = match (args[1].as_str(), &args[2..]) {
        ("rename", [source, old, new]) => registry
            .rescan(source)
            .map_err(|e| FixError::Refused(e.to_string()))
            .and_then(|_| engine.begin_rename(source, old, new))
            .map(Some),
        ("resume", []) => engine.resume().map(|summary| {
            eprintln!("fix_helper: {summary:?}");
            None
        }),
        ("uncertain", [source, path]) => engine.record_uncertain(source, path).map(Some),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match outcome {
        Ok(Some(entry)) => {
            println!(
                "{}",
                serde_json::to_string(&entry).expect("entry serializes")
            );
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(FixError::Stopped) => {
            eprintln!("fix_helper: stopped by --stop-after, as a kill would leave it");
            // Exit with the pending list and the disk as they are: no
            // destructors of the registry or its watchers are wanted.
            std::process::exit(3);
        }
        Err(e) => fail(&e.to_string()),
    }
}
