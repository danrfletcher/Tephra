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
