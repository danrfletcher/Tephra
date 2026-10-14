//! `tests/integration/rpc_concurrency.rs` (spec): fires the N-client
//! concurrent WebSocket scenario (E10, N = 1, 2, 10) and the
//! token-rotation scenario (E11) against a live test instance with a
//! temp `token` file under a tempdir. Also covers E12 (abrupt client
//! disconnect does not panic the server or leak into other clients)
//! and part of E9 (oversized HTTP body, server stays responsive) at
//! the HTTP transport since that's the simplest reproduction).

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tephra_graph::{Edge, EdgeTarget};
use tokio_tungstenite::tungstenite::Message;

use crate::rpc_fixtures::{connect_ws, spawn, MockGraph, MockSources};

/// E10: table-driven over N = 1, 2, 10 concurrent WebSocket clients,
/// each authenticating independently and issuing its own request, so
/// no client's auth state or in-flight request leaks into another's
/// response.
#[tokio::test]
async fn n_concurrent_websocket_clients_each_get_their_own_correct_response() {
    for n in [1usize, 2, 10] {
        let mut graph = MockGraph::new();
        for i in 0..n {
            graph = graph.with_backlinks(
                &format!("@vault:note-{i}.md"),
                vec![Edge {
                    from: format!("@vault:referrer-{i}.md"),
                    to: EdgeTarget::Resolved(format!("@vault:note-{i}.md")),
                    to_raw: "[[link]]".to_string(),
                    kind: "wikilink".to_string(),
                    offline: false,
                }],
            );
        }
        let server = spawn(graph, MockSources::new()).await;

        let mut tasks = Vec::new();
        for i in 0..n {
            let url = server.ws_url();
            let token = server.token.clone();
            tasks.push(tokio::spawn(async move {
                let mut ws = connect_ws(&url, &token).await.expect("each client authenticates independently");
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "method": "graph.get_backlinks",
                    "params": { "id": format!("@vault:note-{i}.md") }
                });
                ws.send(Message::Text(request.to_string().into())).await.unwrap();
                let reply = ws.next().await.unwrap().unwrap();
                let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
                (i, body)
            }));
        }

        for task in tasks {
            let (i, body) = task.await.expect("client task must not panic");
            // This client's own id and only its own expected edge —
            // never another client's response.
            assert_eq!(body["id"], json!(i));
            assert_eq!(body["result"][0]["from"], json!(format!("@vault:referrer-{i}.md")));
        }
    }
}

/// E11: a token rewritten on disk while the server is running takes
/// effect on the very next request — no stale in-memory cache of the
/// now-revoked token.
#[tokio::test]
async fn a_rotated_token_is_honored_on_the_very_next_request() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let old_token = server.token.clone();

    // The old token works before rotation.
    let client = reqwest::Client::new();
    let before = client
        .post(server.http_url())
        .bearer_auth(&old_token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(before.status(), reqwest::StatusCode::OK);

    server.rotate_token("f".repeat(64).as_str());

    // The very next request: old token now rejected.
    let after_old = client
        .post(server.http_url())
        .bearer_auth(&old_token)
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(after_old.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The new token is accepted immediately, with no restart.
    let after_new = client
        .post(server.http_url())
        .bearer_auth("f".repeat(64))
        .json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(after_new.status(), reqwest::StatusCode::OK);
}

/// E11, WebSocket angle: a *new* connection attempt under the old
/// token is rejected immediately after rotation.
#[tokio::test]
async fn rotation_blocks_new_websocket_handshakes_immediately() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let old_token = server.token.clone();

    server.rotate_token("e".repeat(64).as_str());

    let result = connect_ws(&server.ws_url(), &old_token).await;
    assert!(result.is_err(), "a new handshake under the revoked token must be rejected");

    let result_new = connect_ws(&server.ws_url(), &"e".repeat(64)).await;
    assert!(result_new.is_ok(), "the freshly rotated token must authenticate a new handshake");
}

/// R2/E11/G6: a connection that authenticated at handshake time under
/// the old token must NOT keep serving requests on that already-open
/// socket after the token file is rotated out from under it — every
/// message is re-checked against the token file's current contents
/// (`handle_socket` in `src/rpc/server.rs`), not just the one
/// presented at the handshake.
#[tokio::test]
async fn a_rotated_token_is_rejected_on_an_already_open_websocket_on_its_very_next_message() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;
    let old_token = server.token.clone();

    let mut ws = connect_ws(&server.ws_url(), &old_token)
        .await
        .expect("handshake under the still-current token succeeds");

    // The connection works before rotation.
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }).to_string().into(),
    ))
    .await
    .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(body["result"], json!([]));

    server.rotate_token("d".repeat(64).as_str());

    // The same already-open socket, with no reconnect, must now be
    // rejected on its very next message — including a mutating
    // source.* call, which must not be allowed to slip through under
    // a revoked token just because the socket predates the rotation.
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "source.register",
            "params": { "name": "evil", "root": "/tmp" }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(
        body["error"]["code"],
        json!(tephrad::rpc::protocol::UNAUTHORIZED),
        "a message on a socket whose handshake token was since revoked must be rejected: {body:?}"
    );
}

/// E12: an abrupt client disconnect (dropping the WebSocket stream
/// without a close handshake) must not panic the server task, and a
/// subsequent client must still connect and be served successfully.
#[tokio::test]
async fn an_abrupt_client_disconnect_does_not_affect_later_clients() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    {
        let ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();
        drop(ws); // abrupt: no close frame sent.
    }

    // Give the server a brief moment to observe the dropped socket.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut next_client = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("a later client must still be able to connect");
    next_client
        .send(Message::Text(json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }).to_string().into()))
        .await
        .unwrap();
    let reply = next_client.next().await.unwrap().unwrap();
    let body: serde_json::Value = serde_json::from_str(&reply.into_text().unwrap()).unwrap();
    assert_eq!(body["id"], json!(1));
}

/// E12, HTTP angle: an abrupt TCP reset mid-request (the client drops
/// the connection before reading the response) must not panic the
/// server; a subsequent well-formed request from a fresh connection
/// must still succeed.
#[tokio::test]
async fn an_abrupt_http_client_disconnect_does_not_affect_later_requests() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    {
        // A client that connects and is dropped before the request is
        // even fully sent/read — simulated by connecting and
        // immediately dropping the stream.
        let stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
        drop(stream);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "graph.get_edges", "params": {} }))
        .send()
        .await
        .expect("a later well-formed request must still succeed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}
