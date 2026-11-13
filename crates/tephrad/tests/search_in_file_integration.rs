//! `unit.search` inside one file (G9) end to end, in the style of
//! `search_end_to_end.rs`: a real temp-dir source with notes, code and
//! documents, scanned by the real `SourceRegistry`, the real RPC server
//! on an ephemeral loopback port (never the product's port) with a
//! throwaway token, and the real cache. Every request goes through HTTP
//! with the bearer token.
//!
//! Covers G9, G6, G7, E4, E7, E12, E13, F3, F4, F7 and GP5. The stale-
//! file cases scan with no watcher running (`tephrad::scan` into a bare
//! cache), so the file can change under an unchanged cache row
//! deterministically.

mod fixtures;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

struct Daemon {
    cache_path: PathBuf,
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
        let token = "d".repeat(64);
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

    /// An in-file search that must succeed: the whole result object.
    async fn search(&self, scope: &str, unit: &str, query: &str) -> Value {
        let params = json!({"query": query, "scope": scope, "in": unit});
        let reply = self.rpc("unit.search", params.clone()).await;
        assert!(reply.get("error").is_none(), "{params}: {reply}");
        reply["result"].clone()
    }

    async fn results(&self, scope: &str, unit: &str, query: &str) -> Vec<Value> {
        self.search(scope, unit, query).await["results"]
            .as_array()
            .expect("results array")
            .clone()
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

fn doc_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tephra-parser/tests/fixtures/documents")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn field<'a>(hit: &'a Value, key: &str) -> &'a str {
    hit[key]
        .as_str()
        .unwrap_or_else(|| panic!("no string {key} in {hit}"))
}

fn lines(hits: &[Value]) -> Vec<i64> {
    hits.iter().map(|h| h["line"].as_i64().unwrap()).collect()
}

fn texts(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .map(|h| {
            h["text"]
                .as_str()
                .or(h["name"].as_str())
                .unwrap()
                .to_string()
        })
        .collect()
}

const A_MD: &str = "\
---
title: Alpha
---

# Alpha plan

First paragraph about alpha. ^first

## Café notes

A paragraph with café and 日本語 in it.
It runs over two lines.

- a list item ^dup

### Deep heading

Another paragraph. ^dup

Plain paragraph with no id.
";

const B_MD: &str = "# Beta\n\nBeta paragraph.\n\n## Alpha in beta\n";

const BROKEN_TS: &str = "\
export function good(): number {
  return 1;
}

export class Fine {
  method() {}
}

function broken( {
";

const DECORATED_PY: &str = "\
import functools


@functools.cache
@staticmethod
def decorated(x):
    return x


class Thing:
    @property
    def prop(self):
        return 1
";

/// The fixture source: `notes/`, `code/`, `docs/` and the kinds that
/// have nothing to search inside.
fn fixture_source() -> fixtures::TempSource {
    let src = fixtures::empty_source();
    src.write("notes/a.md", A_MD);
    src.write("notes/b.md", B_MD);
    src.write(
        "code/registry.rs",
        &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/registry.rs"))
            .expect("read registry.rs"),
    );
    src.write("code/decorated.py", DECORATED_PY);
    src.write("code/broken.ts", BROKEN_TS);
    src.write_bytes("docs/report.docx", &doc_fixture("headings.docx"));
    src.write_bytes("docs/deck.pptx", &doc_fixture("deck.pptx"));
    src.write_bytes("docs/text.pdf", &doc_fixture("text.pdf"));
    src.write_bytes("docs/scanned.pdf", &doc_fixture("no-text.pdf"));
    src.write_bytes("docs/truncated.docx", &doc_fixture("truncated.docx"));
    src.write("data/table.csv", "a,b\n1,2\n");
    src.write_bytes("img/logo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0]);
    // 20 MiB + 1 byte, sparse: indexed by name only (G7).
    let big = src.root().join("notes/huge.md");
    let file = std::fs::File::create(&big).expect("create big");
    file.set_len(tephrad::file_kind::MAX_INDEXED_BYTES + 1)
        .expect("set_len");
    // Sixty headings: more than the default limit.
    let many: String = (1..=60).map(|n| format!("# Heading {n}\n\n")).collect();
    src.write("notes/many.md", &many);
    src
}

async fn world() -> (Daemon, fixtures::TempSource) {
    let src = fixture_source();
    let daemon = Daemon::start().await;
    daemon.register("fx", src.root()).await;
    (daemon, src)
}

fn line_of(file: &str, needle: &str) -> i64 {
    file.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle} not in file")) as i64
        + 1
}

// ---------------------------------------------------------------- G9

#[tokio::test]
async fn headings_come_from_that_one_file_with_unit_kind_text_level_and_line() {
    let (daemon, _src) = world().await;
    let hits = daemon.results("headings", "@fx:notes/a.md", "").await;
    assert_eq!(
        hits,
        vec![
            json!({"unit": "@fx:notes/a.md", "kind": "heading", "text": "Alpha plan", "level": 1, "line": 5}),
            json!({"unit": "@fx:notes/a.md", "kind": "heading", "text": "Café notes", "level": 2, "line": 9}),
            json!({"unit": "@fx:notes/a.md", "kind": "heading", "text": "Deep heading", "level": 3, "line": 16}),
        ]
    );
    // `alpha` also matches a heading of b.md, which is never returned.
    let hits = daemon.results("headings", "@fx:notes/a.md", "alpha").await;
    assert_eq!(texts(&hits), ["Alpha plan"]);
    let hits = daemon.results("headings", "@fx:notes/b.md", "alpha").await;
    assert_eq!(texts(&hits), ["Alpha in beta"]);
}

#[tokio::test]
async fn unicode_headings_match_and_keep_their_bytes() {
    let (daemon, _src) = world().await;
    for query in ["café", "CAFÉ", "caf"] {
        let hits = daemon.results("headings", "@fx:notes/a.md", query).await;
        assert_eq!(texts(&hits), ["Café notes"], "{query}");
        assert_eq!(hits[0]["line"], 9);
    }
    let hits = daemon.results("blocks", "@fx:notes/a.md", "日本語").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(
        field(&hits[0], "text"),
        "A paragraph with café and 日本語 in it.\nIt runs over two lines."
    );
    assert_eq!(hits[0]["line"], 11);
}

#[tokio::test]
async fn prose_blocks_are_paragraphs_with_text_line_and_an_existing_id_only() {
    let (daemon, _src) = world().await;
    let hits = daemon.results("blocks", "@fx:notes/a.md", "").await;
    let by_line: Vec<(i64, Option<&str>)> = hits
        .iter()
        .map(|h| (h["line"].as_i64().unwrap(), h["block_id"].as_str()))
        .collect();
    assert_eq!(
        by_line,
        [
            (7, Some("first")),
            (11, None),
            (14, Some("dup")),
            (18, Some("dup")),
            (20, None)
        ]
    );
    for hit in &hits {
        assert_eq!(hit["kind"], "paragraph");
        assert_eq!(hit["unit"], "@fx:notes/a.md");
        // A paragraph with no ID has no `block_id` key at all (F3).
        if hit["block_id"].is_null() {
            assert!(hit.get("block_id").is_none(), "{hit}");
        }
    }
    // The marker is `block_id`, not part of the text; a client that
    // shows both would print it twice.
    assert_eq!(field(&hits[0], "text"), "First paragraph about alpha.");
    for hit in &hits {
        assert!(!field(hit, "text").contains('^'), "{hit}");
    }
    assert_eq!(field(&hits[4], "text"), "Plain paragraph with no id.");
    let found = daemon.results("blocks", "@fx:notes/a.md", "plain").await;
    assert_eq!(lines(&found), [20]);
}

/// E7: both occurrences of a duplicate ID are listed, each at its own
/// line; search never collapses them.
#[tokio::test]
async fn duplicate_ids_are_listed_once_per_occurrence() {
    let (daemon, _src) = world().await;
    let hits = daemon.results("blocks", "@fx:notes/a.md", "dup").await;
    // The query is matched against the paragraph as written, marker
    // included, though the returned text drops the marker: two
    // paragraphs, two lines.
    assert_eq!(lines(&hits), [14, 18]);
    assert!(hits.iter().all(|h| h["block_id"] == "dup"));
    assert_eq!(field(&hits[0], "text"), "- a list item");
    assert_eq!(field(&hits[1], "text"), "Another paragraph.");
}

#[tokio::test]
async fn code_blocks_are_symbols_with_kind_line_and_an_existing_id_only() {
    let (daemon, _src) = world().await;
    let registry =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/registry.rs"))
            .unwrap();
    let enum_line = line_of(&registry, "pub enum ConflictReason");

    let hits = daemon
        .results("blocks", "@fx:code/registry.rs", "conflictreason")
        .await;
    assert!(!hits.is_empty());
    let first = &hits[0];
    assert_eq!(first["kind"], "symbol");
    assert_eq!(first["name"], "ConflictReason");
    assert_eq!(first["symbol_kind"], "enum");
    // The definition line, not the `#[derive]` above it.
    assert_eq!(first["line"], enum_line);
    assert!(first.get("block_id").is_none(), "no ID exists yet: {first}");
    assert!(first.get("text").is_none());
    for hit in &hits {
        assert_eq!(hit["kind"], "symbol");
        assert!(hit["symbol_kind"].is_string() && hit["line"].as_i64().unwrap() >= 1);
    }

    // Decorators: the `def` line carries the symbol, not an `@` line.
    let hits = daemon.results("blocks", "@fx:code/decorated.py", "").await;
    let named = |n: &str| {
        hits.iter()
            .find(|h| h["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(named("decorated")["line"], 6);
    assert_eq!(named("Thing")["line"], 10);
    assert_eq!(named("prop")["line"], 12);
}

/// Settled detail: `headings` on a code file is its symbols.
#[tokio::test]
async fn headings_scope_on_a_code_file_returns_its_symbols() {
    let (daemon, _src) = world().await;
    let blocks = daemon.results("blocks", "@fx:code/decorated.py", "").await;
    let headings = daemon
        .results("headings", "@fx:code/decorated.py", "")
        .await;
    assert_eq!(headings, blocks);
    assert!(headings.iter().all(|h| h["kind"] == "symbol"));
}

/// E12: a file with a parse error still gives the symbols of the parts
/// that parse.
#[tokio::test]
async fn a_code_file_with_a_parse_error_still_returns_the_symbols_that_parse() {
    let (daemon, _src) = world().await;
    let hits = daemon.results("blocks", "@fx:code/broken.ts", "").await;
    let names: Vec<&str> = hits.iter().map(|h| field(h, "name")).collect();
    assert!(names.contains(&"good"), "{names:?}");
    assert!(names.contains(&"Fine"), "{names:?}");
}

#[tokio::test]
async fn documents_list_paragraphs_by_page_slide_or_paragraph_and_never_a_block_id() {
    let (daemon, src) = world().await;
    for (file, key) in [
        ("docs/report.docx", "paragraph"),
        ("docs/deck.pptx", "slide"),
        ("docs/text.pdf", "page"),
    ] {
        let bytes = std::fs::read(src.root().join(file)).unwrap();
        let parsed = tephra_parser::parse_document_bytes(Path::new(file), &bytes).unwrap();
        let hits = daemon.results("blocks", &format!("@fx:{file}"), "").await;
        assert_eq!(hits.len(), parsed.paragraphs.len(), "{file}");
        assert!(!hits.is_empty(), "{file}");
        for (hit, want) in hits.iter().zip(&parsed.paragraphs) {
            assert_eq!(hit["kind"], "document paragraph", "{file}");
            assert_eq!(hit[key], want.location.number() as i64, "{file}: {hit}");
            assert_eq!(hit["index"], want.index as i64, "{file}: {hit}");
            assert_eq!(field(hit, "text"), want.text, "{file}");
            assert!(hit.get("line").is_none(), "{file}: {hit}");
            assert!(hit.get("block_id").is_none(), "{file}: {hit}");
        }
        // A query finds the paragraph by its text.
        let probe = parsed.paragraphs[0].text.clone();
        let found = daemon
            .results("blocks", &format!("@fx:{file}"), &probe)
            .await;
        assert_eq!(field(&found[0], "text"), probe, "{file}");
    }
}

#[tokio::test]
async fn document_headings_are_listed_by_their_page_slide_or_paragraph() {
    let (daemon, _src) = world().await;
    let docx = daemon.results("headings", "@fx:docs/report.docx", "").await;
    let pairs: Vec<(String, i64, i64)> = docx
        .iter()
        .map(|h| {
            (
                field(h, "text").to_string(),
                h["level"].as_i64().unwrap(),
                h["paragraph"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ("Quarterly report".to_string(), 1, 1),
            ("Overview".to_string(), 1, 2),
            ("Details".to_string(), 2, 4),
            ("A style known only by name".to_string(), 3, 6),
        ]
    );
    let deck = daemon
        .results("headings", "@fx:docs/deck.pptx", "agenda")
        .await;
    assert_eq!(texts(&deck), ["Agenda"]);
    assert_eq!(deck[0]["slide"], 2);
    let pdf = daemon.results("headings", "@fx:docs/text.pdf", "").await;
    assert!(!pdf.is_empty() && pdf.iter().all(|h| h["page"].is_i64()));
}

/// E4, G6, G7: nothing to list, and no error: a PDF with no text
/// layer, a corrupt docx, a csv, a png, and a file over the size cap.
#[tokio::test]
async fn name_only_units_return_an_empty_list_for_both_scopes() {
    let (daemon, _src) = world().await;
    for unit in [
        "@fx:docs/scanned.pdf",
        "@fx:docs/truncated.docx",
        "@fx:data/table.csv",
        "@fx:img/logo.png",
        "@fx:notes/huge.md",
    ] {
        for scope in ["headings", "blocks"] {
            for query in ["", "a"] {
                let result = daemon.search(scope, unit, query).await;
                assert_eq!(result["results"], json!([]), "{unit} {scope} {query:?}");
                assert_eq!(result["offline"], false);
            }
        }
    }
}

#[tokio::test]
async fn limit_defaults_to_fifty_and_is_honoured() {
    let (daemon, _src) = world().await;
    let all = daemon.results("headings", "@fx:notes/many.md", "").await;
    assert_eq!(all.len(), 50);
    let some = daemon
        .rpc(
            "unit.search",
            json!({"query": "", "scope": "headings", "in": "@fx:notes/many.md", "limit": 60}),
        )
        .await;
    assert_eq!(some["result"]["results"].as_array().unwrap().len(), 60);
    let three = daemon
        .rpc(
            "unit.search",
            json!({"query": "heading", "scope": "headings", "in": "@fx:notes/many.md", "limit": 3}),
        )
        .await;
    assert_eq!(three["result"]["results"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn an_empty_and_a_whitespace_query_both_list_everything_in_file_order() {
    let (daemon, _src) = world().await;
    let empty = daemon.results("headings", "@fx:notes/a.md", "").await;
    let blank = daemon.results("headings", "@fx:notes/a.md", "  \t").await;
    assert_eq!(empty, blank);
    assert_eq!(lines(&empty), [5, 9, 16]);
}

// ------------------------------------------------------- GP5 and ranking

#[tokio::test]
async fn gp5_conflict_lists_the_enum_first_then_the_other_matching_symbols() {
    let (daemon, _src) = world().await;
    let hits = daemon
        .results("blocks", "@fx:code/registry.rs", "conflict")
        .await;
    let first = &hits[0];
    assert_eq!(
        (field(first, "name"), field(first, "symbol_kind")),
        ("ConflictReason", "enum")
    );
    // The other symbols whose names match follow: every result matches.
    for hit in &hits {
        let name = field(hit, "name").to_lowercase();
        assert!(
            name.contains("conflict") || {
                let mut rest = name.as_str();
                "conflict".chars().all(|c| match rest.find(c) {
                    Some(i) => {
                        rest = &rest[i + c.len_utf8()..];
                        true
                    }
                    None => false,
                })
            },
            "{hit}"
        );
    }
    // An `impl` of the same name ties on name and comes after, by line.
    let impls: Vec<&Value> = hits
        .iter()
        .filter(|h| h["name"] == "ConflictReason")
        .collect();
    assert!(impls.len() >= 2, "{hits:?}");
    assert!(impls
        .windows(2)
        .all(|w| w[0]["line"].as_i64() < w[1]["line"].as_i64()));
}

/// GP5 against the real checkout: `crates/` of this repo as a source.
/// The spec names line 73; on this checkout the line is wherever
/// `pub enum ConflictReason` is, found by grep, and the test says so.
#[tokio::test]
async fn gp5_registry_conflict() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let registry = std::fs::read_to_string(crates.join("tephrad/src/registry.rs")).unwrap();
    let found = line_of(&registry, "pub enum ConflictReason");
    if found != 73 {
        eprintln!("skipping the line-73 check: ConflictReason is on line {found} on this checkout; asserting that line instead");
    }
    let daemon = Daemon::start().await;
    daemon.register("tephra", &crates).await;
    let hits = daemon
        .results("blocks", "@tephra:tephrad/src/registry.rs", "conflict")
        .await;
    let first = &hits[0];
    assert_eq!(first["name"], "ConflictReason");
    assert_eq!(first["symbol_kind"], "enum");
    assert_eq!(first["line"], found);
    // The `impl fmt::Display for ConflictReason` block follows it.
    let display = line_of(&registry, "impl fmt::Display for ConflictReason");
    assert!(
        hits.iter()
            .skip(1)
            .any(|h| h["name"] == "ConflictReason" && h["line"] == display),
        "{hits:?}"
    );
}

// -------------------------------------------------------- errors and edges

#[tokio::test]
async fn a_missing_in_is_pick_a_file_first_and_a_misplaced_in_is_refused() {
    let (daemon, _src) = world().await;
    for scope in ["headings", "blocks"] {
        let (code, message) = daemon
            .search_error(json!({"query": "a", "scope": scope}))
            .await;
        assert_eq!(code, -32602);
        assert!(message.contains("pick a file first"), "{message}");
    }
    for scope in ["names", "all_blocks"] {
        let (code, message) = daemon
            .search_error(json!({"query": "a", "scope": scope, "in": "@fx:notes/a.md"}))
            .await;
        assert_eq!(code, -32602, "{scope}: {message}");
        assert!(message.contains("\"in\""), "{message}");
    }
}

/// F7: `in` never widens: an unknown unit, an unknown source and a
/// folder are errors naming the id, though a name search finds the
/// same text in other files.
#[tokio::test]
async fn an_unknown_unit_source_or_folder_is_an_error_naming_the_id() {
    let (daemon, _src) = world().await;
    for id in [
        "@fx:notes/missing.md",
        "@nosuch:notes/a.md",
        "@fx:notes",
        "@fx:notes/",
        "@fx:../notes/a.md",
        "@fx:/etc/hosts",
    ] {
        for scope in ["headings", "blocks"] {
            let (code, message) = daemon
                .search_error(json!({"query": "alpha", "scope": scope, "in": id}))
                .await;
            assert_ne!(code, 0);
            assert!(message.contains(id), "{id}: {message}");
        }
    }
    let (code, _) = daemon
        .search_error(json!({"query": "a", "scope": "headings", "in": "notes/a.md"}))
        .await;
    assert_eq!(code, -32602);
}

#[tokio::test]
async fn a_request_with_no_token_or_a_wrong_one_is_401() {
    let (daemon, _src) = world().await;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "unit.search",
        "params": {"query": "", "scope": "headings", "in": "@fx:notes/a.md"}});
    let none = daemon
        .client
        .post(&daemon.url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), 401);
    let wrong = daemon
        .client
        .post(&daemon.url)
        .bearer_auth("e".repeat(64))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

// -------------------------------------------------------------- fences

fn snapshot(root: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
    fixtures::tree_snapshot(root)
}

/// Every row of every table, in a stable order.
fn dump_cache(path: &Path) -> Vec<String> {
    let cache = Cache::open_read_only(path).expect("open");
    let conn = cache.connection();
    let mut out = Vec::new();
    for (table, order) in [
        ("units", "id"),
        ("headings", "id"),
        ("blocks", "id"),
        ("symbols", "id"),
        ("links", "id"),
        ("tags", "id"),
        ("sources", "id"),
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
            .unwrap();
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap();
        out.extend(rows.map(|r| format!("{table}: {}", r.unwrap())));
    }
    out
}

/// F4: a search writes nothing: the source's bytes and mtimes and the
/// cache's rows are unchanged, and it needs no writer: it succeeds
/// while another connection holds the cache's write lock.
#[tokio::test]
async fn f4_a_search_writes_nothing_and_opens_no_writer_transaction() {
    let (daemon, src) = world().await;
    let files_before = snapshot(src.root());
    let rows_before = dump_cache(&daemon.cache_path);

    // Hold the one writer lock for the whole run of searches.
    let holder = rusqlite::Connection::open(&daemon.cache_path).expect("open holder");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let started = Instant::now();
    for (scope, unit) in [
        ("headings", "@fx:notes/a.md"),
        ("blocks", "@fx:notes/a.md"),
        ("blocks", "@fx:code/registry.rs"),
        ("headings", "@fx:docs/report.docx"),
        ("blocks", "@fx:docs/text.pdf"),
        ("blocks", "@fx:docs/deck.pptx"),
        ("blocks", "@fx:docs/scanned.pdf"),
    ] {
        for query in ["", "a", "zzzz"] {
            daemon.search(scope, unit, query).await;
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "searches waited on the writer lock"
    );
    holder.execute_batch("ROLLBACK").expect("release");
    drop(holder);

    assert_eq!(snapshot(src.root()), files_before, "the source changed");
    assert_eq!(
        dump_cache(&daemon.cache_path),
        rows_before,
        "the cache changed"
    );
}

/// F7: link resolution answers the same before and after any number of
/// searches.
#[tokio::test]
async fn f7_resolve_link_is_identical_before_and_after_searches() {
    let (daemon, _src) = world().await;
    let links = [
        json!({"link": "[[a]]", "default_source": "fx"}),
        json!({"link": "[[notes/a#Café notes]]", "default_source": "fx"}),
        json!({"link": "[[nosuch]]", "default_source": "fx"}),
        json!({"link": "[[a#^dup]]", "default_source": "fx"}),
    ];
    let mut before = Vec::new();
    for params in &links {
        before.push(daemon.rpc("unit.resolve_link", params.clone()).await);
    }
    for _ in 0..5 {
        for scope in ["headings", "blocks"] {
            daemon.search(scope, "@fx:notes/a.md", "dup").await;
            daemon.search(scope, "@fx:notes/b.md", "").await;
        }
        daemon.rpc("unit.search", json!({"query": "alpha"})).await;
    }
    for (params, want) in links.iter().zip(&before) {
        assert_eq!(&daemon.rpc("unit.resolve_link", params.clone()).await, want);
    }
}

/// F3: nothing here invents an ID or a symbol link. A result carries a
/// `block_id` only when the file already holds the ID, and a code
/// file's symbols never gain one from being searched.
#[tokio::test]
async fn f3_block_ids_are_only_those_already_in_the_file() {
    let (daemon, src) = world().await;
    let before = std::fs::read(src.root().join("code/registry.rs")).unwrap();
    let hits = daemon.results("blocks", "@fx:code/registry.rs", "").await;
    assert!(hits.iter().all(|h| h.get("block_id").is_none()));
    let notes = daemon.results("blocks", "@fx:notes/a.md", "").await;
    let ids: Vec<&str> = notes
        .iter()
        .filter_map(|h| h["block_id"].as_str())
        .collect();
    assert_eq!(ids, ["first", "dup", "dup"]);
    assert_eq!(
        std::fs::read(src.root().join("code/registry.rs")).unwrap(),
        before
    );
}

// ---------------------------------------------- stale and unsafe files

/// A bare cache scanned with no watcher, so the file can change under
/// its cached rows.
struct Scanned {
    registry_cache: PathBuf,
    src: fixtures::TempSource,
    _data: tempfile::TempDir,
}

fn scanned(files: &[(&str, &str)]) -> Scanned {
    let data = tempfile::tempdir().unwrap();
    let cache_path = data.path().join("cache.db");
    let cache = Cache::open(&cache_path).unwrap();
    let src = fixtures::empty_source();
    for (path, body) in files {
        src.write(path, body);
    }
    let root = src.root().canonicalize().unwrap();
    let id = cache
        .register_source(
            "s",
            &root.to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .unwrap();
    tephrad::scan::scan_source(
        &cache,
        id,
        &root,
        &tephrad::ignore::IgnoreRules::load(&root).unwrap(),
    )
    .expect("scan");
    Scanned {
        registry_cache: cache_path,
        src,
        _data: data,
    }
}

fn blocks_of(s: &Scanned, query: &str) -> tephrad::search::in_file::InFileSearchWire {
    tephrad::search::in_file::search_in_file(
        &s.registry_cache,
        "@s:n.md",
        tephra_cache::InFileScope::Blocks,
        query,
        50,
    )
    .expect("search")
}

#[test]
fn a_file_that_changed_since_it_was_indexed_is_not_read_for_paragraph_text() {
    let s = scanned(&[("n.md", "# T\n\nalpha paragraph\n\nbeta paragraph\n")]);
    let fresh = blocks_of(&s, "alpha");
    assert_eq!(texts_of(&fresh), ["alpha paragraph"]);
    assert!(!fresh.text_unavailable);

    // Same length, different bytes: the size matches, the mtime does not
    // (set explicitly so the test never depends on clock resolution).
    let path = s.src.root().join("n.md");
    std::fs::write(&path, "# T\n\nOMEGA paragraph\n\nbeta paragraph\n").unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
    let stale = blocks_of(&s, "omega");
    assert!(stale.text_unavailable);
    assert!(stale.results.is_empty(), "stale text must not be served");
    // The positions are still cached and an empty query still lists them.
    assert_eq!(blocks_of(&s, "").results.len(), 2);

    // A file that is gone.
    std::fs::remove_file(&path).unwrap();
    assert!(blocks_of(&s, "beta").text_unavailable);
}

#[cfg(unix)]
#[test]
fn a_file_replaced_by_a_symlink_is_never_followed() {
    let s = scanned(&[("n.md", "# T\n\nalpha paragraph\n")]);
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.md");
    std::fs::write(&secret, "# T\n\nSECRET paragraph\n").unwrap();
    let path = s.src.root().join("n.md");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&secret, &path).unwrap();

    let wire = blocks_of(&s, "secret");
    assert!(wire.text_unavailable);
    assert!(wire.results.is_empty());
    let all = serde_json::to_string(&blocks_of(&s, "")).unwrap();
    assert!(!all.contains("SECRET"), "{all}");

    // A symlinked directory above the file is not followed either.
    let s = scanned(&[("d/n.md", "# T\n\nalpha paragraph\n")]);
    let moved = outside.path().join("d");
    std::fs::rename(s.src.root().join("d"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, s.src.root().join("d")).unwrap();
    let wire = tephrad::search::in_file::search_in_file(
        &s.registry_cache,
        "@s:d/n.md",
        tephra_cache::InFileScope::Blocks,
        "alpha",
        50,
    )
    .unwrap();
    assert!(wire.text_unavailable);
    assert!(wire.results.is_empty());
}

/// E13: a source the OS won't list is a clear error, not a panic and not
/// a half-answer.
#[cfg(unix)]
#[test]
fn a_unit_in_a_permission_denied_source_is_a_clear_error() {
    use std::os::unix::fs::PermissionsExt;
    let s = scanned(&[("n.md", "# T\n\nalpha paragraph\n")]);
    let root = s.src.root().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied = std::fs::read_dir(&root).is_err();
    let result = tephrad::search::in_file::search_in_file(
        &s.registry_cache,
        "@s:n.md",
        tephra_cache::InFileScope::Headings,
        "",
        50,
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !denied {
        eprintln!("skipping: this user can list a mode-000 directory (running as root?)");
        return;
    }
    match result {
        Err(tephrad::search::SearchError::Source(message)) => {
            assert!(message.contains("permission denied"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

fn texts_of(wire: &tephrad::search::in_file::InFileSearchWire) -> Vec<String> {
    let value = serde_json::to_value(wire).unwrap();
    texts(value["results"].as_array().unwrap())
}
