//! `tests/cli/fetch_commands.rs` (PR-7 G10): `tephractl fetch <id>`, from
//! argv to output. Plain output is the path and nothing else; every refusal
//! is exit 1 with the reason on stderr and nothing on stdout; a bad id is
//! refused by the parser before any request.

use serde_json::json;
use tephractl::cli::{self, Action, ArgsError, Command, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, ok_envelope, Behavior, MockServer};

fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn fetch(unit: &str) -> Command {
    Command::Fetch {
        unit: unit.to_string(),
    }
}

fn execute(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client, &mut stdin)
}

const PATH: &str = "/data/tephra/remote/github-atlas/clone/README.md";

fn answer() -> serde_json::Value {
    json!({"unit": "@github-atlas:README.md", "path": PATH, "state": "cached", "size": 54})
}

#[test]
fn parses_one_id_with_json_and_addr_in_any_order() {
    let a = run(&["fetch", "@g:README.md"]).unwrap();
    assert_eq!(a.command, fetch("@g:README.md"));
    assert!(!a.json);
    assert_eq!(a.addr, None);
    let b = run(&[
        "fetch",
        "--json",
        "@g:docs/Guide.md",
        "--addr",
        "127.0.0.1:9",
    ])
    .unwrap();
    assert_eq!(b.command, fetch("@g:docs/Guide.md"));
    assert!(b.json);
    assert_eq!(b.addr.as_deref(), Some("127.0.0.1:9"));
    // A path with a colon in it is still one id.
    assert_eq!(
        run(&["fetch", "@g:a:b.md"]).unwrap().command,
        fetch("@g:a:b.md")
    );
}

#[test]
fn a_bad_id_or_extra_word_is_a_usage_error_before_any_request() {
    for args in [
        vec!["fetch"],
        vec!["fetch", "README.md"],
        vec!["fetch", "g:README.md"],
        vec!["fetch", "@g"],
        vec!["fetch", "@g:"],
        vec!["fetch", "@:README.md"],
        vec!["fetch", "@g:a.md", "@g:b.md"],
        vec!["fetch", "--bogus", "@g:a.md"],
    ] {
        let err = run(&args).unwrap_err();
        assert!(!err.0.is_empty(), "{args:?}");
    }
}

#[test]
fn the_path_is_all_that_is_printed_and_the_call_is_one_unit_fetch() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(answer()),
    }]);
    let outcome = execute(&server, &fetch("@github-atlas:README.md"), false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.stdout, format!("{PATH}\n"));
    assert_eq!(outcome.stderr, "");
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], json!("unit.fetch"));
    assert_eq!(
        received[0].body["params"],
        json!({"unit": "@github-atlas:README.md"})
    );
}

#[test]
fn json_prints_the_whole_answer() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(answer()),
    }]);
    let outcome = execute(&server, &fetch("@github-atlas:README.md"), true);
    assert_eq!(outcome.exit_code, 0);
    let printed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert_eq!(printed, answer());
}

#[test]
fn every_refusal_is_exit_1_with_the_reason_on_stderr_and_nothing_on_stdout() {
    for (message, reason) in [
        ("no such unit: @g:nope.md", "unknown_unit"),
        ("@v:note.md is a file of a local source: there is nothing to fetch", "not_remote"),
        (
            "a Google Doc has no file to download; open it at https://docs.google.com/document/d/x/edit",
            "not_fetchable",
        ),
        (
            "GitHub rejected the token (401): run `tephractl auth github` and try again",
            "auth_required",
        ),
        ("GitHub could not be reached (connection refused): the source is offline", "offline"),
        ("the disk is full: no space was left while writing the file; nothing was kept", "disk_full"),
        ("the connection dropped before the whole file arrived", "connection_dropped"),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: err_envelope(-32011, message, Some(json!({"reason": reason}))),
        }]);
        let outcome = execute(&server, &fetch("@g:x.md"), false);
        assert_eq!(outcome.exit_code, 1, "{reason}");
        assert_eq!(outcome.stderr, format!("tephractl: {message}\n"), "{reason}");
        assert_eq!(outcome.stdout, "", "{reason}");
        // A refusal is never retried.
        assert_eq!(server.received().len(), 1, "{reason}");
    }
}

#[test]
fn a_daemon_that_is_not_running_is_exit_1_and_says_so() {
    let client = RpcClient::new("127.0.0.1:1".to_string(), "t".to_string());
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&fetch("@g:a.md"), false, &client, &mut stdin);
    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.stderr.contains("not running"), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
}
