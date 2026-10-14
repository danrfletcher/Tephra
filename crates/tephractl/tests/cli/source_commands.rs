//! `tests/cli/source_commands.test` (G8/R4): `source add`/`remove`/
//! `list` and `status` against the mock RPC server. Rendered and
//! `--json` output is asserted byte-for-byte (`assert_eq!`) against
//! golden fixtures under `tests/fixtures/expected/` — not a
//! substring/`contains` check — per the spec's "asserting rendered
//! table output and `--json` output byte-for-byte against fixtures"
//! (R4), alongside the exact method/params sent and the exit code.

use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{load_expected, load_expected_templated, load_fixture, Behavior, MockServer};

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
    assert_eq!(outcome.stdout, load_expected("source_add_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(received[0].body["method"], "source.register");
    assert_eq!(received[0].body["params"]["name"], "notes");
    assert_eq!(received[0].body["params"]["root"], "/home/dan/Notes");
}

#[test]
fn source_add_json_flag_prints_raw_result_byte_for_byte() {
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
    assert_eq!(outcome.stdout, load_expected("source_add_json"));
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

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        "tephractl: a source named \"notes\" is already registered\n"
    );
    assert_eq!(outcome.stdout, "");
}

#[test]
fn source_remove_sends_list_then_unregister_and_confirms_removal() {
    // T1: source_remove now checks client-side via `source.list`
    // before calling `source.unregister`, so the mock server needs
    // two scripted responses — "notes" exists in the list, so the
    // unregister call follows.
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_list_two_sources"),
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_success"),
        },
    ]);
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
    assert_eq!(outcome.stdout, load_expected("source_remove_rendered"));
    let received = server.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].body["method"], "source.list");
    assert_eq!(received[1].body["method"], "source.unregister");
    assert_eq!(received[1].body["params"]["name"], "notes");
}

#[test]
fn source_remove_on_an_unknown_name_reports_not_found_without_calling_unregister() {
    // T1: tephrad's `source.unregister` is idempotent and can never
    // itself report "not found", so `source remove` must check
    // client-side against `source.list` and never even issue the
    // `source.unregister` RPC for a name that isn't registered.
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
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

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, "source \"ghost\" not found\n");
    assert_eq!(outcome.stdout, "");
    let received = server.received();
    assert_eq!(
        received.len(),
        1,
        "must not call source.unregister for an unknown name"
    );
    assert_eq!(received[0].body["method"], "source.list");
}

#[test]
fn source_list_renders_a_table_with_offline_marker_exactly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_list_rendered"));
    assert_eq!(server.received()[0].body["method"], "source.list");
}

#[test]
fn source_list_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_list_json"));
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
    assert_eq!(
        outcome.stdout,
        load_expected_templated("status_rendered.template", &server.addr)
    );
}

#[test]
fn status_json_flag_prints_address_uptime_and_sources_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected_templated("status_json.template", &server.addr)
    );
}

#[test]
fn status_with_zero_sources_says_so_plainly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_empty"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.stdout.contains("no sources registered"));
}
