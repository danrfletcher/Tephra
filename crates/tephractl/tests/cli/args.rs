//! `tests/cli/args.test` (G8/E "invalid characters"/"conflicting
//! flags"): pure argv-parsing acceptance checks against the real
//! CLI surface, re-expressed as integration-level checks over
//! `tephractl::cli::parse_args` in addition to the module's own unit
//! tests in `src/cli.rs` — these exercise exactly the invocations the
//! spec's examples use, end to end from a `Vec<&str>` to a
//! [`tephractl::cli::Command`].

use tephractl::cli::{self, Action, Command};

fn run(args: &[&str]) -> Result<tephractl::cli::ParsedArgs, cli::ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

#[test]
fn source_add_happy_path() {
    let parsed = run(&["source", "add", "notes", "/home/dan/Notes"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        }
    );
}

#[test]
fn source_add_rejects_invalid_name_before_any_network_concept_exists() {
    for bad_name in ["my notes", "my/notes", "my@notes"] {
        let err = cli::parse_args(["source", "add", bad_name, "/path"]).unwrap_err();
        assert!(
            err.0.contains("invalid source name"),
            "expected invalid-name error for {bad_name:?}, got: {err}"
        );
    }
}

#[test]
fn link_refactor_rejects_yes_without_apply_as_a_conflicting_flag() {
    let err = cli::parse_args([
        "link", "refactor", "--source", "notes", "--old", "A.md", "--new", "B.md", "--yes",
    ])
    .unwrap_err();
    assert!(err.0.contains("--yes"));
    assert!(err.0.contains("--apply"));
}

#[test]
fn link_resolve_preserves_unicode_wikilink_target_exactly() {
    let target = "[[@notes:Café Notes]]";
    let parsed = run(&["link", "resolve", target]).unwrap();
    match parsed.command {
        Command::LinkResolve { target: t, .. } => assert_eq!(t, target),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn help_with_no_args_and_explicit_flag_both_print_usage() {
    assert_eq!(cli::parse_args(Vec::<&str>::new()).unwrap(), Action::Help);
    assert_eq!(cli::parse_args(["-h"]).unwrap(), Action::Help);
    assert_eq!(cli::parse_args(["--help"]).unwrap(), Action::Help);
}

#[test]
fn missing_required_positional_args_produce_a_usage_error_not_a_panic() {
    assert!(cli::parse_args(["source", "add", "notes"]).is_err());
    assert!(cli::parse_args(["link", "resolve"]).is_err());
    assert!(cli::parse_args(["link", "refactor", "--source", "notes"]).is_err());
}

// ---- daemon (G14) ----------------------------------------------------

#[test]
fn daemon_install_parses_bare_with_from_and_with_json() {
    let parsed = run(&["daemon", "install"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::DaemonInstall {
            from: None,
            keychain: None
        }
    );
    assert!(!parsed.json);

    let parsed = run(&[
        "daemon",
        "install",
        "--from",
        "/tmp/build/tephrad",
        "--json",
    ])
    .unwrap();
    assert_eq!(
        parsed.command,
        Command::DaemonInstall {
            from: Some("/tmp/build/tephrad".to_string()),
            keychain: None
        }
    );
    assert!(parsed.json);
}

#[test]
fn daemon_install_from_needs_a_value() {
    let err = cli::parse_args(["daemon", "install", "--from"]).unwrap_err();
    assert!(err.0.contains("--from requires a value"), "{err}");
}

#[test]
fn the_hidden_keychain_option_parses_but_is_not_in_the_help_text() {
    let parsed = run(&["daemon", "install", "--keychain", "/tmp/t.keychain-db"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::DaemonInstall {
            from: None,
            keychain: Some("/tmp/t.keychain-db".to_string())
        }
    );
    assert!(!cli::USAGE.contains("--keychain"));
}

#[test]
fn daemon_uninstall_and_status_parse() {
    assert_eq!(
        run(&["daemon", "uninstall"]).unwrap().command,
        Command::DaemonUninstall
    );
    let parsed = run(&["daemon", "status"]).unwrap();
    assert_eq!(parsed.command, Command::DaemonStatus);
    assert!(!parsed.json);
    let parsed = run(&["daemon", "status", "--json"]).unwrap();
    assert_eq!(parsed.command, Command::DaemonStatus);
    assert!(parsed.json);
    let parsed = run(&["--json", "daemon", "status"]).unwrap();
    assert!(parsed.json);
}

#[test]
fn daemon_verbs_reject_flags_they_do_not_take() {
    for args in [
        &["daemon", "uninstall", "--json"][..],
        &["daemon", "status", "--from", "x"][..],
        &["daemon", "install", "--bogus"][..],
    ] {
        let err = cli::parse_args(args.iter().copied()).unwrap_err();
        assert!(err.0.contains("unrecognized argument"), "{args:?}: {err}");
    }
}

#[test]
fn an_unknown_daemon_verb_is_an_error_that_lists_the_real_ones() {
    let err = cli::parse_args(["daemon", "restart"]).unwrap_err();
    assert!(
        err.0.contains("unrecognized daemon subcommand: restart"),
        "{err}"
    );
    assert!(err.0.contains("install, uninstall, or status"), "{err}");

    let err = cli::parse_args(["daemon"]).unwrap_err();
    assert!(err.0.contains("expected a daemon subcommand"), "{err}");
}

#[test]
fn the_help_text_lists_the_daemon_commands() {
    for line in [
        "tephractl daemon install [--from <path>] [--json]",
        "tephractl daemon uninstall",
        "tephractl daemon status [--json]",
    ] {
        assert!(cli::USAGE.contains(line), "help is missing {line:?}");
    }
    assert!(cli::USAGE.contains("macOS only"));
}

/// G14: `daemon install|uninstall|status` need neither the token file
/// nor a running daemon. Runs the real binary with a HOME that has no
/// token and a label that can't be the user's real agent, and checks
/// it never complains about a token or a connection.
#[test]
fn daemon_commands_run_when_no_token_file_exists() {
    let home = tempfile::tempdir().unwrap();
    assert!(!home.path().join(".local/share/tephra/token").exists());

    for args in [&["daemon", "status"][..], &["daemon", "uninstall"][..]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(args)
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env(
                "TEPHRA_LAUNCHD_LABEL",
                "com.danrfletcher.tephrad.test-notoken",
            )
            .output()
            .expect("run tephractl");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.to_lowercase().contains("token"),
            "{args:?}: {stderr}"
        );
        assert!(
            !stderr.to_lowercase().contains("connect"),
            "{args:?}: {stderr}"
        );

        if cfg!(target_os = "macos") {
            assert_eq!(output.status.code(), Some(0), "{args:?}: {stderr}");
            assert_eq!(stdout.trim(), "not installed", "{args:?}");
        } else {
            assert_eq!(output.status.code(), Some(1), "{args:?}");
            assert_eq!(
                stderr.trim(),
                "tephractl daemon: launchd is macOS only; start tephrad by hand"
            );
        }
    }

    // And a non-daemon command still fails on the missing token, so
    // the bypass above is specific to `daemon`.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
        .args(["source", "list"])
        .env_clear()
        .env("HOME", home.path())
        .output()
        .expect("run tephractl");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .to_lowercase()
        .contains("token"));
}
