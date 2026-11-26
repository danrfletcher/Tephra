//! `tests/cli/pin_commands.rs` (PR-12 G11): `tephractl pin <id> [--yes]` and
//! `tephractl unpin <id>`, from argv to output. Rendered and `--json` output
//! and the prompt are compared byte for byte with golden files; the params
//! sent are asserted exactly. The daemon decides what needs a question:
//! the CLI asks only when it answers `needs_confirm`, only on a terminal,
//! and only `--yes` or a typed yes goes on to pin.

use serde_json::json;
use tephractl::cli::{self, Action, ArgsError, Command, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn respond(fixture: &str) -> Behavior {
    Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }
}

struct Ran {
    outcome: commands::CommandOutcome,
    prompt: String,
}

/// Runs `argv` with `typed` as stdin, `terminal` saying whether it is one.
fn exec(server: &MockServer, argv: &[&str], typed: &str, terminal: bool) -> Ran {
    let parsed = run(argv).unwrap();
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    let mut stdin = typed.as_bytes();
    let mut prompt = Vec::new();
    let outcome = commands::execute_with_terminal(
        &parsed.command,
        parsed.json,
        &client,
        &mut stdin,
        &mut prompt,
        terminal,
    );
    Ran {
        outcome,
        prompt: String::from_utf8(prompt).unwrap(),
    }
}

fn sent(server: &MockServer) -> Vec<(String, serde_json::Value)> {
    server
        .received()
        .iter()
        .map(|r| {
            (
                r.body["method"].as_str().unwrap().to_string(),
                r.body["params"].clone(),
            )
        })
        .collect()
}

// ---- argument parsing ------------------------------------------------------

#[test]
fn parses_pin_and_unpin_with_flags_in_any_order() {
    let pin = |unit: &str, yes: bool| Command::Pin {
        unit: unit.to_string(),
        yes,
    };
    assert_eq!(
        run(&["pin", "@gdrive:Trip Plans/"]).unwrap().command,
        pin("@gdrive:Trip Plans/", false)
    );
    let b = run(&[
        "pin",
        "--yes",
        "@github-atlas:docs/setup.md",
        "--json",
        "--addr",
        "127.0.0.1:9",
    ])
    .unwrap();
    assert_eq!(b.command, pin("@github-atlas:docs/setup.md", true));
    assert!(b.json);
    assert_eq!(b.addr.as_deref(), Some("127.0.0.1:9"));
    assert_eq!(
        run(&["unpin", "@gdrive:Trip Plans/", "--json"])
            .unwrap()
            .command,
        Command::Unpin {
            unit: "@gdrive:Trip Plans/".to_string()
        }
    );
    // A path with a colon in it is still one id.
    assert_eq!(
        run(&["pin", "@g:a:b.md"]).unwrap().command,
        pin("@g:a:b.md", false)
    );
}

#[test]
fn a_bad_id_a_stray_flag_or_an_extra_word_is_a_usage_error_before_any_request() {
    for args in [
        vec!["pin"],
        vec!["unpin"],
        vec!["pin", "README.md"],
        vec!["pin", "g:README.md"],
        vec!["pin", "@g"],
        vec!["pin", "@g:"],
        vec!["pin", "@:README.md"],
        vec!["pin", "@g:a.md", "@g:b.md"],
        vec!["unpin", "@g:a.md", "@g:b.md"],
        vec!["pin", "--bogus", "@g:a.md"],
        // `--yes` belongs to `pin`: an unpin asks nothing.
        vec!["unpin", "--yes", "@g:a.md"],
    ] {
        let err = run(&args).unwrap_err();
        assert!(!err.0.is_empty(), "{args:?}");
    }
}

// ---- pin -------------------------------------------------------------------

#[test]
fn a_pin_prints_the_count_and_the_total_and_makes_one_call_without_confirm() {
    let server = MockServer::start(vec![respond("pin_folder")]);
    let ran = exec(&server, &["pin", "@gdrive:Trip Plans/"], "", true);
    assert_eq!(ran.outcome.exit_code, 0, "{}", ran.outcome.stderr);
    assert_eq!(ran.outcome.stdout, load_expected("pin_folder_rendered"));
    assert_eq!(ran.outcome.stderr, "");
    assert_eq!(ran.prompt, "");
    assert_eq!(
        sent(&server),
        vec![(
            "unit.pin".to_string(),
            json!({"unit": "@gdrive:Trip Plans/"})
        )]
    );
}

#[test]
fn a_file_pin_and_a_pin_over_the_cap_print_their_golden_text() {
    let server = MockServer::start(vec![respond("pin_file")]);
    let ran = exec(&server, &["pin", "@github-atlas:docs/setup.md"], "", false);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(ran.outcome.stdout, load_expected("pin_file_rendered"));

    // Over the cap it only warns: exit 0, the warning after the totals.
    let server = MockServer::start(vec![respond("pin_over_cap")]);
    let ran = exec(&server, &["pin", "@g:big/"], "", false);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(ran.outcome.stdout, load_expected("pin_over_cap_rendered"));
}

#[test]
fn pinning_what_is_pinned_exits_0_changes_nothing_and_says_so() {
    let server = MockServer::start(vec![respond("pin_already")]);
    let ran = exec(&server, &["pin", "@gdrive:Trip Plans/"], "", false);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(ran.outcome.stdout, load_expected("pin_already_rendered"));
    assert_eq!(server.received().len(), 1);
}

#[test]
fn json_is_the_whole_answer_and_nothing_else_is_on_stdout() {
    let server = MockServer::start(vec![respond("pin_folder")]);
    let ran = exec(&server, &["pin", "--json", "@gdrive:Trip Plans/"], "", true);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(ran.outcome.stdout, load_expected("pin_folder_json"));
    let printed: serde_json::Value = serde_json::from_str(&ran.outcome.stdout).unwrap();
    assert_eq!(printed, load_fixture("pin_folder")["result"]);
}

// ---- the question ----------------------------------------------------------

#[test]
fn a_large_pin_shows_the_size_asks_on_the_prompt_writer_and_a_yes_pins() {
    for yes in ["y\n", "YES\n", " yes \n"] {
        let server =
            MockServer::start(vec![respond("pin_needs_confirm"), respond("pin_confirmed")]);
        let ran = exec(&server, &["pin", "@gdrive:Trip Plans/"], yes, true);
        assert_eq!(ran.outcome.exit_code, 0, "{yes:?}: {}", ran.outcome.stderr);
        // The question is on the prompt writer (stderr), the answer on stdout.
        assert_eq!(ran.prompt, load_expected("pin_prompt"), "{yes:?}");
        assert_eq!(ran.outcome.stdout, load_expected("pin_confirmed_rendered"));
        assert_eq!(
            sent(&server),
            vec![
                (
                    "unit.pin".to_string(),
                    json!({"unit": "@gdrive:Trip Plans/"})
                ),
                (
                    "unit.pin".to_string(),
                    json!({"unit": "@gdrive:Trip Plans/", "confirm": true})
                ),
            ],
            "{yes:?}"
        );
    }
}

#[test]
fn any_answer_but_yes_pins_nothing_and_exits_1() {
    for typed in ["", "n\n", "no\n", "\n", "maybe\n", "yy\n"] {
        let server = MockServer::start(vec![respond("pin_needs_confirm")]);
        let ran = exec(&server, &["pin", "@gdrive:Trip Plans/"], typed, true);
        assert_eq!(ran.outcome.exit_code, 1, "{typed:?}");
        assert_eq!(ran.outcome.stdout, "", "{typed:?}");
        assert_eq!(ran.outcome.stderr, "aborted: not pinned\n", "{typed:?}");
        // The only call was the one that writes nothing.
        assert_eq!(server.received().len(), 1, "{typed:?}");
        assert_eq!(
            server.received()[0].body["params"],
            json!({"unit": "@gdrive:Trip Plans/"})
        );
    }
}

#[test]
fn yes_skips_the_question_and_sends_confirm_in_the_one_call() {
    let server = MockServer::start(vec![respond("pin_confirmed")]);
    let ran = exec(&server, &["pin", "--yes", "@gdrive:Trip Plans/"], "", false);
    assert_eq!(ran.outcome.exit_code, 0, "{}", ran.outcome.stderr);
    assert_eq!(ran.prompt, "");
    assert_eq!(ran.outcome.stdout, load_expected("pin_confirmed_rendered"));
    assert_eq!(
        sent(&server),
        vec![(
            "unit.pin".to_string(),
            json!({"unit": "@gdrive:Trip Plans/", "confirm": true})
        )]
    );
}

#[test]
fn without_a_terminal_and_without_yes_a_large_pin_exits_1_and_pins_nothing() {
    // Even a typed yes on a pipe is not an answer.
    for typed in ["", "y\n"] {
        let server = MockServer::start(vec![respond("pin_needs_confirm")]);
        let ran = exec(&server, &["pin", "@gdrive:Trip Plans/"], typed, false);
        assert_eq!(ran.outcome.exit_code, 1, "{typed:?}");
        assert_eq!(ran.outcome.stdout, "");
        assert_eq!(
            ran.outcome.stderr,
            load_expected("pin_needs_confirm_noterminal_stderr")
        );
        assert_eq!(ran.prompt, "", "nothing is asked of a pipe");
        assert_eq!(server.received().len(), 1, "no second call: {typed:?}");
        assert!(server.received()[0].body["params"].get("confirm").is_none());
    }
}

#[test]
fn json_keeps_stdout_valid_while_the_question_goes_elsewhere() {
    let server = MockServer::start(vec![respond("pin_needs_confirm"), respond("pin_confirmed")]);
    let ran = exec(
        &server,
        &["pin", "--json", "@gdrive:Trip Plans/"],
        "y\n",
        true,
    );
    assert_eq!(ran.outcome.exit_code, 0);
    assert!(ran.prompt.contains("[y/N]"));
    let printed: serde_json::Value = serde_json::from_str(&ran.outcome.stdout).unwrap();
    assert_eq!(printed["status"], json!("pinned"));
}

// ---- refusals --------------------------------------------------------------

#[test]
fn every_refusal_is_exit_1_with_the_reason_on_stderr_and_nothing_on_stdout() {
    for (message, reason) in [
        ("no such unit: @g:nope.md", "unknown_unit"),
        ("nothing at nope.md in @g", "not_found"),
        ("../x is not a path inside the source", "invalid_path"),
        (
            "a Google Doc has no file to download; open it at https://docs.google.com/document/d/x/edit",
            "not_fetchable",
        ),
        ("@v:note.md is a file of a local source: there is nothing to pin", "not_remote"),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: err_envelope(-32011, message, Some(json!({"reason": reason}))),
        }]);
        let ran = exec(&server, &["pin", "@g:x.md"], "", true);
        assert_eq!(ran.outcome.exit_code, 1, "{reason}");
        assert_eq!(ran.outcome.stderr, format!("tephractl: {message}\n"), "{reason}");
        assert_eq!(ran.outcome.stdout, "", "{reason}");
        // A refusal is never retried and never followed by a confirmed call.
        assert_eq!(server.received().len(), 1, "{reason}");
    }
}

#[test]
fn a_lost_reply_is_not_replayed() {
    // `unit.pin` and `unit.unpin` are mutating: a dropped connection is
    // reported, not retried (a retry would answer "already pinned").
    for argv in [&["pin", "@g:a.md"][..], &["unpin", "@g:a.md"][..]] {
        let server = MockServer::start(vec![Behavior::Drop, respond("pin_file")]);
        let ran = exec(&server, argv, "", true);
        assert_eq!(ran.outcome.exit_code, 1, "{argv:?}");
        assert_eq!(server.received().len(), 1, "{argv:?}");
    }
}

// ---- unpin -----------------------------------------------------------------

#[test]
fn unpin_says_the_files_stay_cached_and_makes_one_call() {
    let server = MockServer::start(vec![respond("unpin_ok")]);
    let ran = exec(&server, &["unpin", "@gdrive:Trip Plans/"], "", false);
    assert_eq!(ran.outcome.exit_code, 0, "{}", ran.outcome.stderr);
    assert_eq!(ran.outcome.stdout, load_expected("unpin_rendered"));
    assert_eq!(ran.prompt, "");
    assert_eq!(
        sent(&server),
        vec![(
            "unit.unpin".to_string(),
            json!({"unit": "@gdrive:Trip Plans/"})
        )]
    );

    let server = MockServer::start(vec![respond("unpin_ok")]);
    let ran = exec(
        &server,
        &["unpin", "--json", "@gdrive:Trip Plans/"],
        "",
        false,
    );
    assert_eq!(ran.outcome.stdout, load_expected("unpin_json"));
}

#[test]
fn unpinning_what_is_not_pinned_exits_0_changes_nothing_and_says_so() {
    let server = MockServer::start(vec![respond("unpin_not_pinned")]);
    let ran = exec(&server, &["unpin", "@gdrive:Trip Plans/"], "", false);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(
        ran.outcome.stdout,
        load_expected("unpin_not_pinned_rendered")
    );

    // A file under a pinned folder is kept by the folder: say which.
    let server = MockServer::start(vec![respond("unpin_covered")]);
    let ran = exec(&server, &["unpin", "@gdrive:Trip Plans/a.md"], "", false);
    assert_eq!(ran.outcome.exit_code, 0);
    assert_eq!(ran.outcome.stdout, load_expected("unpin_covered_rendered"));
}

#[test]
fn hostile_text_in_a_name_or_warning_is_printed_without_control_characters() {
    let mut body = load_fixture("pin_over_cap");
    body["result"]["unit"] = json!("@g:evil\u{1b}[31m/");
    body["result"]["warning"] = json!("w\u{7}arn\u{202e}ing");
    let server = MockServer::start(vec![Behavior::Respond { status: 200, body }]);
    let ran = exec(&server, &["pin", "@g:big/"], "", false);
    assert!(
        !ran.outcome
            .stdout
            .chars()
            .any(|c| c == '\u{1b}' || c == '\u{7}' || c == '\u{202e}'),
        "{:?}",
        ran.outcome.stdout
    );
}
