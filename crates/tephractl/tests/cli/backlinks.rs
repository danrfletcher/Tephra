//! `tests/cli/backlinks.rs` (G16, PR-13.F1): `backlinks <id>` and the
//! block line of `link resolve`, against the mock RPC server: the
//! method and params sent, rendered and `--json` output byte for byte
//! against golden fixtures, the empty list, the ambiguity and dangling
//! renderings of a block link, and exit codes. Malformed ids are
//! refused by the argument parser, so no request is made.

use tephractl::cli::{parse_args, Action, Command};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

const BLOCK_ID: &str = "@tephra:crates/tephrad/src/conflict.rs#^k3f9a2";

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn run_backlinks(server: &MockServer, id: &str, json: bool) -> commands::CommandOutcome {
    let client = client_for(server);
    let mut stdin: &[u8] = &[];
    commands::execute(
        &Command::Backlinks { id: id.to_string() },
        json,
        &client,
        &mut stdin,
    )
}

fn run_resolve(server: &MockServer, target: &str, json: bool) -> commands::CommandOutcome {
    let client = client_for(server);
    let mut stdin: &[u8] = &[];
    commands::execute(
        &Command::LinkResolve {
            target: target.to_string(),
            default_source: Some("notes".to_string()),
        },
        json,
        &client,
        &mut stdin,
    )
}

fn respond(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

#[test]
fn backlinks_calls_graph_get_backlinks_with_the_id() {
    let server = respond("backlinks_block");
    let outcome = run_backlinks(&server, BLOCK_ID, false);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let sent = server.received();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body["method"], "graph.get_backlinks");
    assert_eq!(sent[0].body["params"]["id"], BLOCK_ID);
}

#[test]
fn backlinks_renders_each_row_with_the_unit_and_line_exactly() {
    let server = respond("backlinks_block");
    let outcome = run_backlinks(&server, BLOCK_ID, false);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("backlinks_rendered"));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn backlinks_json_flag_prints_the_raw_array_byte_for_byte() {
    let server = respond("backlinks_block");
    let outcome = run_backlinks(&server, BLOCK_ID, true);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("backlinks_block_json"));
}

#[test]
fn backlinks_for_an_id_nothing_links_to_is_an_empty_list_and_exit_zero() {
    let server = respond("backlinks_empty");
    let outcome = run_backlinks(&server, "@notes:Nowhere.md", false);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "no backlinks\n");

    let server = respond("backlinks_empty");
    let outcome = run_backlinks(&server, "@notes:Nowhere.md", true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("backlinks_empty_json"));
}

#[test]
fn backlinks_shows_control_characters_in_link_text_as_question_marks() {
    let mut body = load_fixture("backlinks_block");
    body["result"] = serde_json::json!([{
        "from": "@notes:Evil\u{1b}[2J.md",
        "to": {"resolved": "@notes:A.md"},
        "to_raw": "A\u{7}",
        "kind": "wikilink",
        "offline": false,
        "line": 1
    }]);
    let server = MockServer::start(vec![Behavior::Respond { status: 200, body }]);
    let outcome = run_backlinks(&server, "@notes:A.md", false);
    assert_eq!(outcome.stdout, "@notes:Evil?[2J.md:1  [wikilink] A?\n");
}

#[test]
fn backlinks_daemon_error_exits_nonzero_with_the_message_on_stderr() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32000, "graph not ready", None),
    }]);
    let outcome = run_backlinks(&server, "@notes:A.md", false);
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("graph not ready"),
        "{}",
        outcome.stderr
    );
}

// ---- argument parsing ------------------------------------------------

fn parse(args: &[&str]) -> Result<tephractl::cli::ParsedArgs, String> {
    match parse_args(args) {
        Ok(Action::Run(parsed)) => Ok(parsed),
        Ok(Action::Help) => panic!("unexpected help"),
        Err(e) => Err(e.0),
    }
}

#[test]
fn backlinks_args_accept_a_unit_id_and_a_block_id() {
    for id in [
        "@notes:Tephra test.md",
        "@s:unicode/café notes.md",
        BLOCK_ID,
        "@s:a.md#^my-note-2",
        "@notes:C# notes.md",
        "@notes:#autosave#",
        "@notes:dir/C# notes.md#^k3f9a2",
        // Not a block (a heading), so the daemon reads it as a unit id.
        "@s:a.md#Heading",
    ] {
        let parsed = parse(&["backlinks", id]).unwrap();
        assert_eq!(parsed.command, Command::Backlinks { id: id.to_string() });
        assert!(!parsed.json);
    }
    let parsed = parse(&["--json", "backlinks", BLOCK_ID, "--addr", "127.0.0.1:9"]).unwrap();
    assert!(parsed.json);
    assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:9"));
}

#[test]
fn backlinks_args_refuse_a_malformed_id_with_a_clear_error() {
    for bad in [
        "notes:Tephra test.md",
        "@notes",
        "@:a.md",
        "@s:",
        "@s:a.md#^",
        "@s:a.md#^bad id",
        "@s:a.md#^a/b",
        "",
    ] {
        let err = parse(&["backlinks", bad]).unwrap_err();
        assert!(err.contains("backlinks needs an id"), "{bad:?}: {err}");
    }
    assert!(parse(&["backlinks"])
        .unwrap_err()
        .contains("requires an <id>"));
    assert!(parse(&["backlinks", "@s:a.md", "@s:b.md"])
        .unwrap_err()
        .contains("one id"));
    assert!(parse(&["backlinks", "@s:a.md", "--bogus"])
        .unwrap_err()
        .contains("unrecognized argument"));
}

// ---- link resolve: the block --------------------------------------

#[test]
fn link_resolve_shows_a_moved_block_exactly() {
    let server = respond("resolve_block_moved");
    let outcome = run_resolve(
        &server,
        "[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]",
        false,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        load_expected("resolve_block_moved_rendered")
    );
    let sent = server.received();
    assert_eq!(sent[0].body["method"], "unit.resolve_link");
    assert_eq!(
        sent[0].body["params"]["link"],
        "[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]"
    );
}

#[test]
fn link_resolve_shows_a_block_found_in_the_named_file_without_a_moved_note() {
    let server = respond("resolve_block_here");
    let outcome = run_resolve(
        &server,
        "[[@tephra:crates/tephrad/src/conflict.rs#^k3f9a2]]",
        false,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("resolve_block_here_rendered"));
}

#[test]
fn link_resolve_with_a_null_block_prints_no_block_line() {
    let server = respond("resolve_block_null");
    let outcome = run_resolve(
        &server,
        "[[@tephra:crates/tephrad/src/conflict.rs#ConflictReason]]",
        false,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("resolve_block_null_rendered"));
    assert!(!outcome.stdout.contains("Block:"));
}

#[test]
fn link_resolve_json_prints_the_block_byte_for_byte() {
    let server = respond("resolve_block_moved");
    let outcome = run_resolve(
        &server,
        "[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]",
        true,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("resolve_block_moved_json"));
}

#[test]
fn link_resolve_ambiguous_block_lists_every_place_and_exits_nonzero() {
    let server = respond("resolve_block_ambiguous");
    let outcome = run_resolve(&server, "[[@tephra:registry.rs#^k3f9a2]]", false);
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        load_expected("resolve_block_ambiguous_rendered")
    );
}

#[test]
fn link_resolve_dangling_block_renders_unresolved_and_exits_nonzero() {
    let server = respond("resolve_block_unresolved");
    let outcome = run_resolve(&server, "[[@tephra:registry.rs#^k3f9a2]]", false);
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        load_expected("resolve_block_unresolved_rendered")
    );
}

#[test]
fn backlinks_sends_a_unit_id_whose_path_contains_a_hash_unchanged() {
    let server = respond("backlinks_empty");
    let outcome = run_backlinks(&server, "@notes:C# notes.md", false);
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "no backlinks\n");
    assert_eq!(
        server.received()[0].body["params"]["id"],
        "@notes:C# notes.md"
    );
}
