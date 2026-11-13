//! G9 `unit.search` inside one file, at the dispatch level: the params
//! checks (missing or misplaced `in`, wrong scope, malformed id, empty
//! and whitespace queries, `limit`) through the real `dispatch` function
//! with an in-memory backend that records what reaches it, and the
//! ranking, tie rules, limit and `offline` flag through the real
//! `SourceRegistry` on a scanned temp dir.

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::sync::Mutex;

use serde_json::{json, Value};
use tephra_cache::{Cache, InFileScope};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    InFileSearchWire, ScanStatsWire, SearchError, SourceBackend, SourceRowWire,
};
use tephrad::rpc::dispatch::{dispatch, Backends};
use tephrad::rpc::protocol::{INVALID_PARAMS, SOURCE_ERROR};

// ---- the in-memory backend --------------------------------------------

type Call = (String, InFileScope, String, usize);

struct Stub {
    calls: Mutex<Vec<Call>>,
    names_calls: Mutex<usize>,
    reply: Result<InFileSearchWire, SearchError>,
}

impl Stub {
    fn answering(reply: Result<InFileSearchWire, SearchError>) -> Stub {
        Stub {
            calls: Mutex::new(Vec::new()),
            names_calls: Mutex::new(0),
            reply,
        }
    }
    fn ok() -> Stub {
        Stub::answering(Ok(InFileSearchWire {
            unit: "@s:a.md".to_string(),
            scope: "headings",
            offline: false,
            text_unavailable: false,
            results: Vec::new(),
        }))
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl SourceBackend for Stub {
    fn register(&self, _: &str, _: &str) -> Result<ScanStatsWire, String> {
        unreachable!("unit.search never registers")
    }
    fn unregister(&self, _: &str) -> Result<(), String> {
        unreachable!("unit.search never unregisters")
    }
    fn list(&self) -> Result<Vec<SourceRowWire>, String> {
        unreachable!("unit.search never lists sources")
    }
    fn rescan(&self, _: &str) -> Result<ScanStatsWire, String> {
        unreachable!("unit.search never rescans")
    }
    fn search_names(
        &self,
        _: &str,
        _: usize,
    ) -> Result<Vec<tephrad::rpc::backend::SearchHitWire>, SearchError> {
        *self.names_calls.lock().unwrap() += 1;
        Ok(Vec::new())
    }
    fn search_in_file(
        &self,
        unit: &str,
        scope: InFileScope,
        query: &str,
        limit: usize,
    ) -> Result<InFileSearchWire, SearchError> {
        self.calls
            .lock()
            .unwrap()
            .push((unit.to_string(), scope, query.to_string(), limit));
        self.reply.clone()
    }
}

struct NullGraph;
impl tephrad::rpc::backend::GraphBackend for NullGraph {
    fn resolve_link(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<tephra_graph::ResolveResult, tephra_graph::ResolveError> {
        unreachable!("unit.search never resolves a link")
    }
    fn get_backlinks(&self, _: &str) -> Vec<tephra_graph::Edge> {
        unreachable!()
    }
    fn get_forward_links(&self, _: &str) -> Vec<tephra_graph::Edge> {
        unreachable!()
    }
    fn get_edges(&self) -> Vec<tephra_graph::Edge> {
        unreachable!()
    }
}

/// A proof built the only way there is: a real token file through the
/// real `authenticate`.
fn proof() -> tephrad::rpc::auth::Authenticated {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "x".repeat(64)).unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", "x".repeat(64))).unwrap(),
    );
    tephrad::rpc::auth::authenticate(&headers, &path).expect("proof")
}

fn call(sources: &dyn SourceBackend, params: Value) -> Result<Value, (i64, String)> {
    static REFACTOR: tephrad::rpc::backend::UnsupportedRefactor =
        tephrad::rpc::backend::UnsupportedRefactor;
    let graph = NullGraph;
    let backends = Backends {
        graph: &graph,
        sources,
        refactor: &REFACTOR,
    };
    dispatch(&proof(), "unit.search", &params, &backends)
        .map_err(|(code, message, _)| (code, message))
}

/// A request that must be refused as `invalid_params` before the
/// backend is reached; returns the message.
fn refused(params: Value) -> String {
    let stub = Stub::ok();
    let (code, message) = call(&stub, params.clone()).expect_err("should be refused");
    assert_eq!(code, INVALID_PARAMS, "{params}: {message}");
    assert!(stub.calls().is_empty(), "{params} reached the backend");
    assert_eq!(*stub.names_calls.lock().unwrap(), 0, "{params}");
    message
}

// ---- params validation ------------------------------------------------

#[test]
fn headings_and_blocks_without_in_say_pick_a_file_first() {
    for scope in ["headings", "blocks"] {
        for params in [
            json!({"query": "a", "scope": scope}),
            json!({"query": "a", "scope": scope, "in": null}),
            json!({"query": "", "scope": scope}),
        ] {
            let message = refused(params.clone());
            assert!(message.contains("pick a file first"), "{params}: {message}");
        }
    }
}

#[test]
fn in_with_names_all_blocks_or_no_scope_is_refused_not_ignored() {
    for scope in [json!("names"), json!("all_blocks"), Value::Null] {
        let message = refused(json!({"query": "a", "scope": scope, "in": "@s:a.md"}));
        assert!(message.contains("\"in\""), "{scope}: {message}");
    }
    // The scope key left out altogether is `names`.
    refused(json!({"query": "a", "in": "@s:a.md"}));
}

#[test]
fn a_malformed_or_non_string_in_is_refused() {
    for unit in [
        json!("a.md"),
        json!("s:a.md"),
        json!("@s"),
        json!("@s:"),
        json!("@:a.md"),
        json!(""),
        json!(7),
        json!(["@s:a.md"]),
        json!({}),
    ] {
        for scope in ["headings", "blocks"] {
            refused(json!({"query": "a", "scope": scope, "in": unit}));
        }
    }
}

#[test]
fn unknown_scopes_are_still_refused_and_a_bad_query_is_still_refused() {
    let message = refused(json!({"query": "a", "scope": "Headings", "in": "@s:a.md"}));
    assert!(message.contains("unknown scope"), "{message}");
    let message = refused(json!({"query": "a", "scope": "all_blocks"}));
    assert!(message.contains("not supported yet"), "{message}");
    for scope in ["headings", "blocks"] {
        refused(json!({"scope": scope, "in": "@s:a.md"}));
        refused(json!({"query": 5, "scope": scope, "in": "@s:a.md"}));
        let too_long = "a".repeat(tephrad::search::fuzzy::MAX_QUERY_CHARS + 1);
        refused(json!({"query": too_long, "scope": scope, "in": "@s:a.md"}));
    }
}

#[test]
fn an_empty_or_whitespace_query_is_accepted_and_passed_on_verbatim() {
    for (scope, wire) in [
        ("headings", InFileScope::Headings),
        ("blocks", InFileScope::Blocks),
    ] {
        for query in ["", "   ", "\t\n"] {
            let stub = Stub::ok();
            call(
                &stub,
                json!({"query": query, "scope": scope, "in": "@s:a.md"}),
            )
            .expect("accepted");
            assert_eq!(
                stub.calls(),
                [("@s:a.md".to_string(), wire, query.to_string(), 50)]
            );
        }
    }
}

#[test]
fn limit_defaults_to_fifty_and_is_checked() {
    let stub = Stub::ok();
    call(
        &stub,
        json!({"query": "a", "scope": "blocks", "in": "@s:a.md"}),
    )
    .unwrap();
    call(
        &stub,
        json!({"query": "a", "scope": "blocks", "in": "@s:a.md", "limit": null}),
    )
    .unwrap();
    call(
        &stub,
        json!({"query": "a", "scope": "blocks", "in": "@s:a.md", "limit": 7}),
    )
    .unwrap();
    let limits: Vec<usize> = stub.calls().iter().map(|c| c.3).collect();
    assert_eq!(limits, [50, 50, 7]);

    for limit in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("5"),
        json!(true),
        json!(tephrad::search::MAX_LIMIT + 1),
    ] {
        let message =
            refused(json!({"query": "a", "scope": "blocks", "in": "@s:a.md", "limit": limit}));
        assert!(message.contains("limit"), "{limit}: {message}");
    }
}

#[test]
fn names_scope_never_reaches_the_in_file_search() {
    let stub = Stub::ok();
    call(&stub, json!({"query": "a"})).unwrap();
    call(&stub, json!({"query": "a", "scope": "names"})).unwrap();
    assert!(stub.calls().is_empty());
    assert_eq!(*stub.names_calls.lock().unwrap(), 2);
}

#[test]
fn backend_errors_map_to_the_right_codes_and_the_wire_shape_is_kept() {
    let stub = Stub::answering(Err(SearchError::Source("no such unit: @s:a.md".into())));
    let (code, message) = call(
        &stub,
        json!({"query": "a", "scope": "headings", "in": "@s:a.md"}),
    )
    .unwrap_err();
    assert_eq!(code, SOURCE_ERROR);
    assert!(message.contains("@s:a.md"), "{message}");

    let stub = Stub::answering(Err(SearchError::InvalidParams("bad".into())));
    let (code, _) = call(
        &stub,
        json!({"query": "a", "scope": "headings", "in": "@s:a.md"}),
    )
    .unwrap_err();
    assert_eq!(code, INVALID_PARAMS);

    let stub = Stub::answering(Ok(InFileSearchWire {
        unit: "@s:a.md".into(),
        scope: "blocks",
        offline: true,
        text_unavailable: true,
        results: Vec::new(),
    }));
    let value = call(
        &stub,
        json!({"query": "a", "scope": "blocks", "in": "@s:a.md"}),
    )
    .unwrap();
    assert_eq!(
        value,
        json!({
            "unit": "@s:a.md", "scope": "blocks", "offline": true,
            "text_unavailable": true, "results": []
        })
    );
}

// ---- ranking, ties, limit and offline on the real registry ---------------

struct Real {
    registry: SourceRegistry,
    source: fixtures::TempSource,
    _data: tempfile::TempDir,
}

fn real(files: &[(&str, &str)]) -> Real {
    let data = tempfile::tempdir().unwrap();
    let registry = SourceRegistry::new(Cache::open(data.path().join("cache.db")).unwrap()).unwrap();
    let source = fixtures::empty_source();
    for (path, body) in files {
        source.write(path, body);
    }
    registry.register("s", source.root()).expect("register");
    Real {
        registry,
        source,
        _data: data,
    }
}

fn texts(wire: &InFileSearchWire) -> Vec<String> {
    let value = serde_json::to_value(wire).unwrap();
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["text"]
                .as_str()
                .or(r["name"].as_str())
                .unwrap()
                .to_string()
        })
        .collect()
}

const RANKED: &str = "\
# Conflict resolution
## xxconflict
## Conflicts
## xconflict
## Conflict
## Same
## Same
";

#[test]
fn ranking_runs_exact_prefix_then_fuzzy_and_ties_go_to_the_shorter_text_then_the_line() {
    let r = real(&[("a.md", RANKED)]);
    let wire = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "conflict", 50)
        .unwrap();
    // Exact first; then prefixes by how much of the heading the query
    // covers ("Conflicts" 8/9 beats "Conflict resolution" 8/19); then
    // fuzzy matches, tied on a tight span, the shorter text first even
    // though it comes later in the file.
    assert_eq!(
        texts(&wire),
        [
            "Conflict",
            "Conflicts",
            "Conflict resolution",
            "xconflict",
            "xxconflict"
        ]
    );
    assert!(!wire.offline);

    // Same text, same score: the earlier line first.
    let wire = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "same", 50)
        .unwrap();
    let value = serde_json::to_value(&wire).unwrap();
    let lines: Vec<i64> = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, [6, 7]);
}

#[test]
fn an_empty_query_lists_everything_in_file_order_and_limit_is_honoured() {
    let r = real(&[("a.md", RANKED)]);
    let all = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "  ", 50)
        .unwrap();
    assert_eq!(
        texts(&all),
        [
            "Conflict resolution",
            "xxconflict",
            "Conflicts",
            "xconflict",
            "Conflict",
            "Same",
            "Same"
        ]
    );
    let two = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "", 2)
        .unwrap();
    assert_eq!(texts(&two), ["Conflict resolution", "xxconflict"]);
    let one = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "conflict", 1)
        .unwrap();
    assert_eq!(texts(&one), ["Conflict"]);
}

#[test]
fn an_offline_source_is_flagged_and_still_answers_from_the_cache() {
    let r = real(&[("a.md", "# Alpha\n\nA paragraph about alpha.\n")]);
    let root = r.source.root().to_path_buf();
    let moved = root.with_extension("moved");
    std::fs::rename(&root, &moved).expect("take the source offline");

    let headings = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Headings, "alp", 50)
        .unwrap();
    assert!(headings.offline);
    assert_eq!(texts(&headings), ["Alpha"]);

    // The paragraph text can't be read back, and that is said so.
    let blocks = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Blocks, "", 50)
        .unwrap();
    assert!(blocks.offline);
    assert!(blocks.text_unavailable);
    assert_eq!(blocks.results.len(), 1);
    let none = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Blocks, "alpha", 50)
        .unwrap();
    assert!(none.results.is_empty());

    std::fs::rename(&moved, &root).expect("restore");
    let back = r
        .registry
        .search_in_file("@s:a.md", InFileScope::Blocks, "alpha", 50)
        .unwrap();
    assert!(!back.offline && !back.text_unavailable);
    assert_eq!(texts(&back), ["A paragraph about alpha."]);
}

#[test]
fn an_unknown_unit_source_or_folder_is_an_error_naming_the_id_never_a_wider_search() {
    let r = real(&[("notes/a.md", "# Alpha\n")]);
    for id in [
        "@s:missing.md",
        "@nosuch:notes/a.md",
        "@s:notes",
        "@s:notes/",
    ] {
        let err = r
            .registry
            .search_in_file(id, InFileScope::Headings, "", 50)
            .unwrap_err();
        match err {
            SearchError::Source(message) => assert!(message.contains(id), "{id}: {message}"),
            other => panic!("{id}: {other:?}"),
        }
    }
}
