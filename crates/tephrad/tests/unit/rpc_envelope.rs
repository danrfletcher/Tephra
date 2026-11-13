//! Unit tests for `tephrad::rpc::protocol`'s envelope validation and
//! `tephrad::rpc::dispatch`'s method routing — exercised through the
//! real `validate_envelope`/`dispatch` functions, not a mocked
//! stand-in, per spec: "JSON-RPC envelope validation — missing
//! `jsonrpc`, missing `id`, batch request handling (accepted or
//! explicitly rejected — pick one and test it), unknown method,
//! invalid params shape per method".

use serde_json::json;
use tephrad::rpc::dispatch::{dispatch, Backends, METHOD_NAMES};
use tephrad::rpc::protocol::{
    batch_rejected_response, error_response, success_response, validate_envelope, INVALID_PARAMS,
    INVALID_REQUEST, METHOD_NOT_FOUND,
};

// -- validate_envelope -------------------------------------------------

#[test]
fn missing_jsonrpc_field_is_rejected_as_invalid_request() {
    let raw = json!({ "id": 1, "method": "graph.get_edges" });
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_REQUEST);
    assert_eq!(err.id, json!(1));
}

#[test]
fn wrong_jsonrpc_version_string_is_rejected() {
    let raw = json!({ "jsonrpc": "1.0", "id": 1, "method": "graph.get_edges" });
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_REQUEST);
}

/// Judgement call (spec: "missing `id`" is explicitly called out as
/// a case to test, without saying what the right behavior is): a
/// missing `id` is accepted and treated as `Value::Null`, not
/// rejected — see `protocol.rs`'s `validate_envelope` doc comment for
/// the rationale. Pinned here so the behavior can't silently drift.
#[test]
fn missing_id_is_treated_as_null_not_rejected() {
    let raw = json!({ "jsonrpc": "2.0", "method": "graph.get_edges" });
    let req = validate_envelope(&raw).expect("missing id must not be rejected");
    assert_eq!(req.id, serde_json::Value::Null);
    assert_eq!(req.method, "graph.get_edges");
}

#[test]
fn missing_method_is_rejected_as_invalid_request() {
    let raw = json!({ "jsonrpc": "2.0", "id": 1 });
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_REQUEST);
}

#[test]
fn empty_string_method_is_rejected_as_invalid_request() {
    let raw = json!({ "jsonrpc": "2.0", "id": 1, "method": "" });
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_REQUEST);
}

#[test]
fn missing_params_defaults_to_an_empty_object() {
    let raw = json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges" });
    let req = validate_envelope(&raw).unwrap();
    assert_eq!(req.params, json!({}));
}

#[test]
fn non_object_params_is_rejected_as_invalid_params() {
    let raw = json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": [1, 2] });
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_PARAMS);
}

#[test]
fn a_non_object_top_level_value_is_rejected_as_invalid_request() {
    let raw = json!("just a string");
    let err = validate_envelope(&raw).unwrap_err();
    assert_eq!(err.code, INVALID_REQUEST);
}

/// Judgement call (spec: "batch request handling (accepted or
/// explicitly rejected — pick one and test it)"): this server
/// rejects batch requests outright. A top-level JSON array is never
/// passed to `validate_envelope` by either transport handler in
/// `server.rs` (both check `raw.is_array()` first and short-circuit
/// to [`batch_rejected_response`]); this test instead pins the
/// response shape that path produces, since that is the one a client
/// actually observes.
#[test]
fn batch_requests_are_rejected_with_invalid_request() {
    let response = batch_rejected_response();
    assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
    assert_eq!(response["id"], serde_json::Value::Null);
}

// -- protocol::success_response / error_response ------------------------

#[test]
fn success_response_echoes_the_request_id() {
    let response = success_response(json!(42), json!({ "ok": true }));
    assert_eq!(response["id"], json!(42));
    assert_eq!(response["result"]["ok"], json!(true));
    assert!(response.get("error").is_none());
}

#[test]
fn error_response_includes_optional_data_only_when_provided() {
    let without_data = error_response(json!(1), INVALID_PARAMS, "bad params", None);
    assert!(without_data["error"].get("data").is_none());

    let with_data = error_response(
        json!(1),
        INVALID_PARAMS,
        "bad params",
        Some(json!({"field": "link"})),
    );
    assert_eq!(with_data["error"]["data"]["field"], json!("link"));
}

// -- dispatch: unknown method / invalid params shape per method --------

/// A minimal no-op backend pair, enough to prove `dispatch` reaches
/// (or correctly refuses to reach) a handler — these tests are about
/// routing and params validation, not about what the handlers
/// themselves return (the integration tests cover that end-to-end
/// with `rpc_fixtures::MockGraph`/`MockSources`).
struct NullGraph;
impl tephrad::rpc::backend::GraphBackend for NullGraph {
    fn resolve_link(
        &self,
        _raw: &str,
        _default_source: Option<&str>,
    ) -> Result<tephra_graph::ResolveResult, tephra_graph::ResolveError> {
        unreachable!("params validation must fail before a handler is ever called")
    }
    fn get_backlinks(&self, _id: &str) -> Vec<tephra_graph::Edge> {
        unreachable!("params validation must fail before a handler is ever called")
    }
    fn get_forward_links(&self, _id: &str) -> Vec<tephra_graph::Edge> {
        unreachable!("params validation must fail before a handler is ever called")
    }
    fn get_edges(&self) -> Vec<tephra_graph::Edge> {
        Vec::new()
    }
}
struct NullSources;
impl tephrad::rpc::backend::SourceBackend for NullSources {
    fn register(
        &self,
        _name: &str,
        _root: &str,
    ) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("params validation must fail before a handler is ever called")
    }
    fn unregister(&self, _name: &str) -> Result<(), String> {
        unreachable!("params validation must fail before a handler is ever called")
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        Ok(Vec::new())
    }
    fn rescan(&self, _name: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("params validation must fail before a handler is ever called")
    }
}

/// The one place in this crate that can construct an [`Authenticated`]
/// proof outside `rpc::auth` itself: a real token file this test
/// controls, fed through the real [`tephrad::rpc::auth::authenticate`]
/// function — not a backdoor constructor. Keeping this here (rather
/// than exporting a test-only constructor from `rpc::auth`) is what
/// the compile-fail doctest on `Authenticated` is actually guarding.
fn proof() -> tephrad::rpc::auth::Authenticated {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "x".repeat(64)).unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", "x".repeat(64))).unwrap(),
    );
    tephrad::rpc::auth::authenticate(&headers, &path).expect("constructed proof")
}

fn backends<'a>(graph: &'a NullGraph, sources: &'a NullSources) -> Backends<'a> {
    static NULL_REFACTOR: tephrad::rpc::backend::UnsupportedRefactor =
        tephrad::rpc::backend::UnsupportedRefactor;
    Backends {
        graph,
        sources,
        refactor: &NULL_REFACTOR,
        blocks: &tephrad::rpc::backend::UnsupportedBlockIds,
    }
}

#[test]
fn unknown_method_returns_method_not_found() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "unit.delete_everything",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, METHOD_NOT_FOUND);
}

/// The closed set: the ten verbs before the file-types ticket, then
/// `unit.list` (PR-3.F1), `unit.search` (PR-8.F1) and `block.ensure_id`
/// (PR-11.F1). Adding a method
/// means changing this list on purpose.
const EXPECTED_METHODS: [&str; 13] = [
    "unit.resolve_link",
    "graph.get_backlinks",
    "graph.get_forward_links",
    "graph.get_edges",
    "source.register",
    "source.unregister",
    "source.list",
    "source.rescan",
    "link.preview_refactor",
    "link.apply_refactor",
    "unit.list",
    "unit.search",
    "block.ensure_id",
];

#[test]
fn method_table_is_exactly_the_thirteen_named_verbs() {
    assert_eq!(METHOD_NAMES, EXPECTED_METHODS);
    // No duplicates either.
    let mut sorted = METHOD_NAMES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), METHOD_NAMES.len());
}

/// `unit.search` is routed (it reaches the sources backend, which here
/// is the default that refuses), while near-misses are not methods.
#[test]
fn unit_search_is_accepted_and_a_misspelling_is_method_not_found() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let accepted = dispatch(
        &p,
        "unit.search",
        &json!({"query": "atlas"}),
        &backends(&graph, &sources),
    );
    match accepted {
        Ok(_) => {}
        Err((code, _, _)) => assert_ne!(code, METHOD_NOT_FOUND, "unit.search must be routed"),
    }

    for typo in ["unit.searc", "unit.Search", "units.search", "search"] {
        let err = dispatch(
            &p,
            typo,
            &json!({"query": "atlas"}),
            &backends(&graph, &sources),
        )
        .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND, "{typo}");
    }
}

/// `unit.list` is routed (it reaches the sources backend, which here
/// is the default that refuses), while a one-letter typo is not a
/// method at all.
#[test]
fn unit_list_is_accepted_and_a_misspelling_is_method_not_found() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let accepted = dispatch(
        &p,
        "unit.list",
        &json!({"source": "notes"}),
        &backends(&graph, &sources),
    );
    match accepted {
        Ok(_) => {}
        Err((code, _, _)) => assert_ne!(code, METHOD_NOT_FOUND, "unit.list must be routed"),
    }

    let err = dispatch(
        &p,
        "unit.lst",
        &json!({"source": "notes"}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, METHOD_NOT_FOUND);
}

#[test]
fn unit_resolve_link_without_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "unit.resolve_link",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn unit_resolve_link_with_empty_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "unit.resolve_link",
        &json!({"link": ""}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn unit_resolve_link_with_non_string_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "unit.resolve_link",
        &json!({"link": 123}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_backlinks_without_id_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "graph.get_backlinks",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_forward_links_without_id_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "graph.get_forward_links",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_edges_takes_no_params_and_never_fails_on_params_shape() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let result = dispatch(
        &p,
        "graph.get_edges",
        &json!({"ignored": "extra"}),
        &backends(&graph, &sources),
    );
    assert!(result.is_ok());
}

#[test]
fn source_register_without_name_or_root_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let missing_both = dispatch(
        &p,
        "source.register",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(missing_both.0, INVALID_PARAMS);

    let missing_root = dispatch(
        &p,
        "source.register",
        &json!({"name": "vault"}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(missing_root.0, INVALID_PARAMS);
}

#[test]
fn source_unregister_without_name_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(
        &p,
        "source.unregister",
        &json!({}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn source_rescan_without_name_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "source.rescan", &json!({}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn source_list_takes_no_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let result = dispatch(&p, "source.list", &json!({}), &backends(&graph, &sources));
    assert_eq!(result.unwrap(), json!([]));
}

#[test]
fn link_preview_refactor_without_required_params_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    for params in [
        json!({}),
        json!({"source": "notes"}),
        json!({"source": "notes", "old_path": "OldDoc.md"}),
        json!({"old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
    ] {
        let err = dispatch(
            &p,
            "link.preview_refactor",
            &params,
            &backends(&graph, &sources),
        )
        .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS, "params: {params}");
    }
}

#[test]
fn link_apply_refactor_without_required_params_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    for params in [
        json!({}),
        json!({"source": "notes"}),
        json!({"source": "notes", "old_path": "OldDoc.md"}),
        json!({"old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
    ] {
        let err = dispatch(
            &p,
            "link.apply_refactor",
            &params,
            &backends(&graph, &sources),
        )
        .unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS, "params: {params}");
    }
}

// -- unit.search with scope "all_blocks" (G10, the `^^` search) --------
//
// Through the real `dispatch`, with a backend that records every
// `search_all_blocks` it is handed: a request the dispatcher refuses
// must leave that list empty (it never reached the cache).

#[derive(Default)]
struct RecordingSources {
    all_blocks: std::sync::Mutex<Vec<(String, usize)>>,
    names: std::sync::Mutex<Vec<(String, usize)>>,
}
impl tephrad::rpc::backend::SourceBackend for RecordingSources {
    fn register(
        &self,
        _name: &str,
        _root: &str,
    ) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("a search never registers a source")
    }
    fn unregister(&self, _name: &str) -> Result<(), String> {
        unreachable!("a search never unregisters a source")
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        Ok(Vec::new())
    }
    fn rescan(&self, _name: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("a search never rescans a source")
    }
    fn search_names(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<tephrad::rpc::backend::SearchHitWire>, tephrad::rpc::backend::SearchError> {
        self.names.lock().unwrap().push((query.to_string(), limit));
        Ok(Vec::new())
    }
    fn search_all_blocks(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<tephrad::rpc::backend::AllBlocksHitWire>, tephrad::rpc::backend::SearchError>
    {
        self.all_blocks
            .lock()
            .unwrap()
            .push((query.to_string(), limit));
        Ok(Vec::new())
    }
}

fn search_all_blocks_dispatch(
    sources: &RecordingSources,
    params: serde_json::Value,
) -> Result<serde_json::Value, (i64, String)> {
    static NULL_REFACTOR: tephrad::rpc::backend::UnsupportedRefactor =
        tephrad::rpc::backend::UnsupportedRefactor;
    let graph = NullGraph;
    let p = proof();
    dispatch(
        &p,
        "unit.search",
        &params,
        &Backends {
            graph: &graph,
            sources,
            refactor: &NULL_REFACTOR,
            blocks: &tephrad::rpc::backend::UnsupportedBlockIds,
        },
    )
    .map_err(|(code, message, _)| (code, message))
}

#[test]
fn unit_search_all_blocks_is_routed_to_the_all_blocks_backend_with_the_default_or_given_limit() {
    let sources = RecordingSources::default();
    assert_eq!(
        search_all_blocks_dispatch(
            &sources,
            json!({"query": "conflict", "scope": "all_blocks"})
        )
        .unwrap(),
        json!([])
    );
    assert_eq!(
        search_all_blocks_dispatch(
            &sources,
            json!({"query": "^conflict", "scope": "all_blocks", "limit": 3})
        )
        .unwrap(),
        json!([])
    );
    assert_eq!(
        *sources.all_blocks.lock().unwrap(),
        vec![
            ("conflict".to_string(), tephrad::search::DEFAULT_LIMIT),
            ("^conflict".to_string(), 3)
        ]
    );
    assert!(sources.names.lock().unwrap().is_empty());
}

#[test]
fn unit_search_with_an_unknown_scope_is_invalid_params_naming_the_scope_and_reaches_nothing() {
    let sources = RecordingSources::default();
    // Near-misses of `all_blocks` included: a scope is matched exactly,
    // never case-folded or trimmed, and never quietly run as another.
    for scope in [
        "everything",
        "all",
        "All_Blocks",
        "all-blocks",
        "all_blocks ",
        " all_blocks",
        "^^",
        "",
    ] {
        let (code, message) =
            search_all_blocks_dispatch(&sources, json!({"query": "conflict", "scope": scope}))
                .unwrap_err();
        assert_eq!(code, INVALID_PARAMS, "scope {scope:?}");
        assert!(
            message.contains(&format!("unknown scope \"{scope}\"")),
            "scope {scope:?}: {message}"
        );
        assert!(message.contains("all_blocks"), "{message}");
    }
    // A scope that is not a string at all.
    for scope in [json!(5), json!(true), json!(["all_blocks"]), json!({})] {
        let (code, _) =
            search_all_blocks_dispatch(&sources, json!({"query": "conflict", "scope": scope}))
                .unwrap_err();
        assert_eq!(code, INVALID_PARAMS, "scope {scope}");
    }
    assert!(sources.all_blocks.lock().unwrap().is_empty());
    assert!(sources.names.lock().unwrap().is_empty());
}

#[test]
fn unit_search_all_blocks_without_a_usable_query_is_invalid_params_and_reaches_nothing() {
    let sources = RecordingSources::default();
    let missing_or_wrong_type = [
        json!({"scope": "all_blocks"}),
        json!({"query": null, "scope": "all_blocks"}),
        json!({"query": 5, "scope": "all_blocks"}),
        json!({"query": ["a"], "scope": "all_blocks"}),
    ];
    for params in missing_or_wrong_type {
        let (code, message) = search_all_blocks_dispatch(&sources, params.clone()).unwrap_err();
        assert_eq!(code, INVALID_PARAMS, "{params}");
        assert!(message.contains("query"), "{params}: {message}");
    }
    for query in ["", " ", "   ", "\t", " \n\t "] {
        let (code, message) =
            search_all_blocks_dispatch(&sources, json!({"query": query, "scope": "all_blocks"}))
                .unwrap_err();
        assert_eq!(code, INVALID_PARAMS, "query {query:?}");
        assert!(
            message.contains("empty query"),
            "query {query:?}: {message}"
        );
    }
    assert!(sources.all_blocks.lock().unwrap().is_empty());
}

#[test]
fn unit_search_all_blocks_refuses_an_in_param_and_a_bad_limit_and_reaches_nothing() {
    let sources = RecordingSources::default();
    let (code, message) = search_all_blocks_dispatch(
        &sources,
        json!({"query": "a", "scope": "all_blocks", "in": "@s:a.md"}),
    )
    .unwrap_err();
    assert_eq!(code, INVALID_PARAMS);
    assert!(message.contains("all_blocks"), "{message}");

    for limit in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("5"),
        json!(tephrad::search::MAX_LIMIT + 1),
    ] {
        let (code, message) = search_all_blocks_dispatch(
            &sources,
            json!({"query": "a", "scope": "all_blocks", "limit": limit}),
        )
        .unwrap_err();
        assert_eq!(code, INVALID_PARAMS, "limit {limit}");
        assert!(message.contains("limit"), "limit {limit}: {message}");
    }
    assert!(sources.all_blocks.lock().unwrap().is_empty());
}
