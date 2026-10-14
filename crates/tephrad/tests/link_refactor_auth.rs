//! F5 / spec: "`link.preview_refactor` and `link.apply_refactor` both
//! return the standard auth error for missing/invalid token, reusing
//! the PR-6 auth test harness and fixture token" — this reuses the
//! exact same `rpc_fixtures::spawn`/`MockGraph`/`MockSources` harness
//! and assertions `tests/integration/rpc_server_http.rs` already uses
//! for every other method, so a passing result here proves the two
//! new methods sit behind the identical compile-time-enforced
//! [`tephrad::rpc::auth::Authenticated`] gate (see that module's own
//! doc comment) rather than a new, separately-reasoned-about check.

#[path = "rpc_fixtures/mod.rs"]
mod rpc_fixtures;

use serde_json::json;

use rpc_fixtures::{spawn, MockGraph, MockSources};

#[tokio::test]
async fn preview_refactor_with_no_authorization_header_is_rejected_with_401() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "link.preview_refactor",
            "params": {"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}
        }))
        .send()
        .await
        .expect("request should complete, not hang or crash");

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("valid JSON body");
    assert!(
        body.get("error").is_some(),
        "must be an unambiguous JSON-RPC error, not a bare 200"
    );
    assert!(body.get("result").is_none());
}

#[tokio::test]
async fn preview_refactor_with_wrong_token_is_rejected_with_401() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth("not-the-right-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "link.preview_refactor",
            "params": {"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn apply_refactor_with_no_authorization_header_is_rejected_with_401() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "link.apply_refactor",
            "params": {"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}
        }))
        .send()
        .await
        .expect("request should complete, not hang or crash");

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("valid JSON body");
    assert!(
        body.get("error").is_some(),
        "must be an unambiguous JSON-RPC error, not a bare 200"
    );
    assert!(body.get("result").is_none());
}

#[tokio::test]
async fn apply_refactor_with_wrong_token_is_rejected_with_401() {
    let server = spawn(MockGraph::new(), MockSources::new()).await;

    let client = reqwest::Client::new();
    let response = client
        .post(server.http_url())
        .bearer_auth("not-the-right-token")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "link.apply_refactor",
            "params": {"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}
