//! R4/G6: proves HTTP and WebSocket produce byte-identical JSON-RPC
//! result/error payloads for the same method and params, against one
//! running server — a success, a domain error carrying `data`
//! (`ResolveError`), and each of the four standard envelope error
//! codes this server's own docs promise (`-32600`, `-32601`,
//! `-32602`, `-32700`).
//!
//! Both transports funnel through the exact same
//! `server::handle_single_request` (see `src/rpc/server.rs`), so this
//! is a regression test against ever special-casing one transport's
//! response shape — not just a smoke test of either one individually
//! (that's what `rpc_server_http.rs`/`rpc_server_ws.rs` already do).

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephra_graph::{Edge, EdgeTarget, ResolveError, ResolveResult, UnitMetadata};
use tokio_tungstenite::tungstenite::Message;

use crate::rpc_fixtures::{connect_ws, spawn, MockGraph, MockSources};

async fn http_response_bytes(server: &crate::rpc_fixtures::TestServer, request: &Value) -> Vec<u8> {
    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(request)
        .send()
        .await
        .expect("HTTP request must complete");
    response.bytes().await.expect("HTTP response body").to_vec()
}

async fn ws_response_bytes(server: &crate::rpc_fixtures::TestServer, request: &Value) -> Vec<u8> {
    let mut ws = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("WebSocket handshake must succeed");
    ws.send(Message::Text(request.to_string().into()))
        .await
        .expect("send over WS");
    let reply = ws
        .next()
        .await
        .expect("a reply")
        .expect("not a transport error");
    reply.into_text().expect("text frame").as_bytes().to_vec()
}

/// Sends `request` to `server` over both transports and asserts the
/// raw response bytes are identical, byte for byte — not just
/// equal after re-parsing into a `serde_json::Value` (which would
/// hide a key-ordering or whitespace divergence between the two
/// response-construction paths).
async fn assert_transport_parity(server: &crate::rpc_fixtures::TestServer, request: Value) {
    let http_bytes = http_response_bytes(server, &request).await;
    let ws_bytes = ws_response_bytes(server, &request).await;
    assert_eq!(
        http_bytes,
        ws_bytes,
        "HTTP and WS must return byte-identical responses for {request}:\n  http: {}\n  ws:   {}",
        String::from_utf8_lossy(&http_bytes),
        String::from_utf8_lossy(&ws_bytes),
    );
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
async fn http_and_ws_produce_byte_identical_responses_across_every_standard_outcome() {
    let graph = MockGraph::new()
        .with_resolve(
            "[[one]]",
            Ok(ResolveResult {
                unit: UnitMetadata {
                    id: "@vault:notes/one.md".to_string(),
                    source: "vault".to_string(),
                    rel_path: "notes/one.md".to_string(),
                    frontmatter_json: None,
                    tags: vec!["project".to_string()],
                    blocks: vec![],
                    offline: false,
                },
                edges: vec![sample_edge()],
            }),
        )
        .with_resolve(
            "[[missing]]",
            Err(ResolveError::Unresolved {
                source: "vault".to_string(),
                target: "missing".to_string(),
            }),
        );
    let server = spawn(graph, MockSources::new()).await;

    // Success.
    assert_transport_parity(
        &server,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "unit.resolve_link", "params": { "link": "[[one]]" } }),
    )
    .await;

    // A domain error carrying `data` (ResolveError's structured
    // payload), not just a bare code/message.
    assert_transport_parity(
        &server,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "unit.resolve_link", "params": { "link": "[[missing]]" } }),
    )
    .await;

    // -32600: missing "jsonrpc".
    assert_transport_parity(&server, json!({ "id": 3, "method": "graph.get_edges" })).await;

    // -32601: unknown method.
    assert_transport_parity(
        &server,
        json!({ "jsonrpc": "2.0", "id": 4, "method": "unit.delete_everything" }),
    )
    .await;

    // -32602: non-object params.
    assert_transport_parity(
        &server,
        json!({ "jsonrpc": "2.0", "id": 5, "method": "graph.get_edges", "params": [1] }),
    )
    .await;

    // -32700: unparseable input. HTTP sends this as a malformed body;
    // WS sends it as a malformed text frame — different raw inputs,
    // but [`protocol::parse_error_response`] produces the exact same
    // fixed `Value` (always `id: null`) regardless of which transport
    // hit it, so the two responses must still match byte for byte.
    let client = reqwest::Client::new();
    let http_parse_error = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .header("content-type", "application/json")
        .body(b"not json at all".to_vec())
        .send()
        .await
        .expect("HTTP request must complete")
        .bytes()
        .await
        .expect("HTTP response body")
        .to_vec();

    let mut ws = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("WS handshake");
    ws.send(Message::Text("not json at all".into()))
        .await
        .expect("send over WS");
    let ws_parse_error = ws
        .next()
        .await
        .expect("a reply")
        .expect("not a transport error")
        .into_text()
        .expect("text frame")
        .as_bytes()
        .to_vec();

    assert_eq!(
        http_parse_error,
        ws_parse_error,
        "HTTP and WS parse-error responses must be byte-identical:\n  http: {}\n  ws:   {}",
        String::from_utf8_lossy(&http_parse_error),
        String::from_utf8_lossy(&ws_parse_error),
    );
}
