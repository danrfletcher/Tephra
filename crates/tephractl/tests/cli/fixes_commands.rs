//! `fixes` (G18) against the mock RPC server: the exact `fix.list`
//! request that goes out, `--since` for `30m`, `2h`, `7d` and ISO dates,
//! rendered and `--json` output byte for byte against golden fixtures,
//! and the usage errors (exit 2, nothing sent). The command only reads:
//! one request, always `fix.list` (F9).
//!
//! Times are rendered in the machine's local zone, so the golden file
//! carries `{T<epoch>}` placeholders that the test fills with
//! `render::local_time`, which `render.rs` tests against `mktime`.

use serde_json::json;
use tephractl::cli::{parse_args, Action, Command};
use tephractl::commands;
use tephractl::render::local_time;
use tephractl::since::Since;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn respond(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

fn run(server: &MockServer, since: Option<Since>, json: bool) -> commands::CommandOutcome {
    let mut stdin: &[u8] = &[];
    commands::execute(
        &Command::Fixes { since },
        json,
        &client_for(server),
        &mut stdin,
    )
}

fn expected_rendered() -> String {
    let mut text = load_expected("fixes_rendered");
    for epoch in [
        1_794_790_000i64,
        1_794_789_000,
        1_794_788_000,
        1_794_787_000,
    ] {
        text = text.replace(&format!("{{T{epoch}}}"), &local_time(epoch));
    }
    text
}

fn parsed(args: &[&str]) -> (Command, bool) {
    match parse_args(args.iter().copied()).expect("parses") {
        Action::Run(p) => (p.command, p.json),
        Action::Help => panic!("help"),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn fixes_sends_one_fix_list_with_no_params_when_no_since_is_given() {
    let server = respond("fix_list");
    let outcome = run(&server, None, false);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let sent = server.received();
    assert_eq!(sent.len(), 1, "one request, nothing else");
    assert_eq!(sent[0].body["method"], "fix.list");
    assert_eq!(sent[0].body["params"], json!({}));
    assert_eq!(
        sent[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-token")
    );
}

#[test]
fn fixes_renders_one_line_per_entry_exactly() {
    let server = respond("fix_list");
    let outcome = run(&server, None, false);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, expected_rendered());
    assert_eq!(outcome.stderr, "");
}

#[test]
fn the_golden_path_line_names_the_rename_its_three_files_and_applied() {
    let server = respond("fix_list");
    let outcome = run(&server, None, false);
    let line = outcome
        .stdout
        .lines()
        .find(|l| l.contains("Training Plan.md"))
        .expect("the GP6 line");
    assert_eq!(
        line,
        format!(
            "{} rename Training Plan.md -> Plan.md (3 files) applied",
            local_time(1_794_788_000)
        )
    );
}

#[test]
fn fixes_json_prints_the_raw_array_byte_for_byte_with_the_same_values_as_the_lines() {
    let server = respond("fix_list");
    let outcome = run(&server, None, true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("fixes_json"));

    // The same entry, as data: the fields behind the human line.
    let data: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
    let entry = &data[2];
    assert_eq!(entry["kind"], "rename");
    assert_eq!(entry["old"], "Training Plan.md");
    assert_eq!(entry["new"], "Plan.md");
    assert_eq!(entry["files_changed"], 3);
    assert_eq!(entry["status"], "applied");
    assert_eq!(entry["time"], 1_794_788_000);
}

#[test]
fn an_empty_log_prints_no_fixes_and_an_empty_array_with_json() {
    let server = respond("fix_list_empty");
    let outcome = run(&server, None, false);
    assert_eq!(
        (outcome.exit_code, outcome.stdout.as_str()),
        (0, "no fixes\n")
    );

    let server = respond("fix_list_empty");
    let outcome = run(&server, None, true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("fixes_empty_json"));
}

#[test]
fn since_durations_send_epoch_seconds_that_long_ago() {
    for (arg, secs) in [("30m", 1_800), ("2h", 7_200), ("7d", 604_800)] {
        let (command, _) = parsed(&["fixes", "--since", arg]);
        assert_eq!(
            command,
            Command::Fixes {
                since: Some(Since::Ago(secs))
            }
        );

        let server = respond("fix_list_empty");
        let before = now();
        let mut stdin: &[u8] = &[];
        let outcome = commands::execute(&command, false, &client_for(&server), &mut stdin);
        let after = now();

        assert_eq!(outcome.exit_code, 0, "{arg}");
        let sent = server.received();
        assert_eq!(sent[0].body["method"], "fix.list", "{arg}");
        let since = sent[0].body["params"]["since"]
            .as_i64()
            .expect("since is an integer");
        assert!(
            (before - secs..=after - secs).contains(&since),
            "{arg}: {since} not within {} and {}",
            before - secs,
            after - secs
        );
        assert_eq!(
            sent[0].body["params"].as_object().unwrap().len(),
            1,
            "{arg}: only since is sent"
        );
    }
}

#[test]
fn since_iso_dates_send_that_moment() {
    // With a zone: exact, whatever the machine's zone.
    for (arg, epoch) in [
        ("2026-11-16T00:00:00Z", 1_794_787_200i64),
        ("2026-11-16T09:30Z", 1_794_787_200 + 34_200),
        ("2026-11-16T10:30:00+01:00", 1_794_787_200 + 34_200),
    ] {
        let (command, _) = parsed(&["fixes", "--since", arg]);
        let server = respond("fix_list_empty");
        let mut stdin: &[u8] = &[];
        commands::execute(&command, false, &client_for(&server), &mut stdin);
        assert_eq!(
            server.received()[0].body["params"],
            json!({"since": epoch}),
            "{arg}"
        );
    }

    // Without one: the machine's local time, the zone the lines print in.
    let (command, _) = parsed(&["fixes", "--since", "2026-11-16T09:30"]);
    let Command::Fixes {
        since: Some(Since::At(at)),
    } = command
    else {
        panic!("not an absolute time");
    };
    assert_eq!(local_time(at), "2026-11-16 09:30");
    let (date_only, _) = parsed(&["fixes", "--since", "2026-11-16"]);
    let Command::Fixes {
        since: Some(Since::At(midnight)),
    } = date_only
    else {
        panic!("not an absolute time");
    };
    assert_eq!(local_time(midnight), "2026-11-16 00:00");
}

#[test]
fn json_and_addr_flags_work_before_or_after_since() {
    let (a, json_a) = parsed(&["fixes", "--since", "2h", "--json"]);
    let (b, json_b) = parsed(&["--json", "fixes", "--since", "2h"]);
    assert_eq!(a, b);
    assert!(json_a && json_b);
    let Action::Run(p) = parse_args(["fixes", "--addr", "127.0.0.1:9"]).unwrap() else {
        panic!()
    };
    assert_eq!(p.addr.as_deref(), Some("127.0.0.1:9"));
    assert_eq!(p.command, Command::Fixes { since: None });
}

#[test]
fn bad_since_and_stray_arguments_are_usage_errors_that_send_nothing() {
    for args in [
        &["fixes", "--since", "yesterday"][..],
        &["fixes", "--since", "0m"],
        &["fixes", "--since", "-5m"],
        &["fixes", "--since", "5x"],
        &["fixes", "--since", "2026-13-01"],
        &["fixes", "--since", "1969-12-31"],
        &["fixes", "--since"],
        &["fixes", "--since", "1h", "--since", "2h"],
        &["fixes", "--limit", "5"],
        &["fixes", "extra"],
        &["fixes", "--undo"],
    ] {
        let err = parse_args(args.iter().copied()).expect_err(&format!("{args:?} parsed"));
        assert!(!err.0.is_empty());
    }
    let err = parse_args(["fixes", "--since", "yesterday"]).unwrap_err();
    assert!(err.0.contains("--since") && err.0.contains("30m"), "{err}");
}

#[test]
fn the_real_binary_exits_2_with_usage_on_stderr_and_sends_no_request() {
    for args in [
        &["fixes", "--since", "yesterday"][..],
        &["fixes", "--since", "0m"],
        &["fixes", "--since", "2026-11-1é5"],
        &["fixes", "--since"],
    ] {
        // No token file under this HOME and nothing listening: a request
        // would be a different error (and exit 1).
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(args)
            .env_clear()
            .env("HOME", home.path())
            .output()
            .expect("run tephractl");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.starts_with("tephractl: "), "{args:?}: {stderr}");
        assert!(stderr.contains("USAGE:"), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_daemon_error_is_exit_1_not_a_usage_error() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32602, "\"limit\" must be an integer, 1 or more", None),
    }]);
    let outcome = run(&server, None, false);
    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.stderr.contains("limit"), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
}

#[test]
fn since_works_out_epoch_seconds_from_the_clock_it_is_given() {
    assert_eq!(Since::Ago(7_200).epoch(1_000_000), 992_800);
    assert_eq!(Since::At(5).epoch(1_000_000), 5);
}

/// G13: a source rename names both sources already (`@old:` and `@new:`),
/// so its line carries them as they are, not with a second prefix.
#[test]
fn a_source_rename_line_names_the_old_and_new_source_once() {
    let body = json!({"jsonrpc": "2.0", "id": 1, "result": [{
        "id": 7,
        "time": 1_794_790_000,
        "kind": "source_rename",
        "sources": ["tephrad", "daemon"],
        "old": "@tephrad:",
        "new": "@daemon:",
        "files_changed": 3,
        "status": "applied"
    }]});
    let server = MockServer::start(vec![Behavior::Respond { status: 200, body }]);
    let outcome = run(&server, None, false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!(
            "{} source_rename @tephrad: -> @daemon: (3 files) applied\n",
            local_time(1_794_790_000)
        )
    );
}
