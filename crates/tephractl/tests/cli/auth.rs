//! `tests/cli/auth.test` (P2/F5): token loading from a fixture path,
//! and — against the mock RPC server — that every request this
//! crate sends actually carries the loaded token as a bearer header,
//! and that a server-side 401 is reported as "authentication
//! failed" rather than a generic transport error.

use tephractl::commands;
use tephractl::token::{load_token, TokenError};
use tephractl::transport::RpcClient;

use crate::support::{load_fixture, Behavior, MockServer};

#[test]
fn load_token_reads_and_trims_a_fixture_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "deadbeef1234\n").unwrap();

    let token = load_token(&path).expect("token should load");
    assert_eq!(token, "deadbeef1234");
}

#[test]
fn load_token_on_a_missing_file_is_the_distinct_missing_variant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist");
    let err = load_token(&path).unwrap_err();
    assert!(matches!(err, TokenError::Missing(_)));
}

#[test]
fn every_rpc_call_attaches_the_loaded_token_as_a_bearer_header() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "my-secret-token\n").unwrap();
    let token = load_token(&token_path).unwrap();

    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_empty"),
    }]);
    let client = RpcClient::new(server.addr.clone(), token);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &tephractl::cli::Command::SourceList,
        false,
        &client,
        &mut stdin,
    );
    assert_eq!(outcome.exit_code, 0);

    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].headers.get("authorization").map(String::as_str),
        Some("Bearer my-secret-token")
    );
}

#[test]
fn a_401_response_is_reported_as_authentication_failed() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 401,
        body: load_fixture("unauthorized_error"),
    }]);
    let client = RpcClient::new(server.addr.clone(), "wrong-token".to_string());

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &tephractl::cli::Command::SourceList,
        false,
        &client,
        &mut stdin,
    );

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("authentication failed"));
    assert!(
        !outcome.stderr.to_lowercase().contains("panic"),
        "must not surface a stack trace/panic for a 401"
    );
}
