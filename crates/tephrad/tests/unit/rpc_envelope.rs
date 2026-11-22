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
        fixes: &tephrad::rpc::backend::UnsupportedFixes,
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
/// (PR-11.F1), then `fix.list` (atlas-25 PR-5, G18) and `link.rename_event`
/// (atlas-25 PR-8, G3), `source.promote` / `source.unpromote` (PR-12, PR-13)
/// and `source.rename` (PR-14, G13). Adding a method
/// means changing this list on purpose.
const EXPECTED_METHODS: [&str; 23] = [
    "unit.resolve_link",
    "graph.get_backlinks",
    "graph.get_forward_links",
    "graph.get_edges",
    "source.register",
    "source.unregister",
    "source.list",
    "source.rescan",
    "source.promote",
    "source.unpromote",
    "source.rename",
    "link.preview_refactor",
    "link.apply_refactor",
    "unit.list",
    "unit.search",
    "block.ensure_id",
    "fix.list",
    "link.rename_event",
    // atlas-27 PR-5 adds `source.add_github`, PR-7 adds `unit.fetch`, PR-8
    // adds the three cache verbs.
    "source.add_github",
    "unit.fetch",
    "cache.status",
    "cache.set_limit",
    "source.set_cache",
];

#[test]
fn method_table_is_exactly_the_twenty_three_named_verbs() {
    assert_eq!(METHOD_NAMES, EXPECTED_METHODS);
    // No duplicates either.
    let mut sorted = METHOD_NAMES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), METHOD_NAMES.len());
}

/// F9: the fix log is a record, never an undo. Unpromote folds a source
/// back (a forward fix of its own); no method undoes or reverts a fix.
#[test]
fn there_is_no_undo_or_revert_method() {
    for method in METHOD_NAMES {
        assert!(
            !method.contains("undo") && !method.contains("revert"),
            "{method}"
        );
    }
}

/// G13: `source.rename` needs a string `name` and a string `new_name`;
/// either missing or not a string is `INVALID_PARAMS` (-32602), and an
/// invented near-miss is still `METHOD_NOT_FOUND` (F12).
#[test]
fn source_rename_params_are_validated_and_near_misses_are_not_methods() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    for params in [
        json!({}),
        json!({"name": "notes"}),
        json!({"new_name": "x"}),
        json!({"name": 7, "new_name": "x"}),
        json!({"name": "notes", "new_name": 7}),
        json!({"name": "notes", "new_name": null}),
        json!({"name": "", "new_name": "x"}),
    ] {
        let err = dispatch(&p, "source.rename", &params, &backends(&graph, &sources)).unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS, "{params}");
    }
    // Both strings: routed (the default backend refuses, with a source error).
    let err = dispatch(
        &p,
        "source.rename",
        &json!({"name": "notes", "new_name": ""}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_ne!(err.0, METHOD_NOT_FOUND);
    assert_ne!(err.0, INVALID_PARAMS);

    for typo in [
        "source.rename_",
        "source.rename_preview",
        "source.set_name",
        "source.Rename",
    ] {
        let err = dispatch(
            &p,
            typo,
            &json!({"name": "notes", "new_name": "x"}),
            &backends(&graph, &sources),
        )
        .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND, "{typo}");
    }
}

/// `unit.fetch` (atlas-27 PR-7, G10) needs one non-empty string `unit` that
/// is a unit id (`@source:path`); anything else is `INVALID_PARAMS` before
/// a backend runs. A well-formed id is routed (the default backend refuses
/// with a source error), and near-misses are not methods.
#[test]
fn unit_fetch_params_are_validated_and_near_misses_are_not_methods() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    for params in [
        json!({}),
        json!({"unit": ""}),
        json!({"unit": 7}),
        json!({"unit": null}),
        json!({"unit": "notes:README.md"}),
        json!({"unit": "@notes"}),
        json!({"id": "@notes:README.md"}),
    ] {
        let err = dispatch(&p, "unit.fetch", &params, &backends(&graph, &sources)).unwrap_err();
        assert_eq!(err.0, INVALID_PARAMS, "{params}");
    }
    let err = dispatch(
        &p,
        "unit.fetch",
        &json!({"unit": "@notes:README.md"}),
        &backends(&graph, &sources),
    )
    .unwrap_err();
    assert_ne!(err.0, METHOD_NOT_FOUND);
    assert_ne!(err.0, INVALID_PARAMS);
    assert_eq!(err.2.unwrap()["reason"], "fetch_unavailable");

    for typo in ["unit.fetch_", "unit.Fetch", "unit.download", "source.fetch"] {
        let err = dispatch(
            &p,
            typo,
            &json!({"unit": "@notes:README.md"}),
            &backends(&graph, &sources),
        )
        .unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND, "{typo}");
    }
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
            fixes: &tephrad::rpc::backend::UnsupportedFixes,
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

// -- unit.fetch: result and error shapes, and the bearer-token gate -------

/// A backend whose `fetch_unit` answers by unit id, and remembers every id
/// it was asked for: what the dispatcher and the transport do with a
/// `unit.fetch` is read from what reaches it and what comes back.
struct FetchSources {
    asked: std::sync::Mutex<Vec<String>>,
}

impl FetchSources {
    fn new() -> Self {
        FetchSources {
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl tephrad::rpc::backend::SourceBackend for FetchSources {
    fn register(
        &self,
        _name: &str,
        _root: &str,
    ) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("not a unit.fetch path")
    }
    fn unregister(&self, _name: &str) -> Result<(), String> {
        unreachable!("not a unit.fetch path")
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        Ok(Vec::new())
    }
    fn rescan(&self, _name: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        unreachable!("not a unit.fetch path")
    }
    fn fetch_unit(
        &self,
        unit: &str,
    ) -> Result<tephrad::remote::fetch::FetchWire, tephrad::remote::fetch::FetchRpcError> {
        use tephrad::remote::fetch::{FetchRpcError, FetchWire};
        self.asked.lock().unwrap().push(unit.to_string());
        match unit {
            "@g:docs/Guide.md" => Ok(FetchWire {
                unit: unit.to_string(),
                path: "/data/remote/g/clone/docs/Guide.md".to_string(),
                state: "cached".to_string(),
                size: 29,
            }),
            "@gdrive:Plan" => Err(FetchRpcError {
                reason: "not_fetchable",
                message: "a Google Doc has no file to download; open it at https://docs.google.com/document/d/doc-1/edit".to_string(),
                url: Some("https://docs.google.com/document/d/doc-1/edit".to_string()),
                kind: Some("gdoc".to_string()),
            }),
            "@g:link" => Err(FetchRpcError {
                reason: "not_fetchable",
                message: "a symbolic link is listed by name only".to_string(),
                url: None,
                kind: Some("symlink".to_string()),
            }),
            _ => Err(FetchRpcError {
                reason: "disk_full",
                message: "the disk is full".to_string(),
                url: None,
                kind: None,
            }),
        }
    }
}

struct SharedFetch(std::sync::Arc<FetchSources>);

impl tephrad::rpc::backend::SourceBackend for SharedFetch {
    fn register(&self, n: &str, r: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        self.0.register(n, r)
    }
    fn unregister(&self, n: &str) -> Result<(), String> {
        self.0.unregister(n)
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        self.0.list()
    }
    fn rescan(&self, n: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        self.0.rescan(n)
    }
    fn fetch_unit(
        &self,
        unit: &str,
    ) -> Result<tephrad::remote::fetch::FetchWire, tephrad::remote::fetch::FetchRpcError> {
        self.0.fetch_unit(unit)
    }
}

fn fetch_dispatch(
    sources: &FetchSources,
    unit: &str,
) -> Result<serde_json::Value, (i64, String, Option<serde_json::Value>)> {
    let graph = NullGraph;
    static NULL_REFACTOR: tephrad::rpc::backend::UnsupportedRefactor =
        tephrad::rpc::backend::UnsupportedRefactor;
    dispatch(
        &proof(),
        "unit.fetch",
        &json!({ "unit": unit }),
        &Backends {
            graph: &graph,
            sources,
            refactor: &NULL_REFACTOR,
            blocks: &tephrad::rpc::backend::UnsupportedBlockIds,
            fixes: &tephrad::rpc::backend::UnsupportedFixes,
        },
    )
}

/// The result of a `unit.fetch` that works is exactly `{unit, path, state,
/// size}`: the unit id as asked, an absolute path, the state and the size
/// in bytes.
#[test]
fn unit_fetch_result_is_unit_path_state_and_size_and_nothing_else() {
    let sources = FetchSources::new();
    let result = fetch_dispatch(&sources, "@g:docs/Guide.md").expect("a fetch that works");
    assert_eq!(
        result,
        json!({
            "unit": "@g:docs/Guide.md",
            "path": "/data/remote/g/clone/docs/Guide.md",
            "state": "cached",
            "size": 29,
        })
    );
    let keys: Vec<&String> = result.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
    assert!(result["path"].as_str().unwrap().starts_with('/'));
    assert!(result["size"].is_u64());
    assert_eq!(sources.asked(), vec!["@g:docs/Guide.md".to_string()]);
}

/// A refusal is a `SOURCE_ERROR` whose `data` names the `reason`; a
/// `not_fetchable` one adds the file's `url` (when it has one) and its
/// `kind`, and no other reason carries either.
#[test]
fn unit_fetch_error_data_is_reason_and_for_not_fetchable_url_and_kind() {
    use tephrad::rpc::protocol::SOURCE_ERROR;
    let sources = FetchSources::new();

    let (code, message, data) = fetch_dispatch(&sources, "@gdrive:Plan").unwrap_err();
    assert_eq!(code, SOURCE_ERROR);
    assert!(message.contains("https://docs.google.com/document/d/doc-1/edit"));
    assert_eq!(
        data.unwrap(),
        json!({
            "reason": "not_fetchable",
            "url": "https://docs.google.com/document/d/doc-1/edit",
            "kind": "gdoc",
        })
    );

    // No URL to give: the key is absent, not null.
    let (code, _, data) = fetch_dispatch(&sources, "@g:link").unwrap_err();
    assert_eq!(code, SOURCE_ERROR);
    assert_eq!(
        data.unwrap(),
        json!({ "reason": "not_fetchable", "kind": "symlink" })
    );

    // Any other refusal: the reason alone.
    let (code, _, data) = fetch_dispatch(&sources, "@g:big.bin").unwrap_err();
    assert_eq!(code, SOURCE_ERROR);
    assert_eq!(data.unwrap(), json!({ "reason": "disk_full" }));
}

fn fetch_request(id: u64, unit: &str) -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "unit.fetch", "params": {"unit": unit}})
}

/// Through the real router: a `unit.fetch` with no token, a wrong one, an
/// empty one or the wrong scheme is 401 and never reaches the backend,
/// whatever it asks for; with the right token the same request is
/// dispatched and answers in the shapes above.
#[tokio::test]
async fn unit_fetch_with_no_or_a_wrong_token_is_401_and_the_right_token_reaches_the_backend() {
    use crate::rpc_fixtures::{spawn_with_token_and_any_sources, MockGraph};

    let sources = std::sync::Arc::new(FetchSources::new());
    let token = "k".repeat(64);
    let server = spawn_with_token_and_any_sources(
        MockGraph::new(),
        SharedFetch(sources.clone()),
        token.as_str(),
    )
    .await;
    let client = reqwest::Client::new();

    // A good id, a Doc, and a malformed request: auth comes first, so none
    // of them is ever told "invalid params" or answered by the backend.
    for body in [
        fetch_request(1, "@g:docs/Guide.md"),
        fetch_request(2, "@gdrive:Plan"),
        json!({"jsonrpc": "2.0", "id": 3, "method": "unit.fetch", "params": {}}),
    ] {
        let none = client
            .post(server.http_url())
            .json(&body)
            .send()
            .await
            .expect("request completes");
        assert_eq!(none.status(), reqwest::StatusCode::UNAUTHORIZED, "{body}");
        let parsed: serde_json::Value = none.json().await.expect("JSON-RPC error body");
        assert!(
            parsed.get("result").is_none()
                && parsed["error"]["code"] != json!(tephrad::rpc::protocol::INVALID_PARAMS),
            "{body}: {parsed}"
        );
        for wrong in ["not-the-right-token", "", &format!("{token}x")] {
            let response = client
                .post(server.http_url())
                .header("Authorization", format!("Bearer {wrong}"))
                .json(&body)
                .send()
                .await
                .expect("request completes");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "{body} with token {wrong:?}"
            );
        }
        let basic = client
            .post(server.http_url())
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .json(&body)
            .send()
            .await
            .expect("request completes");
        assert_eq!(basic.status(), reqwest::StatusCode::UNAUTHORIZED, "{body}");
    }
    assert!(
        sources.asked().is_empty(),
        "a refused request reached the backend: {:?}",
        sources.asked()
    );

    // The right token: the same requests are dispatched.
    let ok: serde_json::Value = client
        .post(server.http_url())
        .bearer_auth(&token)
        .json(&fetch_request(10, "@g:docs/Guide.md"))
        .send()
        .await
        .expect("request completes")
        .json()
        .await
        .expect("JSON-RPC body");
    assert_eq!(ok["id"], json!(10));
    assert_eq!(
        ok["result"],
        json!({
            "unit": "@g:docs/Guide.md",
            "path": "/data/remote/g/clone/docs/Guide.md",
            "state": "cached",
            "size": 29,
        })
    );
    let refused: serde_json::Value = client
        .post(server.http_url())
        .bearer_auth(&token)
        .json(&fetch_request(11, "@gdrive:Plan"))
        .send()
        .await
        .expect("request completes")
        .json()
        .await
        .expect("JSON-RPC body");
    assert_eq!(refused["error"]["data"]["reason"], json!("not_fetchable"));
    assert_eq!(
        refused["error"]["data"]["url"],
        json!("https://docs.google.com/document/d/doc-1/edit")
    );
    assert_eq!(refused["error"]["data"]["kind"], json!("gdoc"));
    assert_eq!(
        sources.asked(),
        vec!["@g:docs/Guide.md".to_string(), "@gdrive:Plan".to_string()]
    );
}
