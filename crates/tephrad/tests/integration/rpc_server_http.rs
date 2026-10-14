//! `tests/integration/rpc_server_http.rs` (spec): spins up a real
//! `tephrad::rpc` server bound to an ephemeral loopback port, backed
//! by `rpc_fixtures::MockGraph`/`MockSources`, and exercises
//! `unit.resolve_link` and `graph.get_backlinks` end-to-end over real
//! HTTP with `reqwest` — the auth gate, success path, and error path.

use serde_json::json;
use tephra_graph::{Edge, EdgeTarget, ResolveError, ResolveResult, UnitMetadata};

use crate::rpc_fixtures::{spawn, MockGraph, MockSources};

fn sample_unit() -> UnitMetadata {
    UnitMetadata {
        id: "@vault:notes/one.md".to_string(),
        source: "vault".to_string(),
        rel_path: "notes/one.md".to_string(),
        frontmatter_json: None,
        tags: vec!["project".to_string()],
        blocks: vec![],
        offline: false,
    }
}

fn sample_edge() -> Edge {
    Edge {
        from: "@vault:notes/two.md".to_string(),
        to: EdgeTarget::Resolved("@vault:notes/one.md".to_string()),
        to_raw: "[[one]]".to_string(),
        kind: "wikilink".to_string(),
        offline: false,
    }
}

#[tokio::test]
async fn request_with_no_authorization_header_is_rejected_with_401_and_jsonrpc_error_body() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "graph.get_edges",
            "params": {}
        }))
        .send()
        .await
        .expect("request should complete, not hang or crash");

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("valid JSON body");
    assert!(body.get("error").is_some(), "must be an unambiguous JSON-RPC error, not a bare 200");
    assert!(body.get("result").is_none());
}

#[tokio::test]
async fn request_with_wrong_token_is_rejected() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth("not-the-right-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unit_resolve_link_returns_the_mocked_resolution_over_http() {
    let graph = MockGraph::new().with_resolve(
        "[[one]]",
        Ok(ResolveResult { unit: sample_unit(), edges: vec![sample_edge()] }),
    );
    let server = spawn(graph, MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "unit.resolve_link",
            "params": { "link": "[[one]]" }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], json!("req-1"));
    assert_eq!(body["result"]["unit"]["id"], json!("@vault:notes/one.md"));
    assert_eq!(body["result"]["edges"][0]["to"], json!({"resolved": "@vault:notes/one.md"}));
}

#[tokio::test]
async fn unit_resolve_link_surfaces_a_resolve_error_as_a_jsonrpc_error() {
    let graph = MockGraph::new().with_resolve(
        "[[missing]]",
        Err(ResolveError::Unresolved { source: "vault".to_string(), target: "missing".to_string() }),
    );
    let server = spawn(graph, MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "unit.resolve_link",
            "params": { "link": "[[missing]]" }
        }))
        .send()
        .await
        .unwrap();

    // The auth gate passed and the request was well-formed, so this
    // is still HTTP 200 with a JSON-RPC-level error body — exactly
    // the json_200 path every successfully-dispatched call (success
    // or failure) takes.
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body.get("result").is_none());
    assert!(body["error"]["code"].is_i64());
}

#[tokio::test]
async fn graph_get_backlinks_returns_the_mocked_edges_over_http() {
    let graph = MockGraph::new().with_backlinks("@vault:notes/one.md", vec![sample_edge()]);
    let server = spawn(graph, MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "graph.get_backlinks",
            "params": { "id": "@vault:notes/one.md" }
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["result"].as_array().unwrap().len(), 1);
    assert_eq!(body["result"][0]["from"], json!("@vault:notes/two.md"));
}

#[tokio::test]
async fn malformed_jsonrpc_envelope_returns_the_standard_error_codes() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let client = reqwest::Client::new();

    // Missing "jsonrpc" -> -32600.
    let r1 = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "id": 1, "method": "graph.get_edges" }))
        .send()
        .await
        .unwrap();
    let b1: serde_json::Value = r1.json().await.unwrap();
    assert_eq!(b1["error"]["code"], json!(-32600));

    // Unknown method -> -32601.
    let r2 = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "unit.delete_everything" }))
        .send()
        .await
        .unwrap();
    let b2: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(b2["error"]["code"], json!(-32601));

    // Non-object params -> -32602.
    let r3 = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": [1] }))
        .send()
        .await
        .unwrap();
    let b3: serde_json::Value = r3.json().await.unwrap();
    assert_eq!(b3["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn empty_body_post_returns_a_parse_error_not_a_hang_or_crash() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let client = reqwest::Client::new();

    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .header("content-type", "application/json")
        .body(Vec::<u8>::new())
        .send()
        .await
        .expect("must not hang or crash the server task");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], json!(-32700));
}

#[tokio::test]
async fn oversized_body_is_rejected_and_the_server_stays_responsive() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let client = reqwest::Client::new();

    let oversized = vec![b'a'; tephrad::rpc::server::MAX_BODY_BYTES + 1];
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .header("content-type", "application/octet-stream")
        .body(oversized)
        .send()
        .await
        .expect("request must complete, not hang");

    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    // The server must still answer a normal, well-formed request
    // right afterward.
    let followup = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(followup.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn source_list_and_source_register_route_through_the_mock_registry_over_http() {
    let sources = MockSources::new()
        .with_list(Ok(vec![]))
        .with_register(
            "vault",
            Ok(tephrad::rpc::backend::ScanStatsWire {
                files_seen: 3,
                files_ignored: 0,
                units_written: 3,
                links_written: 1,
                tags_written: 1,
                blocks_written: 0,
            }),
        );
    let server = spawn(MockGraph::new(), sources).await;
    let client = reqwest::Client::new();

    let list = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "source.list", "params": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(list["result"], json!([]));

    let register = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "source.register",
            "params": { "name": "vault", "root": "/tmp/whatever" }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(register["result"]["units_written"], json!(3));
}
