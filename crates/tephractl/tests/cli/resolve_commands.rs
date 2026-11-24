//! `tests/cli/resolve_commands.rs` (PR-11 G21): `tephractl resolve <id>
//! --mine|--theirs`, from argv to output. Exactly one flag is required, the
//! id may come without its `@`, every refusal is exit 1 with the daemon's
//! reason on stderr and nothing on stdout, and a refusal is never retried.

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

fn resolve(unit: &str, keep: &str) -> Command {
    Command::Resolve {
        unit: unit.to_string(),
        keep: keep.to_string(),
    }
}

fn execute(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client, &mut stdin)
}

fn mine_answer() -> serde_json::Value {
    json!({"unit": "@gdrive:Plan.md", "keep": "mine", "state": "cached"})
}

fn theirs_answer() -> serde_json::Value {
    json!({
        "unit": "@gdrive:Plan.md",
        "keep": "theirs",
        "state": "cached",
        "mine_path": "/data/tephra/remote/gdrive/Plan.md.mine",
        "path": "/data/tephra/remote/gdrive/Plan.md"
    })
}

#[test]
fn parses_the_id_and_exactly_one_flag_in_any_order() {
    let a = run(&["resolve", "@gdrive:Plan.md", "--mine"]).unwrap();
    assert_eq!(a.command, resolve("@gdrive:Plan.md", "mine"));
    assert!(!a.json);
    let b = run(&["resolve", "--theirs", "--json", "@gdrive:docs/Plan.md"]).unwrap();
    assert_eq!(b.command, resolve("@gdrive:docs/Plan.md", "theirs"));
    assert!(b.json);
    let c = run(&[
        "resolve",
        "@gdrive:Plan.md",
        "--mine",
        "--addr",
        "127.0.0.1:9",
    ])
    .unwrap();
    assert_eq!(c.addr.as_deref(), Some("127.0.0.1:9"));
}

#[test]
fn the_at_sign_may_be_left_off_as_the_spec_writes_it() {
    let a = run(&["resolve", "gdrive:Plan.md", "--theirs"]).unwrap();
    assert_eq!(a.command, resolve("@gdrive:Plan.md", "theirs"));
}

#[test]
fn a_missing_or_doubled_flag_or_a_bad_id_is_a_usage_error_before_any_request() {
    for args in [
        vec!["resolve"],
        vec!["resolve", "@gdrive:Plan.md"],
        vec!["resolve", "--mine"],
        vec!["resolve", "@gdrive:Plan.md", "--mine", "--theirs"],
        vec!["resolve", "@gdrive:Plan.md", "--mine", "--mine"],
        vec!["resolve", "Plan.md", "--mine"],
        vec!["resolve", "@gdrive", "--mine"],
        vec!["resolve", "@gdrive:", "--mine"],
        vec!["resolve", "@:Plan.md", "--mine"],
        vec!["resolve", "@gdrive:a.md", "@gdrive:b.md", "--mine"],
        vec!["resolve", "@gdrive:a.md", "--both"],
        vec!["resolve", "@gdrive:a.md", "--keep", "mine"],
    ] {
        let err = run(&args).unwrap_err();
        assert!(!err.0.is_empty(), "{args:?}");
    }
    // The message says what is needed.
    let err = run(&["resolve", "@gdrive:Plan.md"]).unwrap_err();
    assert!(
        err.0.contains("--mine") && err.0.contains("--theirs"),
        "{}",
        err.0
    );
}

#[test]
fn mine_is_one_resolve_call_and_says_the_version_went_to_drive() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(mine_answer()),
    }]);
    let outcome = execute(&server, &resolve("@gdrive:Plan.md", "mine"), false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "sent your version of @gdrive:Plan.md to Drive as a new revision\n"
    );
    assert_eq!(outcome.stderr, "");
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], json!("resolve"));
    assert_eq!(
        received[0].body["params"],
        json!({"unit": "@gdrive:Plan.md", "keep": "mine"})
    );
}

#[test]
fn mine_that_is_still_queued_or_held_again_says_so() {
    for (answer, want) in [
        (
            json!({"unit": "@gdrive:Plan.md", "keep": "mine", "state": "pending_upload"}),
            "your version of @gdrive:Plan.md is queued and goes to Drive as a new revision\n",
        ),
        (
            json!({
                "unit": "@gdrive:Plan.md", "keep": "mine",
                "state": "needs_attention", "reason": "changed on Drive"
            }),
            "@gdrive:Plan.md is held again: changed on Drive\n",
        ),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: ok_envelope(answer),
        }]);
        let outcome = execute(&server, &resolve("@gdrive:Plan.md", "mine"), false);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, want);
    }
}

#[test]
fn theirs_says_where_the_edits_were_kept_and_where_the_refreshed_file_is() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(theirs_answer()),
    }]);
    let outcome = execute(&server, &resolve("@gdrive:Plan.md", "theirs"), false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "kept your edits of @gdrive:Plan.md as /data/tephra/remote/gdrive/Plan.md.mine\n\
         refreshed from Drive: /data/tephra/remote/gdrive/Plan.md\n"
    );
    assert_eq!(
        server.received()[0].body["params"],
        json!({"unit": "@gdrive:Plan.md", "keep": "theirs"})
    );
}

#[test]
fn theirs_with_a_refresh_that_failed_still_succeeds_and_names_fetch() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(json!({
            "unit": "@gdrive:Plan.md", "keep": "theirs", "state": "remote",
            "mine_path": "/d/Plan.md.mine",
            "fetch_failed": "Google Drive could not be reached: the source is offline"
        })),
    }]);
    let outcome = execute(&server, &resolve("@gdrive:Plan.md", "theirs"), false);
    assert_eq!(outcome.exit_code, 0);
    assert!(outcome
        .stdout
        .contains("kept your edits of @gdrive:Plan.md as /d/Plan.md.mine"));
    assert!(
        outcome.stdout.contains("tephractl fetch @gdrive:Plan.md"),
        "{}",
        outcome.stdout
    );
}

#[test]
fn json_prints_the_whole_answer() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(theirs_answer()),
    }]);
    let outcome = execute(&server, &resolve("@gdrive:Plan.md", "theirs"), true);
    assert_eq!(outcome.exit_code, 0);
    let printed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert_eq!(printed, theirs_answer());
}

#[test]
fn every_refusal_is_exit_1_with_the_reason_on_stderr_and_nothing_on_stdout() {
    for (message, reason) in [
        (
            "@gdrive:Other.md is not held for a change on Drive: there is nothing to resolve",
            "nothing_to_resolve",
        ),
        (
            "no such unit: @gdrive:Nope.md (a unit id is @source:path)",
            "unknown_unit",
        ),
        (
            "@g:README.md is not a file of a Drive source: only a Drive conflict is resolved here",
            "not_drive",
        ),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: err_envelope(-32011, message, Some(json!({"reason": reason}))),
        }]);
        let outcome = execute(&server, &resolve("@gdrive:x.md", "mine"), false);
        assert_eq!(outcome.exit_code, 1, "{reason}");
        assert_eq!(
            outcome.stderr,
            format!("tephractl: {message}\n"),
            "{reason}"
        );
        assert_eq!(outcome.stdout, "", "{reason}");
        assert_eq!(server.received().len(), 1, "{reason}: never retried");
    }
}

#[test]
fn a_daemon_that_is_not_running_is_exit_1_and_says_so() {
    let client = RpcClient::new("127.0.0.1:1".to_string(), "t".to_string());
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&resolve("@gdrive:a.md", "mine"), false, &client, &mut stdin);
    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.stderr.contains("not running"), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
}

fn drive_row(extra: serde_json::Value) -> serde_json::Value {
    let mut row = json!({
        "name": "gdrive", "path": "drive:Notes", "status": "active",
        "last_scanned_at": 1_700_000_000, "kind": "drive", "repo": "Notes",
        "branch": null, "cache": true, "cache_bytes": 12, "git_bytes": 0,
        "index_done": 3, "index_total": 3, "not_pushed": 0, "behind": 0
    });
    for (key, value) in extra.as_object().unwrap() {
        row[key] = value.clone();
    }
    row
}

fn list(rows: serde_json::Value) -> commands::CommandOutcome {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: ok_envelope(rows),
    }]);
    execute(&server, &Command::SourceList, false)
}

#[test]
fn source_list_prints_a_drive_sources_pending_uploads_and_held_files_with_their_reason() {
    let outcome = list(json!([drive_row(json!({
        "pending_upload": 2,
        "needs_attention": 3,
        "attention": [
            {"path": "Plan.md", "reason": "changed on Drive"},
            {"path": "docs/A.md", "reason": "Drive refused the upload: the file is read-only"}
        ]
    }))]));
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    let text = outcome.stdout;
    assert!(text.contains("gdrive: 2 pending upload\n"), "{text}");
    assert!(
        text.contains("gdrive: Plan.md: needs attention: changed on Drive\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "gdrive: docs/A.md: needs attention: Drive refused the upload: the file is read-only\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("gdrive: and 1 more need attention\n"),
        "{text}"
    );
    for line in text.lines() {
        assert_eq!(line, line.trim_end(), "{line:?}");
    }
}

#[test]
fn source_list_prints_nothing_of_it_for_a_drive_source_with_no_unsent_work() {
    let text = list(json!([drive_row(json!({}))])).stdout;
    assert!(!text.contains("pending upload"), "{text}");
    assert!(!text.contains("needs attention"), "{text}");
}
