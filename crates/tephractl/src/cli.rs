//! Hand-rolled argv parsing for every `tephractl` subcommand (G8),
//! following the same deliberate "no argument-parsing crate" choice
//! `tephrad::cli` documents for its own flags. Pure and
//! network-free: validation that can be decided from the argument
//! list alone (missing args, invalid source names, conflicting
//! flags) happens entirely in this module, *before* `main.rs` ever
//! constructs an [`crate::transport::RpcClient`] — one of this PR's
//! fence checks ("`source add` with a name containing invalid
//! characters ... is rejected client-side ... before any RPC
//! round-trip").

use std::fmt;

pub const USAGE: &str = "\
tephractl - command-line client for the tephrad daemon

USAGE:
    tephractl source add <name> <path> [--json]
    tephractl source remove <name> [--json]
    tephractl source list [--json]
    tephractl status [--json]
    tephractl link resolve [--source <name>] <target> [--json]
    tephractl link refactor --source <name> --old <path> --new <path> [--apply] [--yes] [--json]
    tephractl -h | --help

GLOBAL OPTIONS:
    --json          Print raw structured JSON instead of human-readable text
    --addr <addr>   Daemon address to connect to (default: 127.0.0.1:4812)
    -h, --help      Print this help and exit
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SourceAdd {
        name: String,
        path: String,
    },
    SourceRemove {
        name: String,
    },
    SourceList,
    Status,
    LinkResolve {
        target: String,
        default_source: Option<String>,
    },
    LinkRefactor {
        source: String,
        old_path: String,
        new_path: String,
        apply: bool,
        yes: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArgs {
    pub command: Command,
    pub json: bool,
    pub addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsError(pub String);

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Either a fully parsed command, or a bare request to print help
/// (`-h`/`--help`, or no arguments at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Help,
    Run(ParsedArgs),
}

/// Characters a source name must never contain (edge case: "a name
/// containing invalid characters (spaces, `/`, `@`) is rejected
/// client-side"). Whitespace is checked separately via
/// `char::is_whitespace` so it covers more than just the literal
/// space character.
const SOURCE_NAME_FORBIDDEN_CHARS: [char; 2] = ['/', '@'];

fn validate_source_name(name: &str) -> Result<(), ArgsError> {
    if name.is_empty() {
        return Err(ArgsError("source name must not be empty".to_string()));
    }
    if name.chars().any(char::is_whitespace)
        || name
            .chars()
            .any(|c| SOURCE_NAME_FORBIDDEN_CHARS.contains(&c))
    {
        return Err(ArgsError(format!(
            "invalid source name \"{name}\": must not contain spaces, '/', or '@'"
        )));
    }
    Ok(())
}

/// Consumes the next token or errors with `flag` named in the
/// message, matching `tephrad::cli::parse_args`'s `--config` handling.
fn take_value<'a, I>(iter: &mut I, flag: &str) -> Result<String, ArgsError>
where
    I: Iterator<Item = &'a str>,
{
    iter.next()
        .map(str::to_string)
        .ok_or_else(|| ArgsError(format!("{flag} requires a value argument")))
}

/// Parses the full argv (excluding argv[0]) into an [`Action`].
pub fn parse_args<I, S>(args: I) -> Result<Action, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let owned: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    if owned.is_empty() {
        return Ok(Action::Help);
    }

    let mut iter = owned.iter().map(String::as_str);
    let head = iter.next().expect("non-empty checked above");

    if head == "-h" || head == "--help" {
        return Ok(Action::Help);
    }

    let (command, json, addr) = match head {
        "source" => parse_source(&mut iter)?,
        "status" => parse_status(&mut iter)?,
        "link" => parse_link(&mut iter)?,
        other => return Err(ArgsError(format!("unrecognized command: {other}"))),
    };

    Ok(Action::Run(ParsedArgs {
        command,
        json,
        addr,
    }))
}

/// Shared tail-flag loop: consumes `--json` / `--addr <addr>` from
/// whatever remains of `iter` once a subcommand's own positional
/// arguments have already been consumed. Any other remaining token is
/// an error (unknown flag / too many arguments).
fn parse_trailing_flags<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(bool, Option<String>), ArgsError> {
    let mut json = false;
    let mut addr = None;
    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
        }
    }
    Ok((json, addr))
}

fn parse_source<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let sub = iter.next().ok_or_else(|| {
        ArgsError("expected a source subcommand: add, remove, or list".to_string())
    })?;

    match sub {
        "add" => {
            let name = iter
                .next()
                .ok_or_else(|| ArgsError("source add requires <name> <path>".to_string()))?
                .to_string();
            validate_source_name(&name)?;
            let path = iter
                .next()
                .ok_or_else(|| ArgsError("source add requires <name> <path>".to_string()))?
                .to_string();
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceAdd { name, path }, json, addr))
        }
        "remove" => {
            let name = iter
                .next()
                .ok_or_else(|| ArgsError("source remove requires <name>".to_string()))?
                .to_string();
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceRemove { name }, json, addr))
        }
        "list" => {
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceList, json, addr))
        }
        other => Err(ArgsError(format!(
            "unrecognized source subcommand: {other} (expected add, remove, or list)"
        ))),
    }
}

fn parse_status<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let (json, addr) = parse_trailing_flags(iter)?;
    Ok((Command::Status, json, addr))
}

fn parse_link<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let sub = iter
        .next()
        .ok_or_else(|| ArgsError("expected a link subcommand: resolve or refactor".to_string()))?;

    match sub {
        "resolve" => parse_link_resolve(iter),
        "refactor" => parse_link_refactor(iter),
        other => Err(ArgsError(format!(
            "unrecognized link subcommand: {other} (expected resolve or refactor)"
        ))),
    }
}

fn parse_link_resolve<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut target: Option<String> = None;
    let mut default_source: Option<String> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--source" => default_source = Some(take_value(iter, "--source")?),
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if target.is_none() => target = Some(other.to_string()),
            other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
        }
    }

    let target = target.ok_or_else(|| ArgsError("link resolve requires a <target>".to_string()))?;
    Ok((
        Command::LinkResolve {
            target,
            default_source,
        },
        json,
        addr,
    ))
}

fn parse_link_refactor<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut source: Option<String> = None;
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;
    let mut apply = false;
    let mut yes = false;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--source" => source = Some(take_value(iter, "--source")?),
            "--old" => old_path = Some(take_value(iter, "--old")?),
            "--new" => new_path = Some(take_value(iter, "--new")?),
            "--apply" => apply = true,
            "--yes" => yes = true,
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
        }
    }

    let source =
        source.ok_or_else(|| ArgsError("link refactor requires --source <name>".to_string()))?;
    let old_path =
        old_path.ok_or_else(|| ArgsError("link refactor requires --old <path>".to_string()))?;
    let new_path =
        new_path.ok_or_else(|| ArgsError("link refactor requires --new <path>".to_string()))?;

    if yes && !apply {
        return Err(ArgsError(
            "--yes only makes sense together with --apply".to_string(),
        ));
    }

    Ok((
        Command::LinkRefactor {
            source,
            old_path,
            new_path,
            apply,
            yes,
        },
        json,
        addr,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
        match parse_args(args)? {
            Action::Help => panic!("expected Action::Run, got Help"),
            Action::Run(parsed) => Ok(parsed),
        }
    }

    #[test]
    fn no_args_is_help() {
        assert_eq!(parse_args(Vec::<&str>::new()).unwrap(), Action::Help);
    }

    #[test]
    fn help_flags() {
        assert_eq!(parse_args(["-h"]).unwrap(), Action::Help);
        assert_eq!(parse_args(["--help"]).unwrap(), Action::Help);
    }

    #[test]
    fn source_add_parses_name_and_path() {
        let parsed = run(&["source", "add", "notes", "/home/dan/Notes"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceAdd {
                name: "notes".to_string(),
                path: "/home/dan/Notes".to_string(),
            }
        );
        assert!(!parsed.json);
        assert_eq!(parsed.addr, None);
    }

    #[test]
    fn source_add_with_json_and_addr_flags() {
        let parsed = run(&[
            "source",
            "add",
            "notes",
            "/home/dan/Notes",
            "--json",
            "--addr",
            "127.0.0.1:9999",
        ])
        .unwrap();
        assert!(parsed.json);
        assert_eq!(parsed.addr, Some("127.0.0.1:9999".to_string()));
    }

    #[test]
    fn source_add_missing_path_is_an_error() {
        let err = parse_args(["source", "add", "notes"]).unwrap_err();
        assert!(err.0.contains("source add requires"));
    }

    #[test]
    fn source_add_rejects_name_with_space() {
        let err = parse_args(["source", "add", "my notes", "/path"]).unwrap_err();
        assert!(err.0.contains("invalid source name"));
    }

    #[test]
    fn source_add_rejects_name_with_slash() {
        let err = parse_args(["source", "add", "my/notes", "/path"]).unwrap_err();
        assert!(err.0.contains("invalid source name"));
    }

    #[test]
    fn source_add_rejects_name_with_at_sign() {
        let err = parse_args(["source", "add", "my@notes", "/path"]).unwrap_err();
        assert!(err.0.contains("invalid source name"));
    }

    #[test]
    fn source_remove_parses_name() {
        let parsed = run(&["source", "remove", "notes"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceRemove {
                name: "notes".to_string()
            }
        );
    }

    #[test]
    fn source_remove_requires_a_name() {
        let err = parse_args(["source", "remove"]).unwrap_err();
        assert!(err.0.contains("requires <name>"));
    }

    #[test]
    fn source_list_parses_with_no_positional_args() {
        let parsed = run(&["source", "list"]).unwrap();
        assert_eq!(parsed.command, Command::SourceList);
    }

    #[test]
    fn unknown_source_subcommand_errors() {
        let err = parse_args(["source", "frobnicate"]).unwrap_err();
        assert!(err.0.contains("unrecognized source subcommand"));
    }

    #[test]
    fn status_parses_with_json_flag() {
        let parsed = run(&["status", "--json"]).unwrap();
        assert_eq!(parsed.command, Command::Status);
        assert!(parsed.json);
    }

    #[test]
    fn link_resolve_parses_bare_target() {
        let parsed = run(&["link", "resolve", "[[@atlas-repo:README]]"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::LinkResolve {
                target: "[[@atlas-repo:README]]".to_string(),
                default_source: None,
            }
        );
    }

    #[test]
    fn link_resolve_parses_unicode_target_byte_for_byte() {
        let target = "[[@notes:Café Notes]]";
        let parsed = run(&["link", "resolve", target]).unwrap();
        match parsed.command {
            Command::LinkResolve { target: t, .. } => assert_eq!(t, target),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn link_resolve_with_default_source_flag() {
        let parsed = run(&["link", "resolve", "--source", "notes", "[[Target]]"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::LinkResolve {
                target: "[[Target]]".to_string(),
                default_source: Some("notes".to_string()),
            }
        );
    }

    #[test]
    fn link_resolve_requires_a_target() {
        let err = parse_args(["link", "resolve"]).unwrap_err();
        assert!(err.0.contains("requires a <target>"));
    }

    #[test]
    fn link_refactor_parses_all_required_flags() {
        let parsed = run(&[
            "link",
            "refactor",
            "--source",
            "notes",
            "--old",
            "OldDoc.md",
            "--new",
            "NewDoc.md",
        ])
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::LinkRefactor {
                source: "notes".to_string(),
                old_path: "OldDoc.md".to_string(),
                new_path: "NewDoc.md".to_string(),
                apply: false,
                yes: false,
            }
        );
    }

    #[test]
    fn link_refactor_apply_and_yes() {
        let parsed = run(&[
            "link",
            "refactor",
            "--source",
            "notes",
            "--old",
            "OldDoc.md",
            "--new",
            "NewDoc.md",
            "--apply",
            "--yes",
        ])
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::LinkRefactor {
                source: "notes".to_string(),
                old_path: "OldDoc.md".to_string(),
                new_path: "NewDoc.md".to_string(),
                apply: true,
                yes: true,
            }
        );
    }

    #[test]
    fn link_refactor_missing_source_flag_errors() {
        let err = parse_args(["link", "refactor", "--old", "A.md", "--new", "B.md"]).unwrap_err();
        assert!(err.0.contains("requires --source"));
    }

    #[test]
    fn link_refactor_missing_old_flag_errors() {
        let err =
            parse_args(["link", "refactor", "--source", "notes", "--new", "B.md"]).unwrap_err();
        assert!(err.0.contains("requires --old"));
    }

    #[test]
    fn link_refactor_missing_new_flag_errors() {
        let err =
            parse_args(["link", "refactor", "--source", "notes", "--old", "A.md"]).unwrap_err();
        assert!(err.0.contains("requires --new"));
    }

    #[test]
    fn link_refactor_yes_without_apply_is_a_conflicting_flag_error() {
        let err = parse_args([
            "link", "refactor", "--source", "notes", "--old", "A.md", "--new", "B.md", "--yes",
        ])
        .unwrap_err();
        assert!(err
            .0
            .contains("--yes only makes sense together with --apply"));
    }

    #[test]
    fn unrecognized_top_level_command_errors() {
        let err = parse_args(["bogus"]).unwrap_err();
        assert!(err.0.contains("unrecognized command"));
    }

    #[test]
    fn unrecognized_trailing_flag_errors() {
        let err = parse_args(["source", "list", "--bogus"]).unwrap_err();
        assert!(err.0.contains("unrecognized argument"));
    }
}
