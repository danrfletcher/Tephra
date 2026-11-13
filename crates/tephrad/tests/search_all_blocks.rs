//! `unit.search` with `scope: "all_blocks"` (G10, the `^^` search) end to
//! end, in the style of `search_end_to_end.rs`: real temp-dir sources
//! scanned through the real `SourceRegistry`, the real RPC server on an
//! ephemeral loopback port (never the product's port) with a throwaway
//! token, and the real cache.
//!
//! Covers every file kind being searchable (prose, code symbols and
//! comments, text-other, documents), a substring matching inside a word,
//! block IDs, the edit / delete / rescan / unregister lifecycle of the
//! index, offline sources, the files that must give nothing (binary,
//! too large), the hostile-query cases, F4 (a search writes nothing) and
//! a search racing a rescan.

mod fixtures;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::file_kind::MAX_INDEXED_BYTES;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

fn doc_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-parser/tests/fixtures/documents")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Daemon {
    async fn blocks(&self, query: &str) -> Vec<Value> {
        self.search(json!({"query": query, "scope": "all_blocks", "limit": 200}))
            .await
    }
}

/// `unit` of each hit, in order.
fn units(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .map(|h| h["unit"].as_str().unwrap().to_string())
        .collect()
}

fn hit<'a>(hits: &'a [Value], unit: &str, kind: &str) -> &'a Value {
    hits.iter()
        .find(|h| h["unit"] == unit && h["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} hit in {unit}: {hits:#?}"))
}

fn sources() -> (fixtures::TempSource, fixtures::TempSource) {
    let notes = fixtures::empty_source();
    notes.write(
        "plans/links.md",
        "# Link plans\n\nHow a link conflict is shown to the user. ^k3f9a2\n\nAn unrelated paragraph.\n",
    );
    notes.write(
        "plans/dup.md",
        "First repeated. ^dup\n\nSecond repeated. ^dup\n",
    );
    notes.write(
        "todo.txt",
        "buy milk\nand eggs\n\nthe conflict of the day\n",
    );
    notes.write_bytes("docs/report.docx", &doc_fixture("headings.docx"));
    notes.write_bytes("docs/deck.pptx", &doc_fixture("deck.pptx"));
    notes.write_bytes("docs/text.pdf", &doc_fixture("text.pdf"));
    // Nothing to find in these, whatever they hold.
    notes.write_bytes("blob.bin", b"conflict\0conflict");
    notes.write_bytes("scan.png", b"\x89PNG conflict \0 conflict");

    let repo = fixtures::empty_source();
    repo.write(
        "src/lib.rs",
        "// Decides what to do about a conflict.\n// Second line of the note.\n\
         pub enum ConflictReason {\n    Same,\n}\n\n\
         const LABEL: &str = \"conflict // not a comment\";\n\
         fn walk_and_write() {}\n",
    );
    repo.write(
        "tool.py",
        "# python conflict note\ndef run():\n    \"\"\"Docstring about conflict.\"\"\"\n",
    );
    (notes, repo)
}

async fn world() -> (Daemon, fixtures::TempSource, fixtures::TempSource) {
    let (notes, repo) = sources();
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;
    daemon.register("repo", repo.root()).await;
    (daemon, notes, repo)
}

#[tokio::test]
async fn every_kind_of_file_is_searchable_by_its_text() {
    let (daemon, _notes, _repo) = world().await;
    let hits = daemon.blocks("conflict").await;

    let p = hit(&hits, "@notes:plans/links.md", "paragraph");
    assert_eq!(
        (p["line"].as_i64(), p["block_id"].as_str()),
        (Some(3), Some("k3f9a2"))
    );
    assert!(p["snippet"].as_str().unwrap().contains("link conflict"));
    assert_eq!(p["source"], "notes");
    assert_eq!(p["offline"], false);

    let t = hit(&hits, "@notes:todo.txt", "paragraph");
    assert_eq!(t["line"], 4);
    assert!(t.get("block_id").is_none());

    // Code: a symbol by a word of its name, a comment block, a docstring
    // and a hash comment; a string literal is none of them.
    let sym = hit(&hits, "@repo:src/lib.rs", "symbol");
    assert_eq!(sym["snippet"], "enum ConflictReason");
    assert_eq!(sym["line"], 3);
    let comment = hit(&hits, "@repo:src/lib.rs", "comment");
    assert_eq!(comment["line"], 1);
    assert!(comment["snippet"]
        .as_str()
        .unwrap()
        .contains("Decides what"));
    let py: Vec<&Value> = hits
        .iter()
        .filter(|h| h["unit"] == "@repo:tool.py")
        .collect();
    assert_eq!(py.len(), 2, "{py:?}");
    assert!(
        hits.iter()
            .filter(|h| h["unit"] == "@repo:src/lib.rs" && h["kind"] == "comment")
            .all(|h| h["line"] != 7),
        "the string literal is not a comment"
    );
    assert_eq!(
        hits.iter()
            .filter(|h| h["unit"] == "@repo:src/lib.rs")
            .count(),
        2,
        "the symbol and the one comment block: the literal adds nothing"
    );

    // Binary and name-only files give nothing.
    let all = units(&hits);
    assert!(
        !all.iter()
            .any(|u| u.ends_with("blob.bin") || u.ends_with("scan.png")),
        "{all:?}"
    );
}

#[tokio::test]
async fn a_part_of_a_word_matches_and_a_symbols_name_is_split() {
    let (daemon, _notes, _repo) = world().await;
    for (query, want) in [
        ("flictRea", "@repo:src/lib.rs"),
        ("conflict reason", "@repo:src/lib.rs"),
        ("walk and write", "@repo:src/lib.rs"),
        ("alk_and", "@repo:src/lib.rs"),
        ("ilk", "@notes:todo.txt"),
    ] {
        let hits = daemon.blocks(query).await;
        assert!(
            units(&hits).contains(&want.to_string()),
            "{query:?}: {hits:#?}"
        );
    }
    // Case does not matter.
    assert_eq!(
        units(&daemon.blocks("CONFLICTREASON").await),
        units(&daemon.blocks("conflictreason").await)
    );
}

#[tokio::test]
async fn documents_are_found_by_page_slide_and_paragraph_and_never_have_a_block_id() {
    let (daemon, _notes, _repo) = world().await;
    let hits = daemon.blocks("Agenda").await;
    let slide = hit(&hits, "@notes:docs/deck.pptx", "document paragraph");
    assert_eq!(slide["slide"], 2);
    assert!(slide.get("line").is_none() && slide.get("block_id").is_none());

    let hits = daemon.blocks("Chapter two").await;
    let page = hit(&hits, "@notes:docs/text.pdf", "document paragraph");
    assert_eq!(page["page"], 2);

    let hits = daemon.blocks("Details").await;
    let para = hit(&hits, "@notes:docs/report.docx", "document paragraph");
    assert_eq!(para["paragraph"], 4);
}

#[tokio::test]
async fn duplicate_ids_in_one_file_are_each_their_own_hit() {
    let (daemon, _notes, _repo) = world().await;
    let hits = daemon.blocks("repeated").await;
    let got: Vec<(i64, &str)> = hits
        .iter()
        .filter(|h| h["unit"] == "@notes:plans/dup.md")
        .map(|h| (h["line"].as_i64().unwrap(), h["block_id"].as_str().unwrap()))
        .collect();
    assert_eq!(got, [(1, "dup"), (3, "dup")]);
}

#[tokio::test]
async fn the_index_follows_an_edit_a_delete_a_rescan_and_an_unregister() {
    let (daemon, notes, repo) = world().await;
    assert!(daemon.blocks("zebra").await.is_empty());

    // Edit: through a rescan (the watcher's batch path is covered in
    // `block_index_scan.rs`).
    notes.write("todo.txt", "buy milk\n\na zebra crossing\n");
    daemon.registry.rescan("notes").unwrap();
    let hits = daemon.blocks("zebra").await;
    assert_eq!(units(&hits), ["@notes:todo.txt"]);
    assert!(
        daemon.blocks("eggs").await.is_empty(),
        "the old text is gone, with no duplicate rows"
    );
    // Two rescans in a row leave one row, not two.
    daemon.registry.rescan("notes").unwrap();
    assert_eq!(daemon.blocks("zebra").await.len(), 1);

    // Delete.
    notes.remove("todo.txt");
    daemon.registry.rescan("notes").unwrap();
    assert!(daemon.blocks("zebra").await.is_empty());

    // Unregister: that source's rows go, the other source's stay.
    daemon.registry.unregister("notes").unwrap();
    assert!(daemon.blocks("repeated").await.is_empty());
    assert!(!daemon.blocks("ConflictReason").await.is_empty());
    let _ = repo;
}

/// A file renamed on disk, then rescanned: the daemon finds the text only
/// under the new path, once, and still finds the other files.
#[tokio::test]
async fn a_renamed_file_is_found_only_under_its_new_path() {
    let (daemon, notes, _repo) = world().await;
    notes.write("plans/moving.md", "# Moving\n\nthe quokka paragraph\n");
    daemon.registry.rescan("notes").unwrap();
    assert_eq!(
        units(&daemon.blocks("quokka").await),
        ["@notes:plans/moving.md"]
    );

    std::fs::rename(
        notes.root().join("plans/moving.md"),
        notes.root().join("plans/moved.md"),
    )
    .unwrap();
    daemon.registry.rescan("notes").unwrap();
    let hits = daemon.blocks("quokka").await;
    assert_eq!(units(&hits), ["@notes:plans/moved.md"], "{hits:?}");
    assert_eq!(hits.len(), 1, "one set of rows, not two");
    assert!(!daemon.blocks("ConflictReason").await.is_empty());
}

#[tokio::test]
async fn an_offline_source_is_still_searched_and_flagged() {
    let (daemon, notes, _repo) = world().await;
    let moved = notes.root().with_extension("moved");
    std::fs::rename(notes.root(), &moved).expect("rename root");
    let hits = daemon.blocks("conflict").await;
    let notes_hit = hit(&hits, "@notes:plans/links.md", "paragraph");
    assert_eq!(notes_hit["offline"], true);
    let repo_hit = hit(&hits, "@repo:src/lib.rs", "symbol");
    assert_eq!(repo_hit["offline"], false);
    std::fs::rename(&moved, notes.root()).expect("restore root");
}

#[tokio::test]
async fn a_file_over_the_size_cap_gives_no_rows() {
    let notes = fixtures::empty_source();
    notes.write("small.md", "tiny needle\n");
    // A sparse 20 MiB + 1 file that starts like text: never read.
    let big = notes.root().join("big.md");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(MAX_INDEXED_BYTES + 1).unwrap();
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;
    assert_eq!(units(&daemon.blocks("needle").await), ["@notes:small.md"]);
    // It is still searchable by name (G8), as before.
    let by_name = daemon.search(json!({"query": "big"})).await;
    assert_eq!(by_name[0]["id"], "@notes:big.md");
}

#[tokio::test]
async fn hostile_queries_are_text_never_syntax_and_never_an_error() {
    let notes = fixtures::empty_source();
    notes.write(
        "a.md",
        "say \"hi\" now\n\nleft AND right NEAR(x) OR y\n\nstar* and col:umn -minus ^caret\n",
    );
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;
    for query in [
        "\"hi\"",
        "AND right",
        "NEAR(x)",
        "OR y",
        "star*",
        "col:umn",
        "-minus",
        "^caret",
        "\"",
        "\"\"",
        "*",
        "(",
        "NEAR(",
        "a AND",
        ":",
        "unit_id:1",
        "text:x",
        "-",
        "^",
        "%",
        "_",
        "'; DROP TABLE units; --",
        "\\",
    ] {
        let reply = daemon
            .rpc(
                "unit.search",
                json!({"query": query, "scope": "all_blocks"}),
            )
            .await;
        assert!(reply.get("error").is_none(), "{query:?}: {reply}");
    }
    assert_eq!(units(&daemon.blocks("AND right").await), ["@notes:a.md"]);
    assert_eq!(units(&daemon.blocks("col:umn").await), ["@notes:a.md"]);
    // The tables are all still there.
    assert_eq!(daemon.blocks("hi").await.len(), 1);
}

#[tokio::test]
async fn bad_params_are_refused_with_the_scope_named() {
    let (daemon, _notes, _repo) = world().await;
    for (params, needle) in [
        (json!({"query": "", "scope": "all_blocks"}), "empty query"),
        (
            json!({"query": "  \t", "scope": "all_blocks"}),
            "empty query",
        ),
        (json!({"scope": "all_blocks"}), "query"),
        (
            json!({"query": "x", "scope": "all_blocks", "limit": 0}),
            "limit",
        ),
        (
            json!({"query": "x", "scope": "all_blocks", "limit": 1_000_000}),
            "limit",
        ),
        (
            json!({"query": "x", "scope": "all_blocks", "in": "@notes:todo.txt"}),
            "all_blocks",
        ),
        (
            json!({"query": "x".repeat(5_000), "scope": "all_blocks"}),
            "longer than",
        ),
    ] {
        let (code, message) = daemon.search_error(params.clone()).await;
        assert_eq!(code, -32602, "{params}: {message}");
        assert!(message.contains(needle), "{message}");
    }
}

#[tokio::test]
async fn limit_gives_the_top_of_the_unlimited_list_and_the_order_is_repeatable() {
    let (daemon, _notes, _repo) = world().await;
    let all = daemon.blocks("conflict").await;
    assert!(all.len() >= 6, "{all:#?}");
    for n in [1, 3, all.len()] {
        let some = daemon
            .search(json!({"query": "conflict", "scope": "all_blocks", "limit": n}))
            .await;
        assert_eq!(some, all[..n].to_vec());
    }
    assert_eq!(daemon.blocks("conflict").await, all);
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// F4: a search changes no file (not even by a byte) and no cache row.
#[tokio::test]
async fn a_hundred_searches_change_no_file_and_no_cache_row() {
    let (daemon, notes, repo) = world().await;
    let notes_before = snapshot(notes.root());
    let repo_before = snapshot(repo.root());
    let count = |table: &str| {
        Cache::open(&daemon.cache_path)
            .unwrap()
            .table_row_count(table)
            .unwrap()
    };
    let rows_before: Vec<i64> = [
        "units",
        "blocks",
        "headings",
        "symbols",
        "links",
        "block_fts",
    ]
    .iter()
    .map(|t| count(t))
    .collect();
    for i in 0..100 {
        let q = ["conflict", "ConflictReason", "^^", "\"x", "zz"][i % 5];
        let _ = daemon
            .rpc("unit.search", json!({"query": q, "scope": "all_blocks"}))
            .await;
    }
    assert_eq!(snapshot(notes.root()), notes_before);
    assert_eq!(snapshot(repo.root()), repo_before);
    let rows_after: Vec<i64> = [
        "units",
        "blocks",
        "headings",
        "symbols",
        "links",
        "block_fts",
    ]
    .iter()
    .map(|t| count(t))
    .collect();
    assert_eq!(rows_after, rows_before);
    assert!(rows_before[5] > 0, "the index is not empty");
}

#[tokio::test]
async fn searching_during_a_rescan_neither_errors_nor_sees_a_half_written_file() {
    let notes = fixtures::empty_source();
    for i in 0..300 {
        notes.write(
            &format!("batch/note-{i:03}.md"),
            &format!("# Note {i}\n\nmarker paragraph {i}\n"),
        );
    }
    let daemon = Daemon::start().await;
    daemon.register("notes", notes.root()).await;
    let stop = Arc::new(AtomicBool::new(false));
    let registry = daemon.registry.clone();
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
    let started = std::time::Instant::now();
    let mut searches = 0;
    while searches < 30 || (started.elapsed() < std::time::Duration::from_secs(3) && searches < 300)
    {
        let hits = registry
            .search_all_blocks("marker paragraph", 1000)
            .unwrap_or_else(|e| panic!("search {searches} failed mid-rescan: {e:?}"));
        // One consistent snapshot: no unit twice, and a unit that shows
        // has its own row (it is never a row of a deleted unit).
        let mut seen = std::collections::HashSet::new();
        for h in &hits {
            assert!(
                seen.insert(h.unit.clone()),
                "{} twice in one result",
                h.unit
            );
        }
        searches += 1;
    }
    stop.store(true, Ordering::Relaxed);
    assert!(rescans.join().unwrap() >= 1);
    // `blocks` asks for at most 200: 300 units match, 200 come back.
    assert_eq!(daemon.blocks("marker paragraph").await.len(), 200);
}

/// The search must not take the writer gate or the shared cache lock:
/// checked structurally, over the code and not the comments (the
/// behavioural half is the test above, a search finishing mid-rescan).
#[test]
fn the_all_blocks_code_never_touches_the_writer_gate_or_the_shared_cache_lock() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut code =
        strip_comments(&std::fs::read_to_string(src.join("search/all_blocks.rs")).unwrap());
    let registry = std::fs::read_to_string(src.join("registry.rs")).unwrap();
    let start = registry
        .find("pub fn search_all_blocks")
        .expect("registry.search_all_blocks");
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
            "all_blocks code mentions `{forbidden}`"
        );
    }
    assert!(
        code.contains("open_read_only"),
        "must use a read-only connection"
    );
}

fn strip_comments(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
