//! `search --in <id> "#q"` and `"^q"` (G9) against the mock RPC server:
//! argv parsing (the sigil picks the scope and is stripped, a bare sigil
//! is an empty query, a sigil query without `--in` is "pick a file
//! first"), the exact `unit.search` request that goes out, and rendered
//! and `--json` output byte for byte against
//! `tests/fixtures/expected/search_in_*.txt` (served from
//! `tests/fixtures/search_in_*.json`).

use serde_json::json;
use tephractl::cli::{self, Action, Command, InFileSearchScope, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

const UNIT: &str = "@tephra:crates/tephrad/src/registry.rs";

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn search_in(scope: InFileSearchScope, query: &str, limit: Option<u64>) -> Command {
    Command::SearchIn {
        unit: UNIT.to_string(),
        scope,
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

// ---- argv -------------------------------------------------------------

#[test]
fn a_hash_query_with_in_is_a_headings_search_with_the_sigil_stripped() {
    let parsed = parse(&["search", "--in", UNIT, "#intro"]).unwrap();
    assert_eq!(
        parsed.command,
        search_in(InFileSearchScope::Headings, "intro", None)
    );
    assert!(!parsed.json);
    assert_eq!(parsed.addr, None);
}

#[test]
fn a_caret_query_with_in_is_a_blocks_search_with_the_sigil_stripped() {
    let parsed = parse(&["search", "--in", UNIT, "^conflict"]).unwrap();
    assert_eq!(
        parsed.command,
        search_in(InFileSearchScope::Blocks, "conflict", None)
    );
}

#[test]
fn a_bare_sigil_is_an_empty_query_that_lists_everything() {
    for (sigil, scope) in [
        ("#", InFileSearchScope::Headings),
        ("^", InFileSearchScope::Blocks),
    ] {
        let parsed = parse(&["search", "--in", UNIT, sigil]).unwrap();
        assert_eq!(parsed.command, search_in(scope, "", None), "{sigil}");
    }
}

#[test]
fn only_the_leading_sigil_is_stripped_and_the_rest_is_kept_verbatim() {
    for (arg, scope, query) in [
        ("##x", InFileSearchScope::Headings, "#x"),
        ("#a ^b", InFileSearchScope::Headings, "a ^b"),
        ("^c#", InFileSearchScope::Blocks, "c#"),
        ("#  spaced  ", InFileSearchScope::Headings, "  spaced  "),
        ("#café 日本", InFileSearchScope::Headings, "café 日本"),
    ] {
        let parsed = parse(&["search", "--in", UNIT, arg]).unwrap();
        assert_eq!(parsed.command, search_in(scope, query, None), "{arg}");
    }
}

#[test]
fn in_limit_json_and_addr_work_in_any_order() {
    let want = search_in(InFileSearchScope::Blocks, "x", Some(5));
    for args in [
        vec!["search", "--in", UNIT, "^x", "--limit", "5", "--json"],
        vec!["search", "^x", "--limit", "5", "--in", UNIT, "--json"],
        vec!["--json", "search", "--limit", "5", "--in", UNIT, "^x"],
    ] {
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.command, want, "{args:?}");
        assert!(parsed.json, "{args:?}");
    }
    let parsed = parse(&["search", "--in", UNIT, "^x", "--addr", "127.0.0.1:9"]).unwrap();
    assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:9"));
}

#[test]
fn a_sigil_query_without_in_says_pick_a_file_first() {
    for query in ["#x", "^x", "#", "^"] {
        let err = parse(&["search", query]).unwrap_err();
        assert!(err.0.contains("pick a file first"), "{query}: {err}");
        assert!(err.0.contains("--in"), "{query}: {err}");
    }
}

#[test]
fn in_with_a_query_that_has_no_sigil_is_refused_not_guessed() {
    for query in ["intro", "", "c#"] {
        let err = parse(&["search", "--in", UNIT, query]).unwrap_err();
        assert!(
            err.0.contains("'#'") && err.0.contains("'^'"),
            "{query}: {err}"
        );
    }
}

#[test]
fn a_double_caret_takes_no_in() {
    for args in [
        vec!["search", "--in", UNIT, "^^x"],
        vec!["search", "--in", UNIT, "^^"],
    ] {
        let err = parse(&args).unwrap_err();
        assert!(
            err.0.contains("^^") && err.0.contains("no --in"),
            "{args:?}: {err}"
        );
        assert!(!err.0.contains("pick a file first"), "{args:?}: {err}");
    }
}

#[test]
fn a_malformed_in_is_refused_before_any_rpc() {
    for bad in [
        "",
        "notes/a.md",
        "tephra:a.md",
        "@tephra",
        "@tephra:",
        "@:a.md",
    ] {
        let err = parse(&["search", "--in", bad, "#x"]).unwrap_err();
        assert!(err.0.contains("--in"), "{bad:?}: {err}");
    }
    let err = parse(&["search", "#x", "--in"]).unwrap_err();
    assert!(err.0.contains("--in requires a value"), "{err}");
}

// ---- the request ------------------------------------------------------

#[test]
fn the_request_carries_the_scope_the_id_and_the_query_without_its_sigil() {
    for (command, scope, query) in [
        (
            search_in(InFileSearchScope::Headings, "intro", None),
            "headings",
            "intro",
        ),
        (
            search_in(InFileSearchScope::Blocks, "conflict", None),
            "blocks",
            "conflict",
        ),
        (search_in(InFileSearchScope::Blocks, "", None), "blocks", ""),
    ] {
        let server = serve("search_in_symbols");
        let outcome = run(&server, &command, false);
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        let received = server.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].body["method"], "unit.search");
        assert_eq!(
            received[0].body["params"],
            json!({"query": query, "scope": scope, "in": UNIT})
        );
        assert_eq!(
            received[0].headers.get("authorization").map(String::as_str),
            Some("Bearer test-token")
        );
    }
}

#[test]
fn a_limit_is_sent_only_when_one_was_given() {
    let server = serve("search_in_symbols");
    run(
        &server,
        &search_in(InFileSearchScope::Blocks, "x", Some(7)),
        false,
    );
    assert_eq!(
        server.received()[0].body["params"],
        json!({"query": "x", "scope": "blocks", "in": UNIT, "limit": 7})
    );
}

// ---- output -----------------------------------------------------------

/// The daemon sends a paragraph's `^id` as `block_id` only, never in
/// `text`, so a row shows it once.
#[test]
fn a_block_id_is_shown_once_per_row() {
    let server = serve("search_in_blocks");
    let outcome = run(
        &server,
        &search_in(InFileSearchScope::Blocks, "x", None),
        false,
    );
    let rows: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(rows.len(), 3, "{}", outcome.stdout);
    for row in &rows {
        assert!(row.matches('^').count() <= 1, "{row}");
    }
    assert_eq!(rows.iter().filter(|r| r.ends_with("^dup")).count(), 2);
}

#[test]
fn each_kind_of_result_renders_as_one_line_against_the_golden_files() {
    for (fixture, scope) in [
        ("symbols", InFileSearchScope::Blocks),
        ("headings", InFileSearchScope::Headings),
        ("blocks", InFileSearchScope::Blocks),
        ("documents", InFileSearchScope::Blocks),
        ("offline", InFileSearchScope::Blocks),
        ("empty", InFileSearchScope::Headings),
    ] {
        let server = serve(&format!("search_in_{fixture}"));
        let outcome = run(&server, &search_in(scope, "x", None), false);
        assert_eq!(
            outcome.stdout,
            load_expected(&format!("search_in_{fixture}_rendered")),
            "{fixture}"
        );
        assert_eq!(outcome.stderr, "", "{fixture}");
        assert_eq!(outcome.exit_code, 0, "{fixture}");
    }
}

#[test]
fn gp5_the_enum_is_the_first_row_with_its_kind_and_line() {
    let server = serve("search_in_symbols");
    let outcome = run(
        &server,
        &search_in(InFileSearchScope::Blocks, "conflict", None),
        false,
    );
    assert_eq!(
        outcome.stdout.lines().next(),
        Some("symbol enum  line 73  ConflictReason")
    );
}

#[test]
fn no_terminal_control_character_and_no_second_line_comes_from_a_result() {
    for fixture in ["headings", "blocks"] {
        let server = serve(&format!("search_in_{fixture}"));
        let outcome = run(
            &server,
            &search_in(InFileSearchScope::Headings, "x", None),
            false,
        );
        assert!(!outcome.stdout.contains('\u{1b}'), "{fixture}");
        // One result is one row, however many lines its paragraph has.
        assert_eq!(outcome.stdout.lines().count(), 3, "{fixture}");
    }
}

#[test]
fn json_prints_the_raw_result_object_and_nothing_else() {
    for fixture in [
        "symbols",
        "headings",
        "blocks",
        "documents",
        "offline",
        "empty",
    ] {
        let server = serve(&format!("search_in_{fixture}"));
        let outcome = run(
            &server,
            &search_in(InFileSearchScope::Blocks, "x", None),
            true,
        );
        assert_eq!(
            outcome.stdout,
            load_expected(&format!("search_in_{fixture}_json")),
            "{fixture}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
        assert_eq!(
            parsed,
            load_fixture(&format!("search_in_{fixture}"))["result"],
            "{fixture}"
        );
        assert_eq!(outcome.exit_code, 0, "{fixture}");
    }
}

// ---- errors -----------------------------------------------------------

#[test]
fn the_daemons_error_for_an_unknown_unit_is_printed_with_a_non_zero_exit() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32000, &format!("no such unit: {UNIT}"), None),
    }]);
    let outcome = run(
        &server,
        &search_in(InFileSearchScope::Headings, "x", None),
        false,
    );
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains(UNIT), "{}", outcome.stderr);
    assert!(outcome.stderr.starts_with("tephractl: "));
}

/// Through the real binary: a sigil query with no `--in`, and a bad
/// `--in`, are printed and refused with a non-zero exit and no request
/// reaching the daemon.
#[test]
fn the_binary_refuses_a_missing_or_malformed_in_with_no_round_trip() {
    for (args, needle) in [
        (vec!["search", "^conflict"], "pick a file first"),
        (vec!["search", "#"], "pick a file first"),
        (vec!["search", "--in", "registry.rs", "^x"], "--in"),
        (vec!["search", "--in", UNIT, "x"], "'#'"),
    ] {
        let server = serve("search_in_symbols");
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(&args)
            .args(["--addr", &server.addr])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .output()
            .expect("run tephractl");
        assert!(!output.status.success(), "{args:?}");
        assert_eq!(output.stdout, b"", "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with("tephractl: "), "{args:?}: {stderr}");
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
        assert_eq!(server.call_count(), 0, "{args:?}: no RPC may be made");
    }
}
