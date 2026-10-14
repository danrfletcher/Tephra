//! `tests/cli/source_commands.test` (G8): `source add`/`remove`/
//! `list` against the mock RPC server, checking the exact
//! method/params sent, the rendered human-readable output, the
//! `--json` raw-output path, and the offline-marker/empty-table
//! edge cases.

use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{load_fixture, Behavior, MockServer};

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

#[test]
fn source_add_sends_register_with_name_and_path_and_renders_stats() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert!(outcome.stdout.contains("Registered source \"notes\""));
    assert!(outcome.stdout.contains("units written: 39"));

    let received = server.received();
    assert_eq!(received[0].body["method"], "source.register");
    assert_eq!(received[0].body["params"]["name"], "notes");
    assert_eq!(received[0].body["params"]["root"], "/home/dan/Notes");
}

#[test]
fn source_add_json_flag_prints_raw_result() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(&outcome.stdout).expect("valid JSON output");
    assert_eq!(parsed["units_written"], 39);
}

#[test]
fn source_add_duplicate_name_error_is_reported_clearly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_duplicate_error"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("already registered"));
}

#[test]
fn source_remove_sends_unregister_and_confirms_removal() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_unregister_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "notes".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert!(outcome.stdout.contains("Removed source \"notes\""));
    assert_eq!(server.received()[0].body["method"], "source.unregister");
    assert_eq!(server.received()[0].body["params"]["name"], "notes");
}

#[test]
fn source_remove_on_an_unknown_name_reports_not_found() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_unregister_not_found"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "ghost".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("not found"));
}

#[test]
fn source_list_renders_a_table_with_offline_marker() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.stdout.contains("notes"));
    assert!(outcome.stdout.contains("old-vault"));
    assert!(outcome.stdout.contains("(offline)"));
    assert_eq!(server.received()[0].body["method"], "source.list");
}

#[test]
fn source_list_with_zero_sources_says_so_plainly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_empty"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "no sources registered\n");
}

#[test]
fn status_reports_daemon_address_and_source_table_without_fabricating_uptime() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.stdout.contains(&server.addr));
    assert!(outcome.stdout.contains("notes"));
    assert!(outcome.stdout.contains("unavailable"));
}
