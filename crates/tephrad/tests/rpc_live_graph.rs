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

/// G3 over the real RPC transport and the real registry/graph: a `.md`
/// target's backlinks list the `.txt`, `.org`, `.rst` and `.adoc` units
/// that link to it, and `unit.resolve_link` from a `.txt` resolves.
/// F7 rides along: an unprefixed `[[Foo]]` in source A's `.txt` goes to
/// A's `Foo.md`, never B's, while `[[@b:Foo]]` goes to B's.
#[tokio::test]
async fn non_markdown_prose_units_are_backlinks_of_a_markdown_target_and_resolve() {
    let a = fixtures::empty_source();
    a.write("Foo.md", "# Foo\n\nThe target.\n");
    a.write("from.txt", "plain text linking to [[Foo]]\n");
    a.write(
        "from.org",
        "* Org\nlinking to [[Foo]] and [[file:x.org][X]]\n",
    );
    a.write("from.rst", "Title\n=====\n\nlinking to [[Foo|alias]]\n");
    a.write("from.adoc", "= Doc\n\nlinking to [[Foo#Heading]]\n");
    a.write(
        "cross.txt",
        "the other source's one: [[@b:Foo]], this one's: [[Foo]]\n",
    );
    let b = fixtures::empty_source();
    b.write("Foo.md", "# Foo in B\n");

    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");
    let token_path = data_dir.path().join("token");
    let token = "c".repeat(64);
    std::fs::write(&token_path, &token).expect("write token");

    let registry = Arc::new(
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
    );
    registry.register("a", a.root()).expect("register a");
    registry.register("b", b.root()).expect("register b");

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let shared_graph = Arc::new(SharedGraph::new(
        UnitGraph::build_from_cache(&graph_cache).expect("build graph"),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
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
        None,
    ));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/rpc");
    let call = |id: u32, method: &'static str, params: serde_json::Value| {
        let client = client.clone();
        let url = url.clone();
        let token = token.clone();
        async move {
            client
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    let backlinks = call(1, "graph.get_backlinks", json!({ "id": "@a:Foo.md" })).await;
    assert!(backlinks.get("error").is_none(), "{backlinks:?}");
    let mut from: Vec<String> = backlinks["result"]
        .as_array()
        .expect("array")
        .iter()
        .map(|e| e["from"].as_str().expect("from id").to_string())
        .collect();
    from.sort();
    assert_eq!(
        from,
        [
            "@a:cross.txt",
            "@a:from.adoc",
            "@a:from.org",
            "@a:from.rst",
            "@a:from.txt"
        ],
        "every non-md prose unit that links to Foo.md, and only those: {backlinks:?}"
    );

    // F7: B's Foo.md is linked from cross.txt only through `@b:Foo`.
    let b_backlinks = call(2, "graph.get_backlinks", json!({ "id": "@b:Foo.md" })).await;
    let b_from: Vec<_> = b_backlinks["result"]
        .as_array()
        .expect("array")
        .iter()
        .map(|e| e["from"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(b_from, ["@a:cross.txt"], "{b_backlinks:?}");

    // `unit.resolve_link` from a `.txt`'s own source resolves.
    let resolved = call(
        3,
        "unit.resolve_link",
        json!({ "link": "[[Foo]]", "default_source": "a" }),
    )
    .await;
    assert!(resolved.get("error").is_none(), "{resolved:?}");
    assert_eq!(resolved["result"]["unit"]["id"], json!("@a:Foo.md"));
    let federated = call(
        4,
        "unit.resolve_link",
        json!({ "link": "[[@b:Foo]]", "default_source": "a" }),
    )
    .await;
    assert_eq!(federated["result"]["unit"]["id"], json!("@b:Foo.md"));

    server.abort();
}

/// PR-13.F1 (G13, G16, GP7, GP9, E8) over the real RPC server: a note
/// links to a block of a code file; `graph.get_backlinks` takes the
/// block id and reports the link's line; the enum then moves to another
/// file through the real watcher and `unit.resolve_link` says `moved`
/// at the `pub enum` line (not the `#[derive]` line), and the backlink
/// is still listed under the new id; a copy in a third file is an
/// ambiguity error listing both places. The RPC itself needs the
/// bearer token (401 without it).
#[tokio::test]
async fn block_links_resolve_and_list_backlinks_over_rpc_and_follow_a_watcher_driven_move() {
    const OLD: &str = "crates/tephrad/src/registry.rs";
    const NEW: &str = "crates/tephrad/src/conflict.rs";
    const ENUM: &str = concat!(
        "/// Why two sources conflict.\n",
        "#[derive(Debug, Clone)]\n",
        "pub enum ConflictReason { // ^k3f9a2\n",
        "    DuplicateName,\n",
        "}\n",
    );

    let vault = fixtures::empty_source();
    vault.write(
        "Tephra test.md",
        "# Tephra test\n\nThe enum: [[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]\n",
    );
    let repo = fixtures::empty_source();
    repo.write(OLD, &format!("pub fn parse_config() {{}}\n\n{ENUM}"));

    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");
    let token_path = data_dir.path().join("token");
    let token = "d".repeat(64);
    std::fs::write(&token_path, &token).expect("write token");

    let registry = Arc::new(
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
    );
    registry
        .register("notes", vault.root())
        .expect("register notes");
    registry
        .register("tephra", repo.root())
        .expect("register tephra");

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let shared_graph = Arc::new(SharedGraph::new(
        UnitGraph::build_from_cache(&graph_cache).expect("build graph"),
    ));
    let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let state = Arc::new(AppState::new(
        token_path,
        shared_graph.clone() as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        Arc::new(tephrad::refactor::RefactorEngine::new(
            shared_graph as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
        )) as Arc<dyn RefactorBackend>,
        Some(graph_refresh.clone()),
    ));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/rpc");
    let call = |id: u32, method: &'static str, params: serde_json::Value| {
        let client = client.clone();
        let url = url.clone();
        let token = token.clone();
        async move {
            client
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    // The bearer token is required.
    let unauthorized = client
        .post(&url)
        .json(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "graph.get_backlinks",
            "params": { "id": format!("@tephra:{OLD}#^k3f9a2") }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    // GP7: the note's link is a backlink of the block, with its line.
    let old_id = format!("@tephra:{OLD}#^k3f9a2");
    let backlinks = call(1, "graph.get_backlinks", json!({ "id": old_id })).await;
    assert!(backlinks.get("error").is_none(), "{backlinks:?}");
    let rows = backlinks["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "{backlinks:?}");
    assert_eq!(rows[0]["from"], json!("@notes:Tephra test.md"));
    assert_eq!(rows[0]["line"], json!(3));
    // The unit id lists the same link; a block nothing links to lists none.
    let unit_rows = call(
        2,
        "graph.get_backlinks",
        json!({ "id": format!("@tephra:{OLD}") }),
    )
    .await;
    assert_eq!(unit_rows["result"].as_array().map(Vec::len), Some(1));
    let none = call(
        3,
        "graph.get_backlinks",
        json!({ "id": format!("@tephra:{OLD}#^zzzzzz") }),
    )
    .await;
    assert_eq!(none["result"], json!([]));

    let link = "[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]";
    let here = call(
        4,
        "unit.resolve_link",
        json!({ "link": link, "default_source": "notes" }),
    )
    .await;
    assert_eq!(
        here["result"]["unit"]["id"],
        json!(format!("@tephra:{OLD}"))
    );
    assert_eq!(
        here["result"]["block"],
        json!({ "id": "k3f9a2", "line": 5, "moved": false })
    );

    // GP9: move the enum to conflict.rs; the watcher picks both up.
    repo.write(OLD, "pub fn parse_config() {}\n");
    repo.write(NEW, ENUM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let moved = loop {
        graph_refresh.refresh().expect("refresh");
        let resolved = call(
            5,
            "unit.resolve_link",
            json!({ "link": link, "default_source": "notes" }),
        )
        .await;
        if resolved["result"]["block"]["moved"] == json!(true) {
            break resolved;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the move was never picked up: {resolved:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(
        moved["result"]["unit"]["id"],
        json!(format!("@tephra:{NEW}"))
    );
    // Line 3 of conflict.rs is `pub enum`, not the `#[derive]` on line 2.
    assert_eq!(
        moved["result"]["block"],
        json!({ "id": "k3f9a2", "line": 3, "moved": true })
    );

    // The note's backlink is listed under the new id, with its line.
    let new_id = format!("@tephra:{NEW}#^k3f9a2");
    let moved_backlinks = call(6, "graph.get_backlinks", json!({ "id": new_id })).await;
    let rows = moved_backlinks["result"].as_array().expect("array");
    assert_eq!(rows.len(), 1, "{moved_backlinks:?}");
    assert_eq!(rows[0]["from"], json!("@notes:Tephra test.md"));
    assert_eq!(rows[0]["line"], json!(3));
    // ...and no longer under the old file's block id.
    let old_backlinks = call(7, "graph.get_backlinks", json!({ "id": old_id })).await;
    assert_eq!(old_backlinks["result"], json!([]));

    // E8: a copy in a third file makes the moved ID ambiguous.
    repo.write("crates/tephrad/src/copy.rs", ENUM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let ambiguous = loop {
        graph_refresh.refresh().expect("refresh");
        let resolved = call(
            8,
            "unit.resolve_link",
            json!({ "link": link, "default_source": "notes" }),
        )
        .await;
        if resolved.get("error").is_some() {
            break resolved;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the copy was never picked up: {resolved:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(ambiguous["error"]["data"]["error"], json!("ambiguous"));
    assert_eq!(
        ambiguous["error"]["data"]["data"]["candidates"],
        json!([
            "@tephra:crates/tephrad/src/conflict.rs",
            "@tephra:crates/tephrad/src/copy.rs"
        ])
    );
    assert_eq!(
        ambiguous["error"]["data"]["data"]["lines"],
        json!([
            { "unit": "@tephra:crates/tephrad/src/conflict.rs", "line": 3 },
            { "unit": "@tephra:crates/tephrad/src/copy.rs", "line": 3 }
        ])
    );

    server.abort();
    registry.unregister("notes").expect("unregister notes");
    registry.unregister("tephra").expect("unregister tephra");
}

/// PR-13.F1 T1 (C19): a source whose root vanishes while the daemon
/// runs is flagged `offline` on block resolution and block backlinks
/// without a restart. The watcher marks the source offline in the cache
/// and announces it on the event stream; `main.rs`'s forwarder answers
/// every such event with `GraphRefresh::refresh`, which this test does
/// too, so the flag has to arrive through the real registry, cache and
/// graph rebuild and be read back over the real RPC server.
#[tokio::test]
async fn a_source_that_goes_offline_while_running_flags_block_results_offline() {
    const FILE: &str = "src/lib.rs";
    let repo = fixtures::empty_source();
    repo.write(FILE, "pub fn parse_config() {} // ^offid1\n");
    let vault = fixtures::empty_source();
    vault.write("note.md", "# Note\n\nSee [[@repo:src/lib.rs#^offid1]]\n");

    let data_dir = tempfile::tempdir().expect("data dir");
    let cache_path = data_dir.path().join("cache.db");
    let token_path = data_dir.path().join("token");
    let token = "e".repeat(64);
    std::fs::write(&token_path, &token).expect("write token");

    let registry = Arc::new(
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
    );
    let mut events = registry.subscribe();
    registry
        .register("notes", vault.root())
        .expect("register notes");
    registry
        .register("repo", repo.root())
        .expect("register repo");

    let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
    let shared_graph = Arc::new(SharedGraph::new(
        UnitGraph::build_from_cache(&graph_cache).expect("build graph"),
    ));
    let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let state = Arc::new(AppState::new(
        token_path,
        shared_graph.clone() as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        Arc::new(tephrad::refactor::RefactorEngine::new(
            shared_graph as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
        )) as Arc<dyn RefactorBackend>,
        Some(graph_refresh.clone()),
    ));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/rpc");
    let call = |id: u32, method: &'static str, params: serde_json::Value| {
        let client = client.clone();
        let url = url.clone();
        let token = token.clone();
        async move {
            client
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let link = "[[@repo:src/lib.rs#^offid1]]";
    let block_id = format!("@repo:{FILE}#^offid1");

    // Online first, so the flag really flips.
    let before = call(
        1,
        "unit.resolve_link",
        json!({ "link": link, "default_source": "notes" }),
    )
    .await;
    assert_eq!(
        before["result"]["block"]["id"],
        json!("offid1"),
        "{before:?}"
    );
    assert_eq!(before["result"]["unit"]["offline"], json!(false));
    let before_rows = call(2, "graph.get_backlinks", json!({ "id": block_id })).await;
    assert_eq!(
        before_rows["result"].as_array().map(Vec::len),
        Some(1),
        "{before_rows:?}"
    );
    assert_eq!(before_rows["result"][0]["offline"], json!(false));

    // The root goes away under the running watcher. Drain the events
    // already queued by the registrations, then wait for the one the
    // offline transition sends and refresh the way the daemon does.
    while events.try_recv().is_ok() {}
    std::fs::remove_dir_all(repo.root()).expect("remove the source root");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match events.try_recv() {
            Ok(tephrad::registry::DaemonEvent::SourceChanged { source, .. })
                if source == "repo" =>
            {
                graph_refresh.refresh().expect("refresh");
                break;
            }
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no source_changed event after the root vanished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let after = call(
        3,
        "unit.resolve_link",
        json!({ "link": link, "default_source": "notes" }),
    )
    .await;
    assert_eq!(after["result"]["block"]["id"], json!("offid1"), "{after:?}");
    assert_eq!(after["result"]["unit"]["offline"], json!(true), "{after:?}");
    let after_rows = call(4, "graph.get_backlinks", json!({ "id": block_id })).await;
    assert_eq!(
        after_rows["result"].as_array().map(Vec::len),
        Some(1),
        "{after_rows:?}"
    );
    assert_eq!(
        after_rows["result"][0]["offline"],
        json!(true),
        "{after_rows:?}"
    );

    server.abort();
    registry.unregister("notes").expect("unregister notes");
    registry.unregister("repo").expect("unregister repo");
}
