//! R1 regression: proves the production RPC server's in-memory graph
//! reflects a source registered over RPC on the *same running*
//! daemon, with no restart in between — the live repro in R1's review
//! finding (`source.register "notes"` succeeded, then
//! `unit.resolve_link [[@notes:Two]]` still answered `-32010 "no
//! source named \"notes\" is registered"` until the daemon was
//! restarted).
//!
//! Deliberately exercises the *real* `tephrad::registry::SourceRegistry`
//! and the real `tephra_graph::UnitGraph`/`SharedGraph` (not
//! `rpc_fixtures`'s mocked backends): this is specifically a test of
//! the wiring between those two real components and the RPC dispatch
//! table, which a mocked `GraphBackend` can't prove by construction
//! (a mock can "resolve" anything regardless of whether the real
//! graph was ever rebuilt). Still no real `~/.local/share/tephra`
//! directory is touched — every value below lives under its own
//! disposable tempdir, exactly like every other integration test in
//! this crate.

mod fixtures;

use std::sync::Arc;

use serde_json::json;
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

#[tokio::test]
async fn registering_a_source_over_rpc_makes_it_resolvable_on_the_same_running_daemon() {
    let fixture = fixtures::vault_fixture();

    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");
    let token_path = data_dir.path().join("token");
    let token = "a".repeat(64);
    std::fs::write(&token_path, &token).expect("write token");

    // Mirrors `main.rs`'s own startup wiring exactly: a dedicated
    // cache handle for the registry, a second one (built into an
    // initial, empty `UnitGraph` snapshot) for the graph, and a
    // `GraphRefresh` tying the two together via the shared cache path.
    let registry_cache = Cache::open(&cache_path).expect("open cache for registry");
    let registry = Arc::new(SourceRegistry::new(registry_cache).expect("new registry"));

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let initial_graph = UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
    let shared_graph = Arc::new(SharedGraph::new(initial_graph));
    let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let state = Arc::new(AppState::new(
        token_path,
        shared_graph.clone() as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        Arc::new(tephrad::refactor::RefactorEngine::new(
            shared_graph as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry as Arc<dyn tephrad::refactor::WatcherPauseControl>,
        )) as Arc<dyn RefactorBackend>,
        Some(graph_refresh),
    ));
    let router = build_router(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/rpc");

    // Before registration: the graph has no sources at all, so even
    // `graph.get_edges` must answer empty rather than erroring.
    let edges_before = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(edges_before["result"], json!([]));

    // GP2/GP3: register the source over RPC.
    let register = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "source.register",
            "params": { "name": "notes", "root": fixture.root().to_string_lossy() }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(register["result"]["units_written"], json!(2));
    assert_eq!(register["result"]["links_written"], json!(1));

    // GP4/GP5: resolve a link into the just-registered source, on the
    // very same running server, with no restart in between — this is
    // exactly R1's repro. `fixtures::vault_fixture()`'s own `[[Two]]`
    // link (inside `one.md`) is deliberately case-mismatched against
    // `two.md`'s stem (resolution is case-sensitive — only NFC
    // normalization is applied, per `resolver::normalize_key`), so it
    // is `two.md`'s own exact-case basename that is used here to
    // prove resolution, not that pre-existing in-fixture link.
    let resolve = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "unit.resolve_link",
            "params": { "link": "[[two]]", "default_source": "notes" }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        resolve.get("error").is_none(),
        "unit.resolve_link must not still answer with a stale graph's error: {resolve:?}"
    );
    assert_eq!(resolve["result"]["unit"]["id"], json!("@notes:two.md"));

    let edges_after = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        edges_after["result"].as_array().unwrap().len(),
        1,
        "the one.md -> Two wikilink must now show up in the live graph"
    );

    server.abort();
}

/// R9, end-to-end over the real RPC transport: two `source.register`
/// calls fired concurrently at the same running daemon must both end
/// up resolvable, regardless of which one's background graph rebuild
/// happens to *finish* first. A base source is registered first so
/// every subsequent rebuild has real, non-trivial work to do, widening
/// the real race window — this is a best-effort widening over the
/// network transport, not a guaranteed reproduction on every run (the
/// deterministic proof is `concurrent_refreshes_across_many_threads_
/// never_lose_a_committed_source` below, which drives `GraphRefresh`
/// directly from many real OS threads with no HTTP/async scheduling
/// noise in between). This test instead proves the fix holds up
/// through the actual wire protocol end to end.
#[tokio::test]
async fn concurrent_source_register_calls_leave_both_sources_resolvable() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");
    let token_path = data_dir.path().join("token");
    let token = "b".repeat(64);
    std::fs::write(&token_path, &token).expect("write token");

    let registry_cache = Cache::open(&cache_path).expect("open cache for registry");
    let registry = Arc::new(SourceRegistry::new(registry_cache).expect("new registry"));

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let initial_graph = UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
    let shared_graph = Arc::new(SharedGraph::new(initial_graph));
    let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let state = Arc::new(AppState::new(
        token_path,
        shared_graph.clone() as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        Arc::new(tephrad::refactor::RefactorEngine::new(
            shared_graph as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry as Arc<dyn tephrad::refactor::WatcherPauseControl>,
        )) as Arc<dyn RefactorBackend>,
        Some(graph_refresh),
    ));
    let router = build_router(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let url = format!("http://{addr}/rpc");
    let client = reqwest::Client::new();

    // Seed a large base source first (not part of the race itself),
    // purely so every later rebuild has real work to do.
    let base = tempfile::tempdir().expect("base vault");
    for i in 0..600 {
        std::fs::write(
            base.path().join(format!("note-{i}.md")),
            format!("# Note {i}\n"),
        )
        .expect("write base fixture file");
    }
    let base_result = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "source.register",
            "params": { "name": "base", "root": base.path().to_string_lossy() }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(base_result["result"]["units_written"], json!(600));

    // `alpha` and `beta`: one tiny file each, registered concurrently.
    let alpha = tempfile::tempdir().expect("alpha vault");
    std::fs::write(alpha.path().join("only.md"), "# Only\n").expect("write alpha fixture file");
    let beta = tempfile::tempdir().expect("beta vault");
    std::fs::write(beta.path().join("only.md"), "# Only\n").expect("write beta fixture file");

    let alpha_task = {
        let url = url.clone();
        let token = token.clone();
        let root = alpha.path().to_string_lossy().to_string();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({
                    "jsonrpc": "2.0", "id": 1, "method": "source.register",
                    "params": { "name": "alpha", "root": root }
                }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        })
    };
    let beta_task = {
        let url = url.clone();
        let token = token.clone();
        let root = beta.path().to_string_lossy().to_string();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({
                    "jsonrpc": "2.0", "id": 2, "method": "source.register",
                    "params": { "name": "beta", "root": root }
                }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        })
    };

    let (alpha_result, beta_result) = tokio::join!(alpha_task, beta_task);
    let alpha_result = alpha_result.expect("alpha register task must not panic");
    let beta_result = beta_result.expect("beta register task must not panic");
    assert_eq!(alpha_result["result"]["units_written"], json!(1));
    assert_eq!(beta_result["result"]["units_written"], json!(1));

    // All three sources must be resolvable on this same running
    // daemon, no matter which concurrent rebuild happened to finish
    // (and thus `replace()`) last.
    for (source, link) in [
        ("base", "[[note-0]]"),
        ("alpha", "[[only]]"),
        ("beta", "[[only]]"),
    ] {
        let resolve = client
            .post(&url)
            .bearer_auth(&token)
            .json(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "unit.resolve_link",
                "params": { "link": link, "default_source": source }
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert!(
            resolve.get("error").is_none(),
            "source \"{source}\" must resolve after the concurrent registrations, not be \
             silently dropped by a stale concurrent rebuild landing last (R9): {resolve:?}"
        );
    }

    server.abort();
}

/// R9, functional coverage: drives `SourceRegistry::register` +
/// `GraphRefresh::refresh` directly from many real OS threads (no
/// HTTP, no async scheduler, no dispatch layer in between — the exact
/// production pair every successful mutating `source.*` RPC call
/// makes, per `server.rs`'s `handle_single_request`), all released
/// from a `Barrier` at once, and asserts every one of them ends up
/// resolvable. This is the user-visible correctness property R9
/// protects; the deterministic proof that the *mechanism* behind it
/// (full serialization of `refresh()`'s read-build-replace sequence)
/// is actually in effect is the timing-based test right below, which
/// reliably fails without the fix — real concurrent `register` calls
/// turned out not to reproduce the underlying race reliably enough in
/// this environment to use as the primary regression guard on their
/// own (confirmed by running this test repeatedly against the pre-fix
/// code: it did not fail), so it is kept here as a secondary,
/// black-box sanity check rather than the proof of the fix.
#[test]
fn concurrent_refreshes_across_many_threads_never_lose_a_committed_source() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");

    let registry_cache = Cache::open(&cache_path).expect("open cache for registry");
    let registry = Arc::new(SourceRegistry::new(registry_cache).expect("new registry"));

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let initial_graph = UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
    let shared_graph = Arc::new(SharedGraph::new(initial_graph));
    let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());

    // A base source, registered up front (not part of the race
    // itself), purely so every later `refresh()` rebuild has real,
    // measurable work to do.
    let base = tempfile::tempdir().expect("base vault");
    for i in 0..6000 {
        std::fs::write(
            base.path().join(format!("note-{i}.md")),
            format!("# Note {i}\n"),
        )
        .expect("write base fixture file");
    }
    registry
        .register("base", base.path())
        .expect("register base");
    graph_refresh
        .refresh()
        .expect("initial refresh after registering base");

    const N: usize = 16;
    let vaults: Vec<_> = (0..N)
        .map(|_| {
            let vault = tempfile::tempdir().expect("vault tempdir");
            std::fs::write(vault.path().join("only.md"), "# Only\n")
                .expect("write vault fixture file");
            vault
        })
        .collect();

    let barrier = Arc::new(std::sync::Barrier::new(N));
    let handles: Vec<_> = vaults
        .iter()
        .enumerate()
        .map(|(i, vault)| {
            let registry = registry.clone();
            let graph_refresh = graph_refresh.clone();
            let barrier = barrier.clone();
            let root = vault.path().to_path_buf();
            let name = format!("s{i}");
            std::thread::spawn(move || {
                barrier.wait();
                registry.register(&name, &root).expect("register");
                graph_refresh.refresh().expect("refresh");
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("worker thread must not panic");
    }

    let base_resolved = shared_graph.resolve_link("[[note-0]]", Some("base"));
    assert!(
        base_resolved.is_ok(),
        "the base source must still resolve after the concurrent registrations: {base_resolved:?}"
    );
    for i in 0..N {
        let name = format!("s{i}");
        let resolved = shared_graph.resolve_link("[[only]]", Some(&name));
        assert!(
            resolved.is_ok(),
            "source \"{name}\" must resolve after {N} concurrent registrations, not be \
             silently dropped by a stale concurrent rebuild landing last (R9): {resolved:?}"
        );
    }
}

/// R9, deterministic proof: directly measures whether two concurrent
/// `GraphRefresh::refresh()` calls run in parallel or are serialized.
///
/// Before the fix, `refresh()` opened the cache, ran the full
/// `UnitGraph::build_from_cache` rebuild, and called
/// `SharedGraph::replace` with nothing serializing those three steps
/// across concurrent callers — two calls could, and in this test's
/// probing did, run concurrently and both complete in roughly the
/// time of *one* call. With the fix's `rebuild_lock` held across that
/// whole sequence, the second caller to acquire it cannot start its
/// own rebuild until the first one has fully finished (read, build,
/// *and* replace), so two concurrent calls together always take at
/// least on the order of *two* calls' worth of time, never one's.
///
/// A sizeable base source is registered first purely to make a
/// single `refresh()` call's own duration (measured directly below,
/// rather than assumed) long enough to be a reliable, non-flaky yardstick
/// for comparison, instead of hard-coding a magic duration that would
/// be meaningless on a faster or slower machine.
#[test]
fn concurrent_refresh_calls_are_serialized_not_run_in_parallel() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");

    let registry_cache = Cache::open(&cache_path).expect("open cache for registry");
    let registry = SourceRegistry::new(registry_cache).expect("new registry");

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let initial_graph = UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
    let shared_graph = Arc::new(SharedGraph::new(initial_graph));
    let graph_refresh = GraphRefresh::new(shared_graph, cache_path.clone());

    let base = tempfile::tempdir().expect("base vault");
    for i in 0..6000 {
        std::fs::write(
            base.path().join(format!("note-{i}.md")),
            format!("# Note {i}\n"),
        )
        .expect("write base fixture file");
    }
    registry
        .register("base", base.path())
        .expect("register base");

    // Calibrate against this machine's own speed: time one call alone
    // first, with nothing else contending for the lock.
    let single_call_started = std::time::Instant::now();
    graph_refresh.refresh().expect("calibration refresh");
    let single_call_duration = single_call_started.elapsed();

    // Now fire two calls at once, released from a `Barrier` together,
    // and time each one independently from the moment it was
    // released.
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = {
        let graph_refresh = graph_refresh.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            let started = std::time::Instant::now();
            graph_refresh
                .refresh()
                .expect("concurrent refresh (other thread)");
            started.elapsed()
        })
    };
    barrier.wait();
    let this_started = std::time::Instant::now();
    graph_refresh
        .refresh()
        .expect("concurrent refresh (this thread)");
    let this_duration = this_started.elapsed();
    let other_duration = other.join().expect("other thread must not panic");

    // Whichever of the two calls had to wait for the lock takes
    // roughly as long as *two* calibrated calls; if they instead ran
    // in parallel, both would take roughly as long as *one*. A
    // 1.5x-single threshold sits cleanly between those two outcomes.
    let slower = this_duration.max(other_duration);
    let threshold = single_call_duration + single_call_duration / 2;
    assert!(
        slower >= threshold,
        "two concurrent refresh() calls completed in {this_duration:?} and {other_duration:?} \
         (a single calibrated call took {single_call_duration:?}); the slower of the two must \
         take at least ~1.5x a single call's duration, proving it waited for the other to fully \
         finish (read, build, and replace) rather than racing it — got {slower:?}, which is \
         consistent with the two calls having run concurrently instead of being serialized by \
         `rebuild_lock` (R9)"
    );
}
