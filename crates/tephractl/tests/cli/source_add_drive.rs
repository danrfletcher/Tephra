//! `tests/cli/source_add_drive.rs` (PR-10 G4/GP3): `tephractl source add
//! drive <name> <folder>`, from argv to rendered output, and how `source
//! list` shows a Drive source. Output is compared byte for byte with the
//! golden files `tests/fixtures/expected/source_add_drive_*.txt`; the params
//! sent are asserted exactly; a refusal that argv alone decides is made
//! before any request.

use tephractl::cli::{self, Action, ArgsError, Command, ParsedArgs};
use tephractl::commands;
use tephractl::render;
use tephractl::transport::RpcClient;

use crate::support::{load_expected, load_fixture, Behavior, MockServer};

fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn drive(name: &str, folder: &str) -> Command {
    Command::SourceAddDrive {
        name: name.to_string(),
        folder: folder.to_string(),
    }
}

fn respond(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

fn execute(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let mut stdin: &[u8] = &[];
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    commands::execute(command, json, &client, &mut stdin)
}

// ---- argument parsing ------------------------------------------------------

#[test]
fn a_folder_is_a_path_an_id_or_a_url_taken_as_typed() {
    for folder in [
        "Notes",
        "Work/Notes 2026",
        "1AbCdEfGhIjK",
        "https://drive.google.com/drive/folders/1AbCdEfGhIjK?usp=sharing",
        "Café/日本語",
    ] {
        let parsed = run(&["source", "add", "drive", "gdrive", folder]).unwrap();
        assert_eq!(parsed.command, drive("gdrive", folder), "{folder}");
        assert!(!parsed.json);
    }
}

#[test]
fn json_and_addr_follow_the_two_positionals() {
    let parsed = run(&[
        "source",
        "add",
        "drive",
        "gdrive",
        "Notes",
        "--json",
        "--addr",
        "127.0.0.1:9",
    ])
    .unwrap();
    assert_eq!(parsed.command, drive("gdrive", "Notes"));
    assert!(parsed.json);
    assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:9"));
}

#[test]
fn the_source_name_rules_are_the_local_ones() {
    for bad in ["", "a/b", "with space", "a@b"] {
        assert!(
            run(&["source", "add", "drive", bad, "Notes"]).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn an_empty_or_control_character_folder_is_refused_before_any_request() {
    let server = MockServer::start(vec![]);
    for bad in ["", "   ", "No\ntes"] {
        let err = run(&["source", "add", "drive", "g", bad]).unwrap_err();
        assert!(err.0.contains("path from My Drive"), "{bad:?}: {err}");
    }
    assert!(server.received().is_empty());
}

#[test]
fn stray_flags_and_a_third_positional_are_errors() {
    let err = run(&["source", "add", "drive", "g", "Notes", "extra"]).unwrap_err();
    assert!(err.0.contains("quote a folder"), "{err}");
    let err = run(&["source", "add", "drive", "g", "Notes", "--branch", "x"]).unwrap_err();
    assert!(err.0.contains("unrecognized argument: --branch"), "{err}");
}

#[test]
fn one_argument_after_drive_adds_a_local_source_named_drive() {
    let parsed = run(&["source", "add", "drive", "/home/dan/drive-notes"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "drive".to_string(),
            path: "/home/dan/drive-notes".to_string(),
        }
    );
    assert!(
        run(&["source", "add", "drive", "/p", "--json"])
            .unwrap()
            .json
    );
    // Two arguments are always the Drive form, even when the name is `drive`.
    assert_eq!(
        run(&["source", "add", "drive", "drive", "Notes"])
            .unwrap()
            .command,
        drive("drive", "Notes")
    );
    let err = run(&["source", "add", "drive"]).unwrap_err();
    assert!(err.0.contains("source add requires"), "{err}");
}

#[test]
fn other_names_and_the_github_form_are_untouched() {
    let parsed = run(&["source", "add", "drive-notes", "/p"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "drive-notes".to_string(),
            path: "/p".to_string(),
        }
    );
    assert!(matches!(
        run(&["source", "add", "github", "g", "o/r"])
            .unwrap()
            .command,
        Command::SourceAddGithub { .. }
    ));
}

// ---- what is sent, and what is shown --------------------------------------

#[test]
fn add_sends_source_add_drive_with_name_and_folder_and_prints_what_was_listed() {
    let server = respond("source_add_drive_success");
    let outcome = execute(&server, &drive("gdrive", "Work/Notes 2026"), false);
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_add_drive_rendered"));
    assert_eq!(outcome.stderr, "");
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "source.add_drive");
    assert_eq!(
        received[0].body["params"],
        serde_json::json!({"name": "gdrive", "folder": "Work/Notes 2026"})
    );
}

#[test]
fn add_json_prints_the_daemons_result_byte_for_byte() {
    let server = respond("source_add_drive_success");
    let outcome = execute(&server, &drive("gdrive", "Notes"), true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_add_drive_json"));
}

#[test]
fn an_empty_folder_says_no_text_to_index() {
    let server = respond("source_add_drive_empty");
    let outcome = execute(&server, &drive("gdrive", "Notes"), false);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_add_drive_empty_rendered")
    );
}

#[test]
fn daemon_refusals_print_the_plain_reason_exit_1_and_nothing_on_stdout() {
    for (fixture, message) in [
        (
            "source_add_drive_refused_auth_required",
            "no Google token is stored: run `tephractl auth google` first",
        ),
        (
            "source_add_drive_refused_not_found",
            "no such folder in your Drive (a folder in a Shared Drive, or one shared with you and not added to My Drive, cannot be added)",
        ),
        (
            "source_add_drive_refused_shared_drive",
            "that folder is in a Shared Drive; only folders of My Drive can be added",
        ),
    ] {
        let server = respond(fixture);
        let outcome = execute(&server, &drive("g", "Notes"), false);
        assert_eq!(outcome.exit_code, 1, "{fixture}");
        assert_eq!(outcome.stderr, format!("tephractl: {message}\n"), "{fixture}");
        assert_eq!(outcome.stdout, "", "{fixture}");
        assert_eq!(server.received().len(), 1, "{fixture}: never retried");
    }
}

#[test]
fn source_list_shows_indexing_progress_then_the_rate_limit_note_and_no_git_or_branch() {
    let rows = load_fixture("source_list_drive_indexing")["result"]
        .as_array()
        .unwrap()
        .clone();
    let shown = render::render_source_table_with(&rows, &|_| "never".to_string());
    assert_eq!(shown, load_expected("source_add_drive_source_list"));
    assert!(!shown.contains(".git"), "{shown}");
    assert!(!shown.contains("branch"), "{shown}");
}

#[test]
fn the_usage_text_documents_the_drive_form() {
    assert!(cli::USAGE.contains("tephractl source add drive <name> <folder>"));
    assert!(cli::USAGE.contains("DRIVE SOURCES:"));
}
