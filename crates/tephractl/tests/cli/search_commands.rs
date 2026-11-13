//! `search` (G8) against the mock RPC server: the exact `unit.search`
//! request that goes out, rendered and `--json` output byte for byte
//! against `tests/fixtures/expected/search_*.txt` (served from
//! `tests/fixtures/search_*.json`), the plain "no matches" line, the
//! daemon's refusal of a blank query, and (through the real binary) the
//! client-side "pick a file first" rejection with no RPC round-trip.
//! Argv parsing is tested here too, from a `Vec<&str>` to a `Command`.

use serde_json::json;
use tephractl::cli::{self, Action, Command, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn search(query: &str, limit: Option<u64>) -> Command {
    Command::Search {
        query: query.to_string(),
        limit,
    }
}

fn run(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client_for(server), &mut stdin)
}

fn serve(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

fn parse(args: &[&str]) -> Result<ParsedArgs, cli::ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

// ---- the request ------------------------------------------------------

#[test]
fn search_sends_unit_search_with_the_query_verbatim_and_scope_names() {
    let server = serve("search_gp4");
    let outcome = run(&server, &search("atlas outside", None), false);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "unit.search");
    assert_eq!(
        received[0].body["params"],
        json!({"query": "atlas outside", "scope": "names"})
    );
    assert_eq!(
        received[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-token")
    );
}

#[test]
fn search_sends_a_limit_only_when_one_was_given() {
    let server = serve("search_gp4");
    run(&server, &search("a", Some(7)), false);
    assert_eq!(
        server.received()[0].body["params"],
        json!({"query": "a", "scope": "names", "limit": 7})
    );
}

#[test]
fn search_sends_odd_queries_untouched() {
    for query in ["café", "100%_done", "a, b @c/[[d]] .*(", "it's \"q\"", ""] {
        let server = serve("search_empty");
        run(&server, &search(query, None), false);
        assert_eq!(server.received()[0].body["params"]["query"], query);
    }
}

// ---- output -----------------------------------------------------------

#[test]
fn search_prints_one_row_per_result_in_rank_order() {
    let server = serve("search_gp4");
    let outcome = run(&server, &search("atlas outside", None), false);

    assert_eq!(outcome.stdout, load_expected("search_gp4_rendered"));
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.exit_code, 0);
    // GP4: the target is on line 1.
    assert!(outcome.stdout.lines().next().unwrap().starts_with(
        "@notes:Obsidian Atlas/plans/Atlas outside Obsidian Chrome plugin, VSCode plugin.md  prose"
    ));
}

#[test]
fn search_flags_offline_results() {
    let server = serve("search_offline");
    let outcome = run(&server, &search("atlas", None), false);
    assert_eq!(outcome.stdout, load_expected("search_offline_rendered"));
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn search_json_prints_the_raw_result_array() {
    for (fixture, expected) in [
        ("search_gp4", "search_gp4_json"),
        ("search_offline", "search_offline_json"),
        ("search_empty", "search_empty_json"),
    ] {
        let server = serve(fixture);
        let outcome = run(&server, &search("atlas", None), true);
        assert_eq!(outcome.stdout, load_expected(expected), "{fixture}");
        assert_eq!(outcome.exit_code, 0);
        let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
        assert_eq!(parsed, load_fixture(fixture)["result"], "{fixture}");
        assert!(parsed.is_array());
    }
}

#[test]
fn a_query_matching_nothing_prints_no_matches_and_exits_zero() {
    let server = serve("search_empty");
    let outcome = run(&server, &search("zzzz", None), false);
    assert_eq!(outcome.stdout, load_expected("search_empty_rendered"));
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn control_characters_in_a_result_never_reach_the_terminal() {
    let server = serve("search_hostile");
    let outcome = run(&server, &search("evil", None), false);
    assert_eq!(outcome.stdout, load_expected("search_hostile_rendered"));
    assert!(!outcome.stdout.contains('\u{1b}'));
    assert_eq!(outcome.stdout.lines().count(), 1, "one result is one row");
}

// ---- errors -----------------------------------------------------------

#[test]
fn the_daemons_refusal_of_a_blank_query_is_printed_with_a_non_zero_exit() {
    for query in ["", "   "] {
        let server = serve("search_invalid_query");
        let outcome = run(&server, &search(query, None), false);
        assert_eq!(outcome.stdout, "");
        assert_ne!(outcome.exit_code, 0);
        assert!(
            outcome.stderr.contains("non-blank character"),
            "stderr: {}",
            outcome.stderr
        );
        assert!(outcome.stderr.starts_with("tephractl: "));
    }
}

#[test]
fn an_unsupported_scope_error_from_the_daemon_is_printed_as_is() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32602, "unknown scope \"x\"", None),
    }]);
    let outcome = run(&server, &search("a", None), false);
    assert_ne!(outcome.exit_code, 0);
    assert!(
        outcome.stderr.contains("unknown scope"),
        "{}",
        outcome.stderr
    );
}

#[test]
fn a_wrong_token_is_the_standard_unauthorized_error() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 401,
        body: load_fixture("unauthorized_error"),
    }]);
    let outcome = run(&server, &search("a", None), false);
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("authentication failed"),
        "{}",
        outcome.stderr
    );
}

// ---- argv -------------------------------------------------------------

#[test]
fn argv_json_and_addr_parse_leading_and_trailing() {
    let leading = parse(&["--json", "--addr", "127.0.0.1:1", "search", "atlas outside"]).unwrap();
    let trailing = parse(&["search", "atlas outside", "--json", "--addr", "127.0.0.1:1"]).unwrap();
    for parsed in [&leading, &trailing] {
        assert_eq!(parsed.command, search("atlas outside", None));
        assert!(parsed.json);
        assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:1"));
    }
    let plain = parse(&["search", "atlas outside"]).unwrap();
    assert!(!plain.json);
    assert_eq!(plain.addr, None);
}

#[test]
fn argv_hash_and_caret_queries_are_refused_before_any_rpc() {
    for query in ["#x", "^x", "#"] {
        let err = parse(&["search", query]).unwrap_err();
        assert!(err.0.contains("pick a file first"), "{query}: {err}");
    }
    for query in ["^^", "^^x"] {
        let err = parse(&["search", query]).unwrap_err();
        assert!(err.0.contains("^^"), "{query}: {err}");
        assert!(!err.0.contains("pick a file first"), "{query}: {err}");
    }
    // A hash or caret that does not lead is text.
    assert!(parse(&["search", "c#"]).is_ok());
}

/// Through the real binary: the rejection is printed, the exit code is
/// non-zero, and the mock daemon never sees a request (nor is a token
/// file even looked for: this runs with an empty `HOME`).
#[test]
fn the_binary_rejects_a_hash_query_with_no_round_trip() {
    for query in ["#x", "^x", "^^x"] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: load_fixture("search_gp4"),
        }]);
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(["search", query, "--addr", &server.addr])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .output()
            .expect("run tephractl");
        assert!(!output.status.success(), "{query}");
        assert_eq!(output.stdout, b"", "{query}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with("tephractl: "), "{query}: {stderr}");
        if query == "^^x" {
            assert!(stderr.contains("^^"), "{stderr}");
        } else {
            assert!(stderr.contains("pick a file first"), "{query}: {stderr}");
        }
        assert_eq!(server.call_count(), 0, "{query}: no RPC may be made");
    }
}
