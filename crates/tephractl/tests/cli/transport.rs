//! `tests/cli/transport.test` (E "daemon not running"/"dropped
//! connection mid-request", R1): the two transport-level failure
//! modes that aren't server behavior at all, against a real (if
//! deliberately unhelpful) TCP listener, plus (R1) the
//! timeout-vs-dropped-connection and mutating-method-retry fixes:
//! a slow/dropped response to a mutating method (`source.register`,
//! `link.apply_refactor`) must never be silently re-sent, and a
//! timeout must be reported as ambiguous rather than as either a
//! dropped connection (which retries) or a plain transport failure.

use std::net::TcpListener;
use std::time::Duration;

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

/// R1: a timeout on a mutating method (`source.register` runs a full
/// synchronous scan, which on a large source (E1) can routinely take
/// longer than a short per-attempt budget) must be reported as
/// ambiguous — and, crucially, must result in exactly one request:
/// the previous behavior misclassified a timeout as a dropped
/// connection and re-POSTed the same mutating call, which the
/// reviewer reproduced as a real daemon-side duplicate registration.
#[test]
fn source_register_times_out_without_a_retry_and_reports_ambiguous() {
    let server = MockServer::start(vec![Behavior::Slow {
        delay: Duration::from_millis(300),
        status: 200,
        body: ok_envelope(serde_json::json!({"files_seen": 1})),
    }]);
    let client = RpcClient::with_timeouts(
        server.addr.clone(),
        "token".to_string(),
        Duration::from_millis(50),
        Duration::from_millis(50),
        Duration::from_millis(50),
    );

    let result = client.call(
        "source.register",
        serde_json::json!({"name": "big", "root": "/big"}),
    );

    match result {
        Err(RpcCallError::Ambiguous(msg)) => {
            assert!(
                msg.contains("source.register"),
                "ambiguous message should name the method: {msg}"
            );
        }
        other => panic!("expected Ambiguous on timeout, got {other:?}"),
    }
    assert_eq!(
        server.call_count(),
        1,
        "a timed-out mutating call must never be automatically re-sent"
    );
}

/// R1: `link.apply_refactor` must never be automatically re-sent on a
/// dropped connection — a retry after a drop that in fact followed a
/// successful apply would hit the daemon's staleness guard and the
/// CLI would wrongly report "aborted" while files on disk had already
/// been rewritten. This must stay ambiguous, with exactly one request
/// sent, even though the *second* configured behavior below would
/// otherwise serve a clean success (proving the retry genuinely never
/// happens, not just that it would have failed anyway).
#[test]
fn apply_refactor_is_never_retried_on_a_dropped_connection() {
    let server = MockServer::start(vec![
        Behavior::Drop,
        Behavior::Respond {
            status: 200,
            body: ok_envelope(serde_json::json!({"status": "applied", "files_changed": 1})),
        },
    ]);
    let client = RpcClient::new(server.addr.clone(), "token".to_string());

    let result = client.call(
        "link.apply_refactor",
        serde_json::json!({"source": "notes", "old_path": "A.md", "new_path": "B.md"}),
    );

    match result {
        Err(RpcCallError::Ambiguous(msg)) => {
            assert!(msg.contains("link.apply_refactor"));
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    assert_eq!(
        server.call_count(),
        1,
        "link.apply_refactor must not be automatically re-sent after a dropped connection"
    );
}

/// R1: `source.unregister` (mutating, but not long-running) is also
/// never retried on a dropped connection, for the same duplicate-effect
/// reason as `apply_refactor` — only non-mutating methods get the
/// automatic single retry.
#[test]
fn source_unregister_is_never_retried_on_a_dropped_connection() {
    let server = MockServer::start(vec![
        Behavior::Drop,
        Behavior::Respond {
            status: 200,
            body: ok_envelope(serde_json::Value::Null),
        },
    ]);
    let client = RpcClient::new(server.addr.clone(), "token".to_string());

    let result = client.call("source.unregister", serde_json::json!({"name": "notes"}));

    assert!(matches!(result, Err(RpcCallError::Ambiguous(_))));
    assert_eq!(server.call_count(), 1);
}

/// R1: a long-running method (`source.register`) gets a longer
/// per-attempt timeout than an ordinary method, so a daemon that is
/// merely slow (not hung forever) on a big scan succeeds instead of
/// being misreported as ambiguous.
#[test]
fn long_running_methods_get_a_longer_timeout_than_ordinary_methods() {
    let server = MockServer::start(vec![Behavior::Slow {
        delay: Duration::from_millis(150),
        status: 200,
        body: ok_envelope(serde_json::json!({"files_seen": 1})),
    }]);
    let client = RpcClient::with_timeouts(
        server.addr.clone(),
        "token".to_string(),
        Duration::from_millis(50),
        Duration::from_millis(400),
        Duration::from_millis(50),
    );

    let result = client.call(
        "source.register",
        serde_json::json!({"name": "big", "root": "/big"}),
    );

    assert!(
        result.is_ok(),
        "a 150ms-slow register should fit the 400ms long-running budget: {result:?}"
    );
    assert_eq!(server.call_count(), 1);
}

/// R1 (companion to the above): the same 150ms delay against the
/// *short* (50ms) budget used for an ordinary, non-long-running
/// method is reported as a timeout rather than retried. Per R6,
/// `source.list` is read-only, so this must be a plain
/// [`RpcCallError::Timeout`] — not [`RpcCallError::Ambiguous`], which
/// previously told the user not to re-run a harmless read.
#[test]
fn ordinary_methods_use_the_short_timeout_and_report_a_plain_timeout_on_a_slow_response() {
    let server = MockServer::start(vec![Behavior::Slow {
        delay: Duration::from_millis(150),
        status: 200,
        body: ok_envelope(serde_json::json!([])),
    }]);
    let client = RpcClient::with_timeouts(
        server.addr.clone(),
        "token".to_string(),
        Duration::from_millis(50),
        Duration::from_millis(400),
        Duration::from_millis(50),
    );

    let result = client.call("source.list", serde_json::json!({}));

    assert!(matches!(result, Err(RpcCallError::Timeout(_))));
    assert_eq!(server.call_count(), 1);
}

/// R6: a timeout on a read-only method must not use the
/// mutating-call "don't retry blindly" wording — it tells the user
/// plainly that the daemon didn't respond, without implying it is
/// unsafe to re-run `source.list` (or any other read-only command).
#[test]
fn read_only_timeout_message_is_plain_not_ambiguous() {
    let server = MockServer::start(vec![Behavior::Slow {
        delay: Duration::from_millis(150),
        status: 200,
        body: ok_envelope(serde_json::json!([])),
    }]);
    let client = RpcClient::with_timeouts(
        server.addr.clone(),
        "token".to_string(),
        Duration::from_millis(50),
        Duration::from_millis(400),
        Duration::from_millis(50),
    );

    let result = client.call("source.list", serde_json::json!({}));

    match result {
        Err(RpcCallError::Timeout(msg)) => {
            assert!(
                msg.contains("source.list"),
                "timeout message should name the method: {msg}"
            );
            assert!(
                !msg.contains("retrying") && !msg.contains("blindly"),
                "a read-only timeout must not carry the mutating-call \"don't retry blindly\" wording: {msg}"
            );
        }
        other => panic!("expected a plain Timeout, got {other:?}"),
    }
}
