//! `ls` (G1) against the mock RPC server: the exact `unit.list` request
//! that goes out, and stdout / stderr / exit code for an OK result, an
//! offline result, `no such folder`, an unknown source and a dropped
//! connection.
//!
//! Times are rendered in the machine's local zone, so expected lines
//! are built with `render::local_time` (itself tested against
//! `mktime` in `render.rs`) rather than hard-coded.

use serde_json::{json, Value};
use tephractl::cli::Command;
use tephractl::commands;
use tephractl::render::local_time;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, ok_envelope, Behavior, MockServer};

const SOURCE_ERROR: i64 = -32011;
const INVALID_PARAMS: i64 = -32602;

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn ls(source: &str, folder: &str, recursive: bool) -> Command {
    Command::Ls {
        source: source.to_string(),
        folder: folder.to_string(),
        recursive,
    }
}

fn run(server: &MockServer, command: &Command, json: bool) -> tephractl::commands::CommandOutcome {
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client_for(server), &mut stdin)
}

fn row(kind: &str, size: &str, mtime: i64, name: &str) -> String {
    format!("{kind:<10} {size:>10} {:<16} {name}\n", local_time(mtime))
}

fn listing(offline: bool, recursive: bool) -> Value {
    json!({
        "source": "tephra", "folder": "crates", "recursive": recursive,
        "offline": offline, "needs_rescan": false,
        "entries": [
            {"id": "@tephra:crates/rpc", "name": "rpc", "rel_path": "crates/rpc",
             "kind": "folder", "size": 2, "mtime": if offline { Value::Null } else { json!(1_700_000_000) }},
            {"id": "@tephra:crates/rpc/mod.rs", "name": "mod.rs", "rel_path": "crates/rpc/mod.rs",
             "kind": "code", "size": 321, "mtime": 1_700_000_100},
            {"id": "@tephra:crates/NOTES.md", "name": "NOTES.md", "rel_path": "crates/NOTES.md",
             "kind": "prose", "size": 40, "mtime": 1_700_000_200, "title": "Crate notes"},
        ]
    })
}

#[test]
fn ls_sends_unit_list_with_source_folder_and_recursive() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(listing(false, true)),
    }]);

    let outcome = run(&server, &ls("tephra", "crates", true), false);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "unit.list");
    assert_eq!(
        received[0].body["params"],
        json!({"source": "tephra", "folder": "crates", "recursive": true})
    );
    assert_eq!(
        received[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-token")
    );
}

#[test]
fn ls_without_a_folder_asks_for_the_root_non_recursively() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(json!({"source": "tephra", "folder": "", "recursive": false,
                                  "offline": false, "needs_rescan": false, "entries": []})),
    }]);

    let outcome = run(&server, &ls("tephra", "", false), false);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "(empty)\n");
    assert_eq!(
        server.received()[0].body["params"],
        json!({"source": "tephra", "folder": "", "recursive": false})
    );
}

#[test]
fn ls_prints_one_row_per_entry_for_an_ok_result() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(listing(false, false)),
    }]);

    let outcome = run(&server, &ls("tephra", "crates", false), false);

    let expected = [
        row("folder", "2", 1_700_000_000, "rpc"),
        row("code", "321", 1_700_000_100, "mod.rs"),
        row("prose", "40", 1_700_000_200, "NOTES.md  # Crate notes"),
    ]
    .concat();
    assert_eq!(outcome.stdout, expected);
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn ls_recursive_output_is_indented_by_depth() {
    let result = json!({
        "source": "tephra", "folder": "", "recursive": true, "offline": false,
        "entries": [
            {"name": "a", "rel_path": "a", "kind": "folder", "size": 1, "mtime": 1_700_000_000},
            {"name": "b", "rel_path": "a/b", "kind": "folder", "size": 1, "mtime": 1_700_000_000},
            {"name": "c.rs", "rel_path": "a/b/c.rs", "kind": "code", "size": 5, "mtime": 1_700_000_000},
        ]
    });
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(result),
    }]);

    let outcome = run(&server, &ls("tephra", "", true), false);

    let expected = [
        row("folder", "1", 1_700_000_000, "a"),
        row("folder", "1", 1_700_000_000, "  b"),
        row("code", "5", 1_700_000_000, "    c.rs"),
    ]
    .concat();
    assert_eq!(outcome.stdout, expected);
}

#[test]
fn ls_offline_result_starts_with_the_offline_banner() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(listing(true, false)),
    }]);

    let outcome = run(&server, &ls("tephra", "crates", false), false);

    let expected = format!(
        "source tephra is offline (cached listing)\n{}{}{}",
        // A null folder mtime prints as a dash.
        format_args!("{:<10} {:>10} {:<16} {}\n", "folder", "2", "-", "rpc"),
        row("code", "321", 1_700_000_100, "mod.rs"),
        row("prose", "40", 1_700_000_200, "NOTES.md  # Crate notes"),
    );
    assert_eq!(outcome.stdout, expected);
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn ls_json_prints_the_raw_result_pretty_printed() {
    let result = listing(true, false);
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(result.clone()),
    }]);

    let outcome = run(&server, &ls("tephra", "crates", false), true);

    assert_eq!(
        outcome.stdout,
        format!("{}\n", serde_json::to_string_pretty(&result).unwrap())
    );
    let parsed: Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert_eq!(parsed["offline"], true);
    assert_eq!(parsed["entries"][0]["kind"], "folder");
    assert!(!outcome.stdout.contains("source tephra is offline"));
}

#[test]
fn ls_no_such_folder_is_reported_with_a_nonzero_exit() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(SOURCE_ERROR, "no such folder: Nope", None),
    }]);

    let outcome = run(&server, &ls("notes", "Nope", false), false);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "tephractl: no such folder: Nope\n");
}

#[test]
fn ls_unknown_source_is_reported_with_a_nonzero_exit() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(SOURCE_ERROR, "no such source: ghost", None),
    }]);

    let outcome = run(&server, &ls("ghost", "", false), false);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "tephractl: no such source: ghost\n");
}

#[test]
fn ls_invalid_params_from_the_daemon_are_reported() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(INVALID_PARAMS, "\"folder\" must not contain \"..\"", None),
    }]);

    let outcome = run(&server, &ls("notes", "../x", false), false);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        "tephractl: \"folder\" must not contain \"..\"\n"
    );
}

#[test]
fn ls_a_dropped_connection_is_a_transport_error_after_one_retry() {
    // `unit.list` is read-only, so the client retries once; both
    // attempts are dropped.
    let server = MockServer::start(vec![Behavior::Drop, Behavior::Drop]);

    let outcome = run(&server, &ls("notes", "", false), false);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.starts_with("tephractl: transport error:"),
        "{}",
        outcome.stderr
    );
    assert_eq!(server.call_count(), 2);
}

#[test]
fn ls_rejected_token_reports_authentication_failure() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 401,
        body: json!({}),
    }]);

    let outcome = run(&server, &ls("notes", "", false), false);

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, "tephractl: authentication failed\n");
}
