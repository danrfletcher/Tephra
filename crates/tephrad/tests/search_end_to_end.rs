//! `unit.search` (G8) end to end, in the style of `rpc_live_graph.rs`:
//! two real temp-dir sources scanned through the real `SourceRegistry`,
//! the real RPC server on an ephemeral loopback port (never the
//! product's port) with a throwaway token, and the real cache.
//!
//! Covers GP4 (the `atlas outside` fixture), the matching edge cases, the
//! F7 fence (a search never changes how a link resolves), F4 (a search
//! writes nothing, in the cache or the sources) and the concurrency
//! cases: a search racing a `source.rescan` and a burst of watcher
//! writes, and `source.unregister`.

mod fixtures;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

const TARGET: &str = "Obsidian Atlas/plans/Atlas outside Obsidian Chrome plugin, VSCode plugin.md";
const TARGET_ID: &str =
    "@notes:Obsidian Atlas/plans/Atlas outside Obsidian Chrome plugin, VSCode plugin.md";

struct Daemon {
    registry: Arc<SourceRegistry>,
    cache_path: std::path::PathBuf,
    client: reqwest::Client,
    url: String,
    token: String,
    server: tokio::task::JoinHandle<()>,
    _data_dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Daemon {
        let data_dir = tempfile::tempdir().expect("data dir");
        let cache_path = data_dir.path().join("cache.db");
        let token_path = data_dir.path().join("token");
        let token = "c".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
        );
        let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
        let shared_graph = Arc::new(SharedGraph::new(
            UnitGraph::build_from_cache(&graph_cache).expect("graph"),
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
            Some(graph_refresh),
        ));
        let router = build_router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Daemon {
            registry,
            cache_path,
            client: reqwest::Client::new(),
            url: format!("http://{addr}/rpc"),
            token,
            server,
            _data_dir: data_dir,
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        self.client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .expect("send")
            .json::<Value>()
            .await
            .expect("json")
    }

    async fn register(&self, name: &str, root: &Path) {
        let reply = self
            .rpc(
                "source.register",
                json!({"name": name, "root": root.to_string_lossy()}),
            )
            .await;
        assert!(reply.get("error").is_none(), "register failed: {reply}");
    }

    async fn search(&self, params: Value) -> Vec<Value> {
        let reply = self.rpc("unit.search", params.clone()).await;
        assert!(reply.get("error").is_none(), "{params}: {reply}");
        reply["result"].as_array().expect("an array").clone()
    }

    async fn search_error(&self, params: Value) -> (i64, String) {
        let reply = self.rpc("unit.search", params).await;
        let err = reply.get("error").unwrap_or_else(|| panic!("{reply}"));
        (
            err["code"].as_i64().unwrap(),
            err["message"].as_str().unwrap().to_string(),
        )
    }

    async fn ids(&self, query: &str) -> Vec<String> {
        ids(&self.search(json!({"query": query, "limit": 1000})).await)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn ids(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect()
}

/// GP4's two sources, plus the edge-case files.
fn fixture_sources() -> (fixtures::TempSource, fixtures::TempSource) {
    let notes = fixtures::empty_source();
    notes.write(TARGET, "# Atlas outside Obsidian\n\nPlan text.\n");
    notes.write("Obsidian Atlas/Atlas.md", "# Atlas\n");
    notes.write("Outside.md", "# Outside\n");
    notes.write("plans/atlas-notes.md", "# Atlas notes\n");
    notes.write("unicode/café notes.md", "# Café\n");
    notes.write("teamA/Shared.md", "# Shared in notes\n");
    notes.write("twins/one/Twin.md", "# Twin\n");
    notes.write("twins/two/Twin.md", "# Twin\n");
    notes.write("odd/100%_done, it's [[x]] @y .*(.md", "text\n");
    notes.write("titled.md", "---\ntitle: Quarterly Review\n---\n\nbody\n");
    notes.write(
        "headed.md",
        "intro line\n\n# The First Heading\n\n## Second\n",
    );
    notes.write("bare.md", "no heading at all\n");
    // Ignored: never units.
    notes.write(".git/atlas-hook.md", "# no\n");
    notes.write("node_modules/atlas/index.md", "# no\n");
    notes.write(".venv/atlas.md", "# no\n");

    let tephra = fixtures::empty_source();
    tephra.write("crates/atlas/outside.rs", "// outside\nfn outside() {}\n");
    tephra.write("teamA/Shared.md", "# Shared in tephra\n");
    tephra.write("Guide.md", "# Guide\n");
    tephra.write("docs/Spec Sheet.pdf", "%PDF-1.4 not really a pdf\n");
    tephra.write_bytes(
        "img/atlas-logo.png",
        &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0],
    );
    tephra.write("data/atlas.csv", "a,b\n1,2\n");
    tephra.write("target/debug/atlas.md", "# built\n");
    (notes, tephra)
}

async fn gp4() -> (Daemon, fixtures::TempSource, fixtures::TempSource) {
    let (notes, tephra) = fixture_sources();
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;
    daemon.register("tephra", tephra.root()).await;
    (daemon, notes, tephra)
}

#[tokio::test]
async fn gp4_atlas_outside_lists_the_plan_first_matched_on_its_name() {
    let (daemon, _notes, _tephra) = gp4().await;
    let hits = daemon.search(json!({"query": "atlas outside"})).await;

    assert_eq!(hits[0]["id"], TARGET_ID);
    assert_eq!(hits[0]["source"], "notes");
    assert_eq!(hits[0]["kind"], "prose");
    assert_eq!(hits[0]["offline"], false);
    // The title is the first heading, not the name.
    assert_eq!(hits[0]["title"], "Atlas outside Obsidian");
    // A prefix of the file name is the second tier: above fuzzy and path
    // matches, below an exact name.
    let score = hits[0]["score"].as_f64().unwrap();
    assert!((0.8..1.0).contains(&score), "score {score}");
    for hit in &hits {
        let keys: BTreeMap<_, _> = hit.as_object().unwrap().iter().collect();
        assert_eq!(
            keys.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            ["id", "kind", "offline", "score", "source", "title"]
        );
    }
    // The decoys are not name matches for "atlas outside" (a space is a
    // character), so none outranks the plan.
    assert!(hits[1..]
        .iter()
        .all(|h| h["score"].as_f64().unwrap() < score));
}

#[tokio::test]
async fn ranking_runs_exact_then_prefix_then_fuzzy_then_path() {
    let (daemon, _notes, _tephra) = gp4().await;

    // "atlas": exact stems (Atlas.md, atlas.csv; the shorter path first),
    // then prefixes of names (atlas-notes.md, atlas-logo.png, the plan),
    // then the path-only match (crates/atlas/outside.rs).
    let hits = daemon
        .search(json!({"query": "atlas", "limit": 1000}))
        .await;
    let order = ids(&hits);
    let pos = |id: &str| {
        order
            .iter()
            .position(|o| o == id)
            .unwrap_or_else(|| panic!("{id} missing from {order:?}"))
    };
    assert_eq!(
        order[..2],
        ["@tephra:data/atlas.csv", "@notes:Obsidian Atlas/Atlas.md"]
    );
    assert_eq!(hits[0]["score"], 1.0);
    assert_eq!(hits[1]["score"], 1.0);
    for prefix in [
        "@notes:plans/atlas-notes.md",
        "@tephra:img/atlas-logo.png",
        TARGET_ID,
    ] {
        assert!(pos(prefix) >= 2, "{prefix}");
        assert!(
            pos(prefix) < pos("@tephra:crates/atlas/outside.rs"),
            "{prefix} ranks above the path-only match"
        );
        let score = hits[pos(prefix)]["score"].as_f64().unwrap();
        assert!((0.8..1.0).contains(&score), "{prefix}: {score}");
    }
    assert!(
        hits[pos("@tephra:crates/atlas/outside.rs")]["score"]
            .as_f64()
            .unwrap()
            < 0.4
    );
    // Scores never rise down the list; equal scores go to the shorter path.
    let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    for pair in hits.windows(2) {
        if pair[0]["score"] == pair[1]["score"] {
            let len = |h: &Value| {
                h["id"]
                    .as_str()
                    .unwrap()
                    .split_once(':')
                    .unwrap()
                    .1
                    .chars()
                    .count()
            };
            assert!(len(&pair[0]) <= len(&pair[1]), "{pair:?}");
        }
    }
    assert!(scores.iter().all(|s| (0.0..=1.0).contains(s)));
}

#[tokio::test]
async fn every_kind_is_searchable_and_ignored_paths_never_appear() {
    let (daemon, _notes, _tephra) = gp4().await;
    let all = daemon.search(json!({"query": ".", "limit": 1000})).await;
    let by_id = |id: &str| {
        all.iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .clone()
    };
    let title_and_kind = |id: &str| {
        let h = by_id(id);
        (
            h["title"].as_str().unwrap().to_string(),
            h["kind"].as_str().unwrap().to_string(),
        )
    };
    let tk = |t: &str, k: &str| (t.to_string(), k.to_string());
    assert_eq!(
        title_and_kind("@notes:titled.md"),
        tk("Quarterly Review", "prose")
    );
    assert_eq!(
        title_and_kind("@notes:headed.md"),
        tk("The First Heading", "prose")
    );
    assert_eq!(title_and_kind("@notes:bare.md"), tk("bare.md", "prose"));
    assert_eq!(
        title_and_kind("@tephra:crates/atlas/outside.rs"),
        tk("outside", "code")
    );
    assert_eq!(
        title_and_kind("@tephra:docs/Spec Sheet.pdf"),
        tk("Spec Sheet", "document")
    );
    assert_eq!(title_and_kind("@tephra:data/atlas.csv").1, "text-other");
    assert_eq!(title_and_kind("@tephra:data/atlas.csv").0, "atlas");
    assert_eq!(
        title_and_kind("@tephra:img/atlas-logo.png"),
        tk("atlas-logo", "binary")
    );

    // Ignored paths, and folders, are never results.
    for hit in &all {
        let id = hit["id"].as_str().unwrap();
        for ignored in [".git/", "node_modules/", ".venv/", "target/"] {
            assert!(!id.contains(ignored), "{id}");
        }
        assert!(!id.ends_with('/'), "{id}");
    }
}

#[tokio::test]
async fn a_file_over_twenty_megabytes_is_searchable_by_name() {
    let notes = fixtures::empty_source();
    notes.write("small.md", "# s\n");
    let big = std::fs::File::create(notes.root().join("huge-dump.bin")).unwrap();
    big.set_len(21 * 1024 * 1024).unwrap();
    drop(big);

    let daemon = Daemon::start().await;
    daemon.register("big", notes.root()).await;
    let hits = daemon.search(json!({"query": "huge"})).await;
    assert_eq!(ids(&hits), ["@big:huge-dump.bin"]);
    assert_eq!(hits[0]["title"], "huge-dump");
    assert_eq!(hits[0]["kind"], "binary");
}

#[tokio::test]
async fn matching_is_case_insensitive_for_unicode() {
    let (daemon, _notes, _tephra) = gp4().await;
    for query in ["café", "CAFÉ", "Café", "cAfÉ"] {
        let found = daemon.ids(query).await;
        assert!(
            found.contains(&"@notes:unicode/café notes.md".to_string()),
            "{query}: {found:?}"
        );
    }
}

#[tokio::test]
async fn queries_are_literal_whatever_characters_they_hold() {
    let (daemon, _notes, _tephra) = gp4().await;
    let odd = "@notes:odd/100%_done, it's [[x]] @y .*(.md".to_string();

    // Found by pieces of its own name, each character meaning itself.
    for query in [
        "100%_done",
        ", it's",
        "[[x]]",
        "@y",
        ".*(",
        "%_",
        "done, it",
        "it's [[",
    ] {
        assert!(daemon.ids(query).await.contains(&odd), "{query:?}");
    }
    // Wildcards and regex metacharacters match nothing they would as a
    // pattern: `%` and `_` are not "anything", `.*` is not "everything".
    for (query, expect_odd) in [("%", true), ("_", true), (".*(", true)] {
        let found = daemon.ids(query).await;
        assert_eq!(found.contains(&odd), expect_odd, "{query:?}");
        assert!(
            found.len() <= 2,
            "{query:?} must not match everything: {found:?}"
        );
    }
    for query in [
        "[",
        "(",
        "a.c",
        "^a",
        "a$",
        "\\",
        "'",
        "\"",
        "; DROP TABLE units",
        "%%",
        "__",
    ] {
        let found = daemon.ids(query).await;
        assert!(
            found.len() < 5,
            "{query:?} leaked as a wildcard or pattern: {found:?}"
        );
    }
    // And the cache is intact afterwards.
    assert!(daemon.ids("atlas").await.len() >= 3);
    // A very long query is a clean error, never a panic or a hang.
    let (code, message) = daemon
        .search_error(json!({"query": "a".repeat(1000)}))
        .await;
    assert_eq!(code, -32602);
    assert!(message.contains("query"), "{message}");
    // A query nothing matches is an empty array.
    assert!(daemon.search(json!({"query": "zzzzqqqq"})).await.is_empty());
}

#[tokio::test]
async fn limit_gives_exactly_that_many_as_the_top_of_the_unlimited_list() {
    let (daemon, _notes, _tephra) = gp4().await;
    let all = daemon.ids("a").await;
    assert!(all.len() > 10, "{all:?}");
    for limit in [1usize, 2, 5, 10] {
        let some = ids(&daemon.search(json!({"query": "a", "limit": limit})).await);
        assert_eq!(some.len(), limit);
        assert_eq!(some, all[..limit]);
    }
    // The default is 50: with fewer than 50 matches, all of them.
    let default = ids(&daemon.search(json!({"query": "a"})).await);
    assert_eq!(default, all[..all.len().min(50)]);
}

#[tokio::test]
async fn bad_params_are_refused_and_name_a_scope_that_is_not_built_yet() {
    let (daemon, _notes, _tephra) = gp4().await;
    for scope in ["headings", "blocks", "all_blocks", "nonsense"] {
        let (code, message) = daemon
            .search_error(json!({"query": "atlas", "scope": scope}))
            .await;
        assert_eq!(code, -32602, "{scope}");
        assert!(message.contains(scope), "{scope}: {message}");
    }
    for params in [
        json!({"query": ""}),
        json!({"query": "   "}),
        json!({"query": "a", "limit": 0}),
        json!({"query": "a", "limit": -3}),
        json!({"query": "a", "limit": 2.5}),
        json!({"query": "a", "limit": "10"}),
    ] {
        assert_eq!(
            daemon.search_error(params.clone()).await.0,
            -32602,
            "{params}"
        );
    }
    // A missing scope is `names`.
    assert_eq!(
        daemon.search(json!({"query": "atlas outside"})).await,
        daemon
            .search(json!({"query": "atlas outside", "scope": "names"}))
            .await
    );
}

#[tokio::test]
async fn the_same_rel_path_in_two_sources_is_two_results_in_a_fixed_order() {
    let (daemon, _notes, _tephra) = gp4().await;
    let first = daemon.search(json!({"query": "shared"})).await;
    assert_eq!(
        ids(&first),
        ["@notes:teamA/Shared.md", "@tephra:teamA/Shared.md"]
    );
    assert_ne!(first[0]["source"], first[1]["source"]);
    assert_eq!(first[0]["title"], "Shared in notes");
    assert_eq!(first[1]["title"], "Shared in tephra");
    for _ in 0..10 {
        assert_eq!(daemon.search(json!({"query": "shared"})).await, first);
    }
}

#[tokio::test]
async fn an_offline_source_is_still_searched_and_flagged() {
    let (daemon, notes, _tephra) = gp4().await;
    let before = daemon.search(json!({"query": "atlas outside"})).await;
    assert_eq!(before[0]["offline"], false);

    // Take the source offline: rename its root away.
    let moved = notes.root().with_extension("moved");
    std::fs::rename(notes.root(), &moved).expect("rename root");
    let during = daemon.search(json!({"query": "atlas outside"})).await;
    assert_eq!(
        during[0]["id"], TARGET_ID,
        "offline units are never dropped"
    );
    assert_eq!(during[0]["offline"], true);
    // Only that source is offline.
    let tephra_hits = daemon.search(json!({"query": "Guide"})).await;
    assert_eq!(tephra_hits[0]["id"], "@tephra:Guide.md");
    assert_eq!(tephra_hits[0]["offline"], false);
    std::fs::rename(&moved, notes.root()).expect("restore root");
}

#[tokio::test]
async fn a_search_never_changes_how_a_link_resolves() {
    let (daemon, _notes, _tephra) = gp4().await;
    let resolve = |link: &'static str, default: Option<&'static str>| {
        let mut params = json!({"link": link});
        if let Some(d) = default {
            params["default_source"] = json!(d);
        }
        daemon.rpc("unit.resolve_link", params)
    };

    // `Guide.md` is only in `tephra`: from a `notes` unit the bare link
    // does not resolve ...
    let guide_before = resolve("[[Guide]]", Some("notes")).await;
    assert!(guide_before.get("error").is_some(), "{guide_before}");
    // ... and a tie is the ambiguity error, never the top search hit.
    let twin_before = resolve("[[Twin]]", Some("notes")).await;
    assert!(
        twin_before["error"]["data"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("ambig")),
        "two files named Twin in one source tie: {twin_before}"
    );
    let atlas_before = resolve("[[Atlas]]", Some("notes")).await;

    // Searching for exactly those names puts `@tephra:Guide.md` at the
    // top of the results ...
    let found = daemon.search(json!({"query": "Guide"})).await;
    assert_eq!(found[0]["id"], "@tephra:Guide.md");
    let twins = daemon.search(json!({"query": "Twin"})).await;
    assert_eq!(twins.len(), 2);
    daemon.search(json!({"query": "Atlas"})).await;

    // ... and it changes nothing.
    assert_eq!(resolve("[[Guide]]", Some("notes")).await, guide_before);
    assert_eq!(resolve("[[Twin]]", Some("notes")).await, twin_before);
    assert_eq!(resolve("[[Atlas]]", Some("notes")).await, atlas_before);
    assert_eq!(
        resolve("[[@tephra:Guide]]", Some("notes")).await["result"]["unit"]["id"],
        "@tephra:Guide.md"
    );
}

/// Every file under `root`: its mtime and a hash of its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, (std::time::SystemTime, u64)> {
    use std::hash::{Hash, Hasher};
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (std::time::SystemTime, u64)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::fs::read(&path).unwrap().hash(&mut hasher);
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    (
                        path.metadata().unwrap().modified().unwrap(),
                        hasher.finish(),
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[tokio::test]
async fn a_hundred_searches_change_no_file_and_no_cache_row() {
    let (daemon, notes, tephra) = gp4().await;
    let before = (snapshot(notes.root()), snapshot(tephra.root()));
    let rows = |path: &Path| -> Vec<String> {
        let cache = Cache::open_read_only(path).unwrap();
        let mut stmt = cache
            .connection()
            .prepare(
                "SELECT s.name || ':' || u.rel_path || ':' || u.kind || ':' || COALESCE(u.mtime, '') \
                 FROM units u JOIN sources s ON s.id = u.source_id ORDER BY 1",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let cache_before = rows(&daemon.cache_path);

    for i in 0..100 {
        let query = ["atlas outside", "guide", "café", "%", "outside"][i % 5];
        daemon.search(json!({"query": query})).await;
    }

    assert_eq!((snapshot(notes.root()), snapshot(tephra.root())), before);
    assert_eq!(rows(&daemon.cache_path), cache_before);
}

#[tokio::test]
async fn after_unregister_that_sources_units_no_longer_appear() {
    let (daemon, _notes, _tephra) = gp4().await;
    assert!(daemon
        .ids("shared")
        .await
        .contains(&"@tephra:teamA/Shared.md".to_string()));
    let reply = daemon
        .rpc("source.unregister", json!({"name": "tephra"}))
        .await;
    assert!(reply.get("error").is_none(), "{reply}");
    let after = daemon.ids("a").await;
    assert!(
        after.iter().all(|id| id.starts_with("@notes:")),
        "{after:?}"
    );
    assert_eq!(daemon.ids("shared").await, ["@notes:teamA/Shared.md"]);
}

#[tokio::test]
async fn searching_during_a_rescan_and_a_burst_of_watcher_writes_neither_errors_nor_duplicates() {
    let notes = fixtures::empty_source();
    for i in 0..1_200 {
        notes.write(&format!("batch/note-{i:04}.md"), &format!("# Note {i}\n"));
    }
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;

    let stop = Arc::new(AtomicBool::new(false));
    let registry = daemon.registry.clone();

    // A rescan loop (the writer) ...
    let rescans = {
        let registry = registry.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut done = 0;
            while !stop.load(Ordering::Relaxed) {
                registry.rescan("notes").expect("rescan");
                done += 1;
            }
            done
        })
    };
    // ... and the watcher's burst: real files appearing and vanishing.
    let root = notes.root().to_path_buf();
    let burst = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                for k in 0..20 {
                    let _ =
                        std::fs::write(root.join(format!("burst-{}.md", (i + k) % 50)), "# b\n");
                }
                for k in 0..10 {
                    let _ = std::fs::remove_file(root.join(format!("burst-{}.md", (i + k) % 50)));
                }
                i += 7;
            }
        })
    };

    // Searches on this thread, through the RPC and through the registry.
    let started = std::time::Instant::now();
    let mut searches = 0;
    while searches < 40 || (started.elapsed() < std::time::Duration::from_secs(3) && searches < 400)
    {
        let hits = registry
            .search_names("note", 1000)
            .unwrap_or_else(|e| panic!("search {searches} failed mid-rescan: {e:?}"));
        let mut seen = HashSet::new();
        for hit in &hits {
            assert!(
                seen.insert(hit.id.clone()),
                "{} twice in one result",
                hit.id
            );
        }
        searches += 1;
        if searches % 10 == 0 {
            let reply = daemon
                .rpc("unit.search", json!({"query": "burst", "limit": 100}))
                .await;
            assert!(reply.get("error").is_none(), "{reply}");
            let ids = ids(reply["result"].as_array().unwrap());
            assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
        }
    }
    stop.store(true, Ordering::Relaxed);
    let rescans = rescans.join().unwrap();
    burst.join().unwrap();
    assert!(rescans >= 1, "the rescan loop never completed one rescan");
}

/// The search must not take the writer gate: its source holds no
/// `writer_gate` reference at all. (The gate itself is private to the
/// crate, so this is checked structurally, over the code and not the
/// comments; the behavioural half, a search finishing while a writer
/// holds the database lock, is `tephra-cache`'s `search_names.rs`.)
#[test]
fn the_search_code_never_touches_the_writer_gate_or_the_shared_cache_lock() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut code = String::new();
    for file in ["search/mod.rs", "search/fuzzy.rs"] {
        code.push_str(&strip_comments(
            &std::fs::read_to_string(src.join(file)).unwrap(),
        ));
    }
    // The registry method: from its signature to the next `pub fn`.
    let registry = std::fs::read_to_string(src.join("registry.rs")).unwrap();
    let start = registry
        .find("pub fn search_names")
        .expect("registry.search_names");
    let rest = &registry[start + 1..];
    let end = rest.find("pub fn ").unwrap_or(rest.len());
    code.push_str(&strip_comments(&rest[..end]));

    for forbidden in [
        "writer_gate",
        "WriterGate",
        "acquire(",
        "self.cache.",
        "BEGIN",
        "execute(",
        "Cache::open(",
    ] {
        assert!(
            !code.contains(forbidden),
            "search code mentions `{forbidden}`"
        );
    }
    assert!(
        code.contains("open_read_only"),
        "search must use a read-only connection"
    );
}

fn strip_comments(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
