//! `tests/cli/transport.test` (E "daemon not running"/"dropped
//! connection mid-request"): the two transport-level failure modes
//! that aren't server behavior at all, against a real (if
//! deliberately unhelpful) TCP listener.

use std::net::TcpListener;

use tephractl::transport::{RpcCallError, RpcClient};

use crate::support::{ok_envelope, Behavior, MockServer};

/// Binds then immediately drops a listener, freeing the port while
/// guaranteeing nothing else grabs it before the test's `call()`
/// below runs (close enough to "connection refused" in practice on
/// loopback addresses, and avoids depending on a genuinely unused
/// port number guess).
fn unreachable_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    drop(listener);
    addr
}

#[test]
fn daemon_not_running_is_reported_clearly_with_no_retry_needed() {
    let addr = unreachable_addr();
    let client = RpcClient::new(addr, "token".to_string());

    let result = client.call("source.list", serde_json::json!({}));
    match result {
        Err(RpcCallError::DaemonNotRunning) => {}
        other => panic!("expected DaemonNotRunning, got {other:?}"),
    }
    assert_eq!(result.unwrap_err().to_string(), "tephrad is not running");
}

#[test]
fn a_dropped_connection_is_retried_once_then_succeeds() {
    let server = MockServer::start(vec![
        Behavior::Drop,
        Behavior::Respond {
            status: 200,
            body: ok_envelope(serde_json::json!([])),
        },
    ]);
    let client = RpcClient::new(server.addr.clone(), "token".to_string());

    let result = client.call("source.list", serde_json::json!({}));
    assert!(
        result.is_ok(),
        "expected the automatic retry to succeed: {result:?}"
    );
    assert_eq!(server.call_count(), 2, "exactly one retry, not more");
}

#[test]
fn a_dropped_connection_that_fails_again_on_retry_is_a_transport_error() {
    let server = MockServer::start(vec![Behavior::Drop, Behavior::Drop]);
    let client = RpcClient::new(server.addr.clone(), "token".to_string());

    let result = client.call("source.list", serde_json::json!({}));
    match result {
        Err(RpcCallError::Transport(_)) => {}
        other => panic!("expected Transport error after retry also failed, got {other:?}"),
    }
    assert_eq!(server.call_count(), 2);
}

#[test]
fn each_rpc_client_is_an_independent_fresh_connection() {
    // Two separate invocations (as two separate `RpcClient`s, the
    // way two separate `tephractl` process runs would each build
    // their own) against the same server must not share any state:
    // the second client with the *wrong* token is still rejected
    // even though the first, correctly-tokened client just
    // succeeded moments before.
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: ok_envelope(serde_json::json!([])),
        },
        Behavior::Respond {
            status: 401,
            body: serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"unauthorized"}}),
        },
    ]);

    let first = RpcClient::new(server.addr.clone(), "right-token".to_string());
    assert!(first.call("source.list", serde_json::json!({})).is_ok());

    let second = RpcClient::new(server.addr.clone(), "wrong-token".to_string());
    match second.call("source.list", serde_json::json!({})) {
        Err(RpcCallError::Unauthorized) => {}
        other => panic!("expected the second, differently-tokened client to be rejected independently, got {other:?}"),
    }
}
