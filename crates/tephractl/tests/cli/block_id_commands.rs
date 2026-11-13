//! `block-id <unit-id> --line N | --symbol NAME [--json]` (G11) against
//! the mock RPC server: argv parsing (one of `--line`, a positive whole
//! number, and `--symbol`, a name; not both, not neither), the exact `block.ensure_id` request that goes out, the printed link,
//! the `--json` shape `{id, link, created}`, and a refusal giving a
//! non-zero exit with the daemon's message on stderr.

use serde_json::{json, Value};
use tephractl::cli::{self, Action, BlockTarget, Command, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, ok_envelope, Behavior, MockServer};

const UNIT: &str = "@notes:Tephra test.md";

fn parse(args: &[&str]) -> Result<ParsedArgs, cli::ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn block_id(line: u64) -> Command {
    Command::BlockId {
        unit: UNIT.to_string(),
        target: BlockTarget::Line(line),
    }
}

const CODE_UNIT: &str = "@tephra:crates/tephrad/src/registry.rs";

fn block_id_symbol(unit: &str, symbol: &str) -> Command {
    Command::BlockId {
        unit: unit.to_string(),
        target: BlockTarget::Symbol(symbol.to_string()),
    }
}

fn serve(body: Value) -> MockServer {
    MockServer::start(vec![Behavior::Respond { status: 200, body }])
}

fn run(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client, &mut stdin)
}

// ---- argv -------------------------------------------------------------

#[test]
fn block_id_takes_a_unit_id_and_a_line() {
    let parsed = parse(&["block-id", UNIT, "--line", "3"]).unwrap();
    assert_eq!(parsed.command, block_id(3));
    assert!(!parsed.json);
    assert_eq!(parsed.addr, None);
}

#[test]
fn the_flags_may_come_in_any_order_and_json_and_addr_are_accepted() {
    for args in [
        vec!["block-id", "--line", "3", UNIT, "--json"],
        vec!["--json", "block-id", UNIT, "--line", "3"],
        vec!["block-id", UNIT, "--json", "--line", "3"],
    ] {
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.command, block_id(3), "{args:?}");
        assert!(parsed.json, "{args:?}");
    }
    let parsed = parse(&["block-id", UNIT, "--line", "3", "--addr", "127.0.0.1:9"]).unwrap();
    assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:9"));
}

#[test]
fn line_or_symbol_is_required() {
    let err = parse(&["block-id", UNIT]).unwrap_err();
    assert!(err.0.contains("--line"), "{err}");
    assert!(err.0.contains("--symbol"), "{err}");
}

#[test]
fn line_must_be_a_positive_whole_number() {
    for bad in ["0", "-1", "1.5", "abc", "", "1e3", "+x"] {
        let err = parse(&["block-id", UNIT, "--line", bad]).unwrap_err();
        assert!(err.0.contains("--line"), "{bad:?}: {err}");
        assert!(err.0.contains("at least 1"), "{bad:?}: {err}");
    }
    let err = parse(&["block-id", UNIT, "--line"]).unwrap_err();
    assert!(err.0.contains("--line"), "{err}");
}

#[test]
fn the_unit_id_is_required_and_must_look_like_one() {
    let err = parse(&["block-id", "--line", "3"]).unwrap_err();
    assert!(err.0.contains("<unit-id>"), "{err}");
    for bad in [
        "Tephra test.md",
        "@notes",
        "@notes:",
        "@:x.md",
        "notes:x.md",
    ] {
        let err = parse(&["block-id", bad, "--line", "3"]).unwrap_err();
        assert!(err.0.contains("@source:path"), "{bad:?}: {err}");
    }
}

#[test]
fn extra_arguments_and_unknown_flags_are_refused() {
    let err = parse(&["block-id", UNIT, "@notes:b.md", "--line", "3"]).unwrap_err();
    assert!(err.0.contains("extra argument"), "{err}");
    let err = parse(&["block-id", UNIT, "--line", "3", "--bogus", "x"]).unwrap_err();
    assert!(err.0.contains("--bogus"), "{err}");
}

#[test]
fn symbol_takes_a_name_and_may_come_anywhere() {
    for args in [
        vec!["block-id", CODE_UNIT, "--symbol", "ConflictReason"],
        vec![
            "block-id",
            "--symbol",
            "ConflictReason",
            CODE_UNIT,
            "--json",
        ],
        vec![
            "--json",
            "block-id",
            CODE_UNIT,
            "--symbol",
            "ConflictReason",
        ],
    ] {
        let parsed = parse(&args).unwrap();
        assert_eq!(
            parsed.command,
            block_id_symbol(CODE_UNIT, "ConflictReason"),
            "{args:?}"
        );
    }
    let parsed = parse(&["block-id", CODE_UNIT, "--symbol", "Registry.register"]).unwrap();
    assert_eq!(
        parsed.command,
        block_id_symbol(CODE_UNIT, "Registry.register")
    );
}

#[test]
fn line_and_symbol_together_or_a_blank_symbol_are_refused() {
    let err = parse(&["block-id", UNIT, "--line", "3", "--symbol", "x"]).unwrap_err();
    assert!(err.0.contains("not both"), "{err}");
    let err = parse(&["block-id", UNIT, "--symbol", "x", "--line", "3"]).unwrap_err();
    assert!(err.0.contains("not both"), "{err}");
    for bad in ["", "  "] {
        let err = parse(&["block-id", UNIT, "--symbol", bad]).unwrap_err();
        assert!(err.0.contains("--symbol"), "{bad:?}: {err}");
    }
    let err = parse(&["block-id", UNIT, "--symbol"]).unwrap_err();
    assert!(err.0.contains("--symbol"), "{err}");
}

// ---- request and output ----------------------------------------------

#[test]
fn the_request_is_block_ensure_id_with_the_unit_and_line() {
    let server = serve(ok_envelope(json!({
        "id": "k3f9a2", "link": "[[@notes:Tephra test#^k3f9a2]]", "created": true
    })));
    let outcome = run(&server, &block_id(3), false);
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "block.ensure_id");
    assert_eq!(received[0].body["params"], json!({"unit": UNIT, "line": 3}));
    assert_eq!(
        received[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-token")
    );
}

#[test]
fn a_symbol_request_sends_the_symbol_and_no_line_and_prints_the_link() {
    let link = "[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]";
    let server = serve(ok_envelope(json!({
        "id": "k3f9a2", "link": link, "created": true
    })));
    let outcome = run(
        &server,
        &block_id_symbol(CODE_UNIT, "ConflictReason"),
        false,
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, format!("{link}\n"));
    let received = server.received();
    assert_eq!(received[0].body["method"], "block.ensure_id");
    assert_eq!(
        received[0].body["params"],
        json!({"unit": CODE_UNIT, "symbol": "ConflictReason"})
    );

    // --json passes the daemon's result through.
    let result = json!({"id": "k3f9a2", "link": link, "created": false});
    let server = serve(ok_envelope(result.clone()));
    let outcome = run(&server, &block_id_symbol(CODE_UNIT, "ConflictReason"), true);
    assert_eq!(
        serde_json::from_str::<Value>(&outcome.stdout).unwrap(),
        result
    );
}

#[test]
fn a_code_refusal_is_a_non_zero_exit_with_its_reason_on_stderr() {
    for message in [
        "cannot place ID: line ends inside a string (line 5): a comment there would become part of the string",
        "2 symbols are named hello in @tephra:a.py: Greeter.hello (line 10, method), Other.hello (line 15, method); name one as Parent.name, or give its line",
        "no symbol on line 2",
    ] {
        for json in [false, true] {
            let server = serve(err_envelope(-32013, message, None));
            let outcome = run(&server, &block_id_symbol("@tephra:a.py", "hello"), json);
            assert_ne!(outcome.exit_code, 0);
            assert_eq!(outcome.stdout, "");
            assert!(outcome.stderr.contains(message), "{}", outcome.stderr);
        }
    }
}

#[test]
fn the_link_is_printed_alone_whether_created_or_existing() {
    for created in [true, false] {
        let server = serve(ok_envelope(json!({
            "id": "k3f9a2", "link": "[[@notes:Tephra test#^k3f9a2]]", "created": created
        })));
        let outcome = run(&server, &block_id(3), false);
        assert_eq!(outcome.stdout, "[[@notes:Tephra test#^k3f9a2]]\n");
        assert_eq!(outcome.stderr, "");
        assert_eq!(outcome.exit_code, 0);
    }
}

#[test]
fn a_heading_link_is_printed_the_same_way() {
    let server = serve(ok_envelope(json!({
        "id": null, "link": "[[@notes:Tephra test#My Heading]]", "created": false
    })));
    let outcome = run(&server, &block_id(1), false);
    assert_eq!(outcome.stdout, "[[@notes:Tephra test#My Heading]]\n");
}

#[test]
fn json_prints_the_daemons_result_as_id_link_created() {
    for (result, created) in [
        (
            json!({"id": "k3f9a2", "link": "[[@notes:log.txt#^k3f9a2]]", "created": true}),
            true,
        ),
        (
            json!({"id": "summary", "link": "[[@notes:log.txt#^summary]]", "created": false}),
            false,
        ),
    ] {
        let server = serve(ok_envelope(result.clone()));
        let outcome = run(&server, &block_id(2), true);
        assert_eq!(outcome.exit_code, 0);
        let printed: Value = serde_json::from_str(&outcome.stdout).unwrap();
        assert_eq!(printed, result);
        assert_eq!(printed["created"], json!(created));
        assert_eq!(
            printed.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["created", "id", "link"]
        );
        assert!(outcome.stdout.ends_with('\n'));
    }
}

#[test]
fn a_refusal_is_a_non_zero_exit_with_the_daemons_message_on_stderr() {
    let message =
        "line 5 is inside a code block (code fence): a block ID there would be part of the code";
    let server = serve(err_envelope(-32013, message, None));
    let outcome = run(&server, &block_id(5), false);
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.starts_with("tephractl: "),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains(message), "{}", outcome.stderr);

    // The same with --json: still stderr and a non-zero exit, nothing
    // on stdout.
    let server = serve(err_envelope(-32013, message, None));
    let outcome = run(&server, &block_id(5), true);
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains(message), "{}", outcome.stderr);
}

#[test]
fn a_changed_file_message_is_passed_through_unchanged() {
    let server = serve(err_envelope(-32013, "file changed, try again", None));
    let outcome = run(&server, &block_id(1), false);
    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("file changed, try again"));
}

/// Through the real binary: bad arguments are refused with a non-zero
/// exit and no request reaching the daemon.
#[test]
fn the_binary_refuses_bad_arguments_with_no_round_trip() {
    let run_binary = |args: &[&str], server: &MockServer| {
        let home = tempfile::tempdir().unwrap();
        std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(args)
            .args(["--addr", &server.addr])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .output()
            .expect("run tephractl")
    };

    for (args, needle) in [
        (vec!["block-id", UNIT], "--line"),
        (vec!["block-id", UNIT, "--line", "0"], "--line"),
        (vec!["block-id", UNIT, "--line", "x"], "--line"),
        (vec!["block-id", UNIT, "--symbol", ""], "--symbol"),
        (
            vec!["block-id", UNIT, "--line", "3", "--symbol", "x"],
            "not both",
        ),
    ] {
        let server = serve(ok_envelope(json!({})));
        let output = run_binary(&args, &server);
        assert!(!output.status.success(), "{args:?}");
        assert_eq!(output.stdout, b"", "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with("tephractl: "), "{args:?}: {stderr}");
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
        assert_eq!(server.call_count(), 0, "{args:?}: no RPC may be made");
    }
}
