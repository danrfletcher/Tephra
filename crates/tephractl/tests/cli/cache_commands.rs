//! `tests/cli/cache_commands.rs` (PR-8 G12/G20): `tephractl cache status |
//! set-limit <limit> | on <source> | off <source>`, from argv to output.
//! Rendered and `--json` output are compared byte for byte with golden
//! files; the params sent are asserted exactly. The limit is not parsed
//! here: a bad one is the daemon's refusal, shown with exit 1.

use serde_json::json;
use tephractl::cli::{self, Action, ArgsError, Command, ParsedArgs};
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{load_expected, load_fixture, Behavior, MockServer};

fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn respond(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

fn execute(server: &MockServer, argv: &[&str], json: bool) -> commands::CommandOutcome {
    let command = run(argv).unwrap().command;
    let client = RpcClient::new(server.addr.clone(), "test-token".to_string());
    let mut stdin: &[u8] = &[];
    commands::execute(&command, json, &client, &mut stdin)
}

// ---- argument parsing ------------------------------------------------------

#[test]
fn parses_every_subcommand() {
    assert_eq!(
        run(&["cache", "status"]).unwrap().command,
        Command::CacheStatus
    );
    assert_eq!(
        run(&["cache", "set-limit", "10GB"]).unwrap().command,
        Command::CacheSetLimit {
            limit: "10GB".into()
        }
    );
    assert_eq!(
        run(&["cache", "set-limit", "500", "MiB", "--json"])
            .unwrap()
            .command,
        Command::CacheSetLimit {
            limit: "500 MiB".into()
        }
    );
    assert_eq!(
        run(&["cache", "off", "g"]).unwrap().command,
        Command::CacheSet {
            source: "g".into(),
            on: false
        }
    );
    assert_eq!(
        run(&["cache", "on", "g", "--json"]).unwrap().command,
        Command::CacheSet {
            source: "g".into(),
            on: true
        }
    );
    let parsed = run(&["cache", "status", "--addr", "127.0.0.1:1", "--json"]).unwrap();
    assert!(parsed.json);
    assert_eq!(parsed.addr.as_deref(), Some("127.0.0.1:1"));
}

#[test]
fn a_limit_the_daemon_would_refuse_is_still_sent_not_judged_here() {
    for bad in ["abc", "-5MB", "1.5.2GB", "10"] {
        assert_eq!(
            run(&["cache", "set-limit", bad]).unwrap().command,
            Command::CacheSetLimit { limit: bad.into() },
            "{bad}"
        );
    }
}

#[test]
fn argv_mistakes_are_refused_before_any_request() {
    for argv in [
        vec!["cache"],
        vec!["cache", "wipe"],
        vec!["cache", "status", "extra"],
        vec!["cache", "set-limit"],
        vec!["cache", "set-limit", "1", "GB", "more"],
        vec!["cache", "on"],
        vec!["cache", "off", "a", "b"],
        vec!["cache", "off", "g", "--nope"],
    ] {
        assert!(run(&argv).is_err(), "{argv:?}");
    }
}

// ---- cache status ------------------------------------------------------------

#[test]
fn status_over_the_limit_says_so_and_puts_git_on_its_own_line() {
    let server = respond("cache_status_over_limit");
    let outcome = execute(&server, &["cache", "status"], false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        load_expected("cache_status_over_limit_rendered")
    );
    assert!(outcome.stdout.contains("over limit"));
    assert!(outcome.stdout.contains("\n.git        4096 bytes"));
    assert_eq!(outcome.stderr, "");
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "cache.status");
    assert_eq!(received[0].body["params"], json!({}));
}

#[test]
fn status_under_the_limit_has_no_over_limit_line_and_shows_each_sources_flag() {
    let server = respond("cache_status_under_limit");
    let outcome = execute(&server, &["cache", "status"], false);
    assert_eq!(
        outcome.stdout,
        load_expected("cache_status_under_limit_rendered")
    );
    assert!(!outcome.stdout.contains("over limit"));
    assert!(outcome.stdout.contains("notes-drive          off"));
}

#[test]
fn status_warns_when_pinned_files_alone_are_over_the_limit() {
    let server = respond("cache_status_pins_exceed");
    let outcome = execute(&server, &["cache", "status"], false);
    assert_eq!(
        outcome.stdout,
        load_expected("cache_status_pins_exceed_rendered")
    );
    assert!(outcome.stdout.contains("warning: pinned files alone"));
}

#[test]
fn status_json_prints_the_whole_answer() {
    let server = respond("cache_status_over_limit");
    let outcome = execute(&server, &["cache", "status"], true);
    assert_eq!(
        outcome.stdout,
        load_expected("cache_status_over_limit_json")
    );
}

// ---- cache set-limit -----------------------------------------------------------

#[test]
fn set_limit_sends_the_text_as_typed_and_shows_what_the_pass_did() {
    let server = respond("cache_set_limit_success");
    let outcome = execute(&server, &["cache", "set-limit", "1MB"], false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("cache_set_limit_rendered"));
    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "cache.set_limit");
    assert_eq!(received[0].body["params"], json!({"limit": "1MB"}));
}

#[test]
fn set_limit_with_two_words_sends_one_string() {
    let server = respond("cache_set_limit_success");
    execute(&server, &["cache", "set-limit", "1", "MB"], false);
    assert_eq!(
        server.received()[0].body["params"],
        json!({"limit": "1 MB"})
    );
}

#[test]
fn set_limit_json_prints_the_whole_answer() {
    let server = respond("cache_set_limit_success");
    let outcome = execute(&server, &["cache", "set-limit", "1MB"], true);
    assert_eq!(outcome.stdout, load_expected("cache_set_limit_json"));
}

#[test]
fn a_refused_limit_is_exit_1_with_the_reason_and_the_kept_limit() {
    let server = respond("cache_set_limit_refused");
    let outcome = execute(&server, &["cache", "set-limit", "abc"], false);
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert!(outcome
        .stderr
        .starts_with("tephractl: abc is not a usable cache limit"));
    assert!(
        outcome.stderr.contains("the limit stays 10 GB"),
        "{}",
        outcome.stderr
    );
    assert_eq!(server.received().len(), 1, "never retried");
}

// ---- cache on / off --------------------------------------------------------------

#[test]
fn off_sends_the_flag_and_says_when_a_file_will_go() {
    let server = respond("cache_off_success");
    let outcome = execute(&server, &["cache", "off", "g"], false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "Cache for \"g\" is off: a fetched file is dropped about 10 minutes after its last use. Pinned files stay.\n"
    );
    let received = server.received();
    assert_eq!(received[0].body["method"], "source.set_cache");
    assert_eq!(
        received[0].body["params"],
        json!({"name": "g", "cache": false})
    );
}

#[test]
fn on_sends_the_flag() {
    let server = respond("cache_on_success");
    let outcome = execute(&server, &["cache", "on", "g"], false);
    assert_eq!(
        outcome.stdout,
        "Cache for \"g\" is on: fetched files stay until the limit needs the room.\n"
    );
    assert_eq!(
        server.received()[0].body["params"],
        json!({"name": "g", "cache": true})
    );
}

#[test]
fn setting_the_flag_it_already_has_says_so() {
    let server = respond("cache_off_again");
    let outcome = execute(&server, &["cache", "off", "g"], false);
    assert_eq!(outcome.stdout, "Cache for \"g\" is already off.\n");
}

#[test]
fn an_unknown_source_is_exit_1_with_the_reason_on_stderr() {
    let server = respond("cache_off_unknown_source");
    let outcome = execute(&server, &["cache", "off", "nope"], false);
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "tephractl: no source is named nope\n");
}
