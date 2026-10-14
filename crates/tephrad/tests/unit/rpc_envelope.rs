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
    batch_rejected_response, error_response, success_response, validate_envelope,
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
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

    let with_data = error_response(json!(1), INVALID_PARAMS, "bad params", Some(json!({"field": "link"})));
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
    fn register(&self, _name: &str, _root: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
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
    Backends { graph, sources }
}

#[test]
fn unknown_method_returns_method_not_found() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "unit.delete_everything", &json!({}), &backends(&graph, &sources))
        .unwrap_err();
    assert_eq!(err.0, METHOD_NOT_FOUND);
}

#[test]
fn method_table_is_exactly_the_eight_named_verbs() {
    assert_eq!(METHOD_NAMES.len(), 8);
    for name in ["unit.resolve_link", "graph.get_backlinks", "graph.get_forward_links", "graph.get_edges", "source.register", "source.unregister", "source.list", "source.rescan"] {
        assert!(METHOD_NAMES.contains(&name), "missing method {name}");
    }
}

#[test]
fn unit_resolve_link_without_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "unit.resolve_link", &json!({}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn unit_resolve_link_with_empty_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "unit.resolve_link", &json!({"link": ""}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn unit_resolve_link_with_non_string_link_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "unit.resolve_link", &json!({"link": 123}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_backlinks_without_id_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "graph.get_backlinks", &json!({}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_forward_links_without_id_param_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "graph.get_forward_links", &json!({}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(err.0, INVALID_PARAMS);
}

#[test]
fn graph_get_edges_takes_no_params_and_never_fails_on_params_shape() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let result = dispatch(&p, "graph.get_edges", &json!({"ignored": "extra"}), &backends(&graph, &sources));
    assert!(result.is_ok());
}

#[test]
fn source_register_without_name_or_root_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let missing_both = dispatch(&p, "source.register", &json!({}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(missing_both.0, INVALID_PARAMS);

    let missing_root = dispatch(&p, "source.register", &json!({"name": "vault"}), &backends(&graph, &sources)).unwrap_err();
    assert_eq!(missing_root.0, INVALID_PARAMS);
}

#[test]
fn source_unregister_without_name_is_invalid_params() {
    let graph = NullGraph;
    let sources = NullSources;
    let p = proof();

    let err = dispatch(&p, "source.unregister", &json!({}), &backends(&graph, &sources)).unwrap_err();
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
