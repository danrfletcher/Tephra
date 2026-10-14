//! `tests/integration/rpc_server_ws.rs` (spec): the WebSocket
//! transport-parity counterpart of `rpc_server_http.rs` — same mocked
//! fixture, driven over `tokio-tungstenite`: asserts the WebSocket
//! auth handshake, request/response parity with the HTTP test, and
//! delivery of a synthetic `graph:changed` push.

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::rpc_fixtures::{connect_ws, spawn, MockGraph, MockSources};

#[tokio::test]
async fn ws_upgrade_without_authorization_header_is_rejected() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    // A plain, unauthenticated upgrade attempt: `connect_async` itself
    // returns an error for the non-101 (401) handshake response,
    // rather than this server panicking or silently accepting it.
    let request = server.ws_url().into_client_request().unwrap();
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(
        result.is_err(),
        "an unauthenticated WebSocket handshake must be rejected"
    );
}

#[tokio::test]
async fn ws_upgrade_with_wrong_token_is_rejected() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let result = connect_ws(&server.ws_url(), "definitely-the-wrong-token").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn an_authenticated_websocket_serves_requests_with_the_same_results_as_http() {
    use tephra_graph::{Edge, EdgeTarget};

    let graph = MockGraph::new().with_backlinks(
        "@vault:notes/one.md",
        vec![Edge {
            from: "@vault:notes/two.md".to_string(),
            to: EdgeTarget::Resolved("@vault:notes/one.md".to_string()),
            to_raw: "[[one]]".to_string(),
            kind: "wikilink".to_string(),
            offline: false,
        }],
    );
    let server = spawn(graph, MockSources::new()).await;

    let mut ws = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("authenticated handshake must succeed");

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "graph.get_backlinks",
        "params": { "id": "@vault:notes/one.md" }
    });
    ws.send(Message::Text(request.to_string().into()))
        .await
        .unwrap();

    let reply = ws
        .next()
        .await
        .expect("a reply")
        .expect("not a transport error");
    let text = reply.into_text().expect("text frame");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Transport parity: byte-for-byte the same JSON-RPC result shape
    // `rpc_server_http.rs`'s `graph_get_backlinks_returns_the_mocked_edges_over_http`
    // asserts for an identical method/params pair over plain HTTP.
    assert_eq!(body["id"], json!(1));
    assert_eq!(body["result"][0]["from"], json!("@vault:notes/two.md"));
    assert_eq!(
        body["result"][0]["to"],
        json!({"resolved": "@vault:notes/one.md"})
    );
}

#[tokio::test]
async fn malformed_envelope_over_websocket_gets_the_same_error_code_as_over_http() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let mut ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();

    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "nope" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let reply = ws.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(body["error"]["code"], json!(-32601));
}

/// E6/E8-adjacent for the WebSocket transport: an unparseable text
/// frame returns a parse error over the socket, never a panic or an
/// abrupt disconnect.
#[tokio::test]
async fn invalid_json_frame_returns_a_parse_error_without_closing_the_socket() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let mut ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();

    ws.send(Message::Text("not json at all".into()))
        .await
        .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(body["error"]["code"], json!(-32700));

    // The socket must still be usable afterward.
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 9, "method": "graph.get_edges", "params": {} })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let second = ws.next().await.unwrap().unwrap();
    let second_body: serde_json::Value =
        serde_json::from_str(&second.into_text().unwrap()).unwrap();
    assert_eq!(second_body["id"], json!(9));
}

/// G6: "an authenticated WebSocket connection can receive
/// asynchronous `graph:changed` push notifications without issuing a
/// request first (server-initiated message, not a response to a
/// client call)".
#[tokio::test]
async fn an_authenticated_connection_receives_a_graph_changed_push_with_no_request_sent() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let mut ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();

    // Give the server a moment to actually register this connection's
    // broadcast subscription before pushing, since the handshake
    // completing on the client side doesn't guarantee the server-side
    // task has reached its `subscribe()` call yet.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    server.broadcast_graph_changed(json!({ "source": "vault", "paths": ["notes/one.md"] }));

    let pushed = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("must receive the push within the timeout, not hang")
        .expect("stream not closed")
        .expect("not a transport error");

    let body: serde_json::Value = serde_json::from_str(&pushed.into_text().unwrap()).unwrap();
    // A JSON-RPC Notification: no "id" field at all, distinguishing it
    // from a reply to a request this test never sent.
    assert!(body.get("id").is_none());
    assert_eq!(body["method"], json!("graph:changed"));
    assert_eq!(body["params"]["source"], json!("vault"));
}

/// R3/E9: the WebSocket transport must enforce the same body-size cap
/// as `POST /rpc` (`MAX_BODY_BYTES`), not axum/tungstenite's own much
/// larger defaults (64 MiB message / 16 MiB frame) — otherwise a WS
/// client could make the server buffer and fully `serde_json`-parse a
/// body far larger than the HTTP cap ever allows.
#[tokio::test]
async fn an_oversized_websocket_message_is_rejected_and_the_server_stays_responsive() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let mut ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();

    // A single text frame bigger than this server's configured
    // `max_message_size` (set to `MAX_BODY_BYTES`, same as the HTTP
    // cap) — tungstenite's own *client*-side default (64 MiB) is well
    // above this, so the client itself has no trouble constructing
    // it; it is this server's own configured limit that must reject
    // it. The server can reject a message this large either after a
    // full message is received, or mid-frame while still streaming
    // it in (closing the TCP connection outright) — either way the
    // client-side `send` itself may surface the rejection as a
    // transport error rather than this call succeeding, so both
    // outcomes count as "rejected" here.
    let oversized = "a".repeat(tephrad::rpc::server::MAX_BODY_BYTES + 1);
    let send_result = ws.send(Message::Text(oversized.into())).await;

    if send_result.is_ok() {
        // The server tears down a connection that violates its own
        // message-size limit rather than silently buffering/parsing
        // it; this socket must end (close frame or the stream simply
        // closing), never hang and never reply as if nothing happened.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
        match outcome {
            Err(_) => panic!("the server must not hang on an oversized WS message"),
            Ok(None) => {}         // stream ended
            Ok(Some(Err(_))) => {} // transport-level error/close
            Ok(Some(Ok(Message::Close(_)))) => {}
            Ok(Some(Ok(other))) => {
                panic!("expected the oversized-message connection to close, got: {other:?}")
            }
        }
    }

    // A fresh connection must still be served normally right
    // afterward — the whole server, not just this one connection,
    // stays responsive.
    let mut next_ws = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("a later client must still be able to connect");
    next_ws
        .send(Message::Text(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply = next_ws.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(body["id"], json!(1));
}
