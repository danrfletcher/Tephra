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
use tephrad::rpc::backend::{GraphBackend, GraphRefresh, SharedGraph, SourceBackend};
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
        shared_graph as Arc<dyn GraphBackend>,
        registry as Arc<dyn SourceBackend>,
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
