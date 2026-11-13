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
    tephractl ls <source> [folder] [-r] [--json]
    tephractl search <query> [--limit <n>] [--json]
    tephractl search --in <id> \"#<query>\" | \"^<query>\" [--limit <n>] [--json]
    tephractl search \"^^<query>\" [--limit <n>] [--json]
    tephractl block-id <unit-id> --line <n> [--json]
    tephractl link resolve [--source <name>] <target> [--json]
    tephractl link refactor --source <name> --old <path> --new <path> [--apply] [--yes] [--json]
    tephractl daemon install [--from <path>] [--json]
    tephractl daemon uninstall
    tephractl daemon status [--json]
    tephractl -h | --help

LS:
    ls <source> [folder]  List one folder of a source: kind, size, modified time and name.
                          <folder> is relative to the source root (default: the root).
                          Works for an offline source too, from the cache.
    -r, --recursive       Also list everything below <folder>, indented by depth

SEARCH:
    search <query>        Find files by name across every source (offline ones included):
                          fuzzy match on file name, title and path, best match first.
                          One row per result: id, kind, title. Quote a query with spaces.
    --limit <n>           Show at most <n> results (default 50)
                          Use `--` before a query that starts with '-'.
    search --in <id> \"#q\"  Find headings of one file (<id> is @source:path/to/file); a bare
                          \"#\" lists them all, in file order. A document has headings by
                          page, slide or paragraph.
    search --in <id> \"^q\"  Find blocks of one file: paragraphs of a note, symbols of a code
                          file, paragraphs of a document by page, slide or paragraph. A bare
                          \"^\" lists them all, in file order. One row per result: kind,
                          line (or page/slide) and text or name; a ^id shows after it.
                          A '#' or '^' query needs --in: pick a file first.
    search \"^^q\"        Find text in the blocks of every file of every source (offline ones
                          included): paragraphs, code comments, symbol names, headings and
                          document paragraphs; part of a word matches, best first. One row per
                          result: file:line, kind, the text around the match (a document hit
                          shows page/slide instead of :line); a ^id shows after it. Quote it:
                          ^ is special to some shells.

BLOCK-ID:
    block-id <unit-id> --line <n>
                          Give the paragraph or list item holding line <n> of a note a block
                          ID and print the link to it (<unit-id> is @source:path/to/file; lines
                          count from 1). A block that already has an ID is not changed: its
                          link is printed. A heading prints its #Heading link. This is the one
                          command that writes to your files: it appends \" ^id\" to the end of
                          the block's last line, and nothing else. md, markdown, txt, org, rst
                          and adoc files only.

DAEMON (macOS only; need no token and no running daemon):
    daemon install    Copy tephrad to ~/Library/Application Support/tephra/bin, sign it
                      with a local \"Tephra Local\" identity and load it as a launchd agent
                      (--from <path> installs that tephrad instead of the one next to tephractl)
    daemon uninstall  Unload the agent and remove the plist and the installed copy
    daemon status     Show whether the agent is installed and running

GLOBAL OPTIONS:
    --json          Print raw structured JSON instead of human-readable text
    --addr <addr>   Daemon address to connect to (default: 127.0.0.1:4812)
    -h, --help      Print this help and exit
";

/// Which in-file search a sigil asks for: `#` is `headings`, `^` is
/// `blocks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFileSearchScope {
    Headings,
    Blocks,
}

impl InFileSearchScope {
    /// The `scope` value `unit.search` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            InFileSearchScope::Headings => "headings",
            InFileSearchScope::Blocks => "blocks",
        }
    }
}

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
    /// `ls <source> [folder] [-r]`: `folder` is `""` for the source
    /// root.
    Ls {
        source: String,
        folder: String,
        recursive: bool,
    },
    /// `search <query> [--limit <n>]`: a name search across every
    /// source (`unit.search`, scope `names`). `limit` is `None` for the
    /// daemon's default of 50.
    Search {
        query: String,
        limit: Option<u64>,
    },
    /// `search --in <id> "#q"` or `"^q"`: a search inside one file
    /// (`unit.search`, scope `headings` or `blocks`, G9). `query` is what
    /// follows the sigil, which is empty for a bare `#` or `^` (list
    /// everything); the sigil itself is never sent.
    SearchIn {
        unit: String,
        scope: InFileSearchScope,
        query: String,
        limit: Option<u64>,
    },
    /// `search "^^q"`: a full-text search of the blocks of every file of
    /// every source (`unit.search`, scope `all_blocks`, G10). `query` is
    /// what follows the `^^`; the `^^` itself is never sent. The parser
    /// never builds one with a blank `query` (it refuses `^^` and `^^  `
    /// as an "empty query" before any request); a hand-built one still
    /// reaches the daemon, which refuses it too.
    SearchAllBlocks {
        query: String,
        limit: Option<u64>,
    },
    /// `block-id <unit-id> --line <n>`: `block.ensure_id`. `line` counts
    /// from 1; the parser refuses 0 and anything that is not a whole
    /// number before any request.
    BlockId {
        unit: String,
        line: u64,
    },
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
    /// `daemon install`. `keychain` is the hidden `--keychain <path>`
    /// the live test uses so it never touches the login keychain.
    DaemonInstall {
        from: Option<String>,
        keychain: Option<String>,
    },
    DaemonUninstall,
    DaemonStatus,
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
///
/// `--json`/`--addr` are accepted both as leading flags (before the
/// subcommand word, matching the `GLOBAL OPTIONS` section of
/// [`USAGE`]) and as trailing flags (after a subcommand's own
/// positional args, matching every existing fixture/test) — R7: the
/// documented usage showed them as global, but only the trailing
/// form used to parse; `tephractl --json source list` failed with
/// "unrecognized command: --json".
pub fn parse_args<I, S>(args: I) -> Result<Action, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let owned: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    if owned.is_empty() {
        return Ok(Action::Help);
    }

    let mut iter = owned.iter().map(String::as_str).peekable();

    // R7: consume any `--json`/`--addr <addr>` that appear *before*
    // the subcommand word, so the documented "GLOBAL OPTIONS" usage
    // (`tephractl --json source list`, `tephractl --addr <addr>
    // status`) actually parses.
    let mut leading_json = false;
    let mut leading_addr: Option<String> = None;
    loop {
        match iter.peek() {
            Some(&"--json") => {
                iter.next();
                leading_json = true;
            }
            Some(&"--addr") => {
                iter.next();
                leading_addr = Some(take_value(&mut iter, "--addr")?);
            }
            _ => break,
        }
    }

    let head = match iter.next() {
        Some(h) => h,
        None => return Ok(Action::Help),
    };

    if head == "-h" || head == "--help" {
        return Ok(Action::Help);
    }

    let (command, trailing_json, trailing_addr) = match head {
        "source" => parse_source(&mut iter)?,
        "status" => parse_status(&mut iter)?,
        "ls" => parse_ls(&mut iter)?,
        "search" => parse_search(&mut iter)?,
        "link" => parse_link(&mut iter)?,
        "block-id" => parse_block_id(&mut iter)?,
        "daemon" => parse_daemon(&mut iter)?,
        other => return Err(ArgsError(format!("unrecognized command: {other}"))),
    };

    Ok(Action::Run(ParsedArgs {
        command,
        json: leading_json || trailing_json,
        addr: trailing_addr.or(leading_addr),
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

/// `ls <source> [folder] [-r|--recursive] [--json] [--addr <addr>]`.
/// Flags may sit anywhere after `ls`, including between the two
/// positionals; the first positional is the source, the second the
/// folder. A folder that starts with `-` can't be given (it would
/// read as a flag), which no real source-relative path needs.
fn parse_ls<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut source: Option<String> = None;
    let mut folder: Option<String> = None;
    let mut recursive = false;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "-r" | "--recursive" => recursive = true,
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if source.is_none() => source = Some(other.to_string()),
            other if folder.is_none() => folder = Some(other.to_string()),
            other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
        }
    }

    let source = source.ok_or_else(|| ArgsError("ls requires <source> [folder]".to_string()))?;
    validate_source_name(&source)?;
    Ok((
        Command::Ls {
            source,
            folder: folder.unwrap_or_default(),
            recursive,
        },
        json,
        addr,
    ))
}

/// `search [--in <id>] <query> [--limit <n>] [--json] [--addr <addr>]`,
/// flags anywhere after `search`. Exactly one positional, the query: it
/// may be empty or blank (the daemon refuses that for a name search, and
/// its message is what the user sees). A query that starts with `-` can
/// follow a `--`.
///
/// Without `--in`, a query that starts with a single `#` or `^` is a
/// search inside one file, which needs the file picked first, and one
/// that starts with `^^` is a block search across every file (G10):
/// what follows the `^^` is sent as the `all_blocks` query, a bare or
/// blank `^^` is refused here as an "empty query" with no RPC round-trip,
/// and it is never sent as a names search. A `#` or `^` query is refused
/// here, with no RPC
/// round-trip (like the source-name check above), and never sent as a
/// names search.
///
/// With `--in <id>` the query must start with `#` (headings) or `^`
/// (blocks), and what follows the sigil is the query sent: a bare `#` or
/// `^` sends an empty one, which lists everything. A query without a
/// sigil is refused, never guessed at, and `^^` is refused too: it
/// searches every file, so it takes no `--in`.
fn parse_search<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut query: Option<String> = None;
    let mut limit: Option<u64> = None;
    let mut in_unit: Option<String> = None;
    let mut json = false;
    let mut addr = None;
    let mut only_positionals = false;

    while let Some(arg) = iter.next() {
        if only_positionals {
            if query.is_some() {
                return Err(too_many_search_words(arg));
            }
            query = Some(arg.to_string());
            continue;
        }
        match arg {
            "--" => only_positionals = true,
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            "--in" => {
                let unit = take_value(iter, "--in")?;
                match unit.strip_prefix('@').and_then(|rest| rest.split_once(':')) {
                    Some((source, path)) if !source.is_empty() && !path.is_empty() => {}
                    _ => {
                        return Err(ArgsError(format!(
                            "--in needs a unit id like \"@source:path/to/file\", got \"{unit}\""
                        )))
                    }
                }
                in_unit = Some(unit);
            }
            "--limit" => {
                let raw = take_value(iter, "--limit")?;
                limit = Some(match raw.parse::<u64>() {
                    Ok(n) if n >= 1 => n,
                    _ => {
                        return Err(ArgsError(format!(
                            "--limit must be a whole number of at least 1, got \"{raw}\""
                        )))
                    }
                });
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!(
                    "unrecognized argument: {other} (put -- before a query that starts with '-')"
                )))
            }
            other if query.is_none() => query = Some(other.to_string()),
            other => return Err(too_many_search_words(other)),
        }
    }

    let query = query.ok_or_else(|| ArgsError("search requires a <query>".to_string()))?;
    if let Some(rest) = query.strip_prefix("^^") {
        if in_unit.is_some() {
            return Err(ArgsError(
                "a query starting with '^^' searches blocks across every file, so it takes no \
                 --in; use '^' to search the blocks of one file"
                    .to_string(),
            ));
        }
        if rest.trim().is_empty() {
            return Err(ArgsError(
                "empty query: put the text to find after '^^', like \"^^conflict\"".to_string(),
            ));
        }
        return Ok((
            Command::SearchAllBlocks {
                query: rest.to_string(),
                limit,
            },
            json,
            addr,
        ));
    }
    if let Some(unit) = in_unit {
        let (scope, rest) = if let Some(rest) = query.strip_prefix('#') {
            (InFileSearchScope::Headings, rest)
        } else if let Some(rest) = query.strip_prefix('^') {
            (InFileSearchScope::Blocks, rest)
        } else {
            return Err(ArgsError(
                "with --in, start the query with '#' to search headings or '^' to search \
                 blocks (a bare '#' or '^' lists them all)"
                    .to_string(),
            ));
        };
        let query = rest.to_string();
        return Ok((
            Command::SearchIn {
                unit,
                scope,
                query,
                limit,
            },
            json,
            addr,
        ));
    }
    if query.starts_with('#') || query.starts_with('^') {
        return Err(ArgsError(format!(
            "pick a file first: a query starting with '{}' searches inside one file; add \
             --in <id> (an id like \"@source:path\") or drop the '{}' to search by name",
            &query[..1],
            &query[..1]
        )));
    }
    Ok((Command::Search { query, limit }, json, addr))
}

fn too_many_search_words(extra: &str) -> ArgsError {
    ArgsError(format!(
        "unrecognized argument: {extra} (quote a query that has spaces: tephractl search \"two words\")"
    ))
}

fn parse_daemon<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let sub = iter.next().ok_or_else(|| {
        ArgsError("expected a daemon subcommand: install, uninstall, or status".to_string())
    })?;

    match sub {
        "install" => {
            let mut from = None;
            let mut keychain = None;
            let mut json = false;
            while let Some(arg) = iter.next() {
                match arg {
                    "--from" => from = Some(take_value(iter, "--from")?),
                    "--keychain" => keychain = Some(take_value(iter, "--keychain")?),
                    "--json" => json = true,
                    other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
                }
            }
            Ok((Command::DaemonInstall { from, keychain }, json, None))
        }
        "uninstall" => {
            let (json, _) = parse_daemon_trailing(iter, false)?;
            Ok((Command::DaemonUninstall, json, None))
        }
        "status" => {
            let (json, _) = parse_daemon_trailing(iter, true)?;
            Ok((Command::DaemonStatus, json, None))
        }
        other => Err(ArgsError(format!(
            "unrecognized daemon subcommand: {other} (expected install, uninstall, or status)"
        ))),
    }
}

/// Trailing flags for `daemon uninstall|status`: `--json` only where
/// the verb has JSON output (`status`).
fn parse_daemon_trailing<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
    allow_json: bool,
) -> Result<(bool, ()), ArgsError> {
    let mut json = false;
    for arg in iter {
        match arg {
            "--json" if allow_json => json = true,
            other => return Err(ArgsError(format!("unrecognized argument: {other}"))),
        }
    }
    Ok((json, ()))
}

fn parse_block_id<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut unit: Option<String> = None;
    let mut line: Option<u64> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            "--line" => {
                let raw = take_value(iter, "--line")?;
                line = Some(match raw.parse::<u64>() {
                    Ok(n) if n >= 1 => n,
                    _ => {
                        return Err(ArgsError(format!(
                            "--line must be a whole number of at least 1, got \"{raw}\""
                        )))
                    }
                });
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if unit.is_none() => {
                match other
                    .strip_prefix('@')
                    .and_then(|rest| rest.split_once(':'))
                {
                    Some((source, path)) if !source.is_empty() && !path.is_empty() => {}
                    _ => {
                        return Err(ArgsError(format!(
                        "block-id needs a unit id like \"@source:path/to/file\", got \"{other}\""
                    )))
                    }
                }
                unit = Some(other.to_string());
            }
            other => {
                return Err(ArgsError(format!(
                    "block-id takes one unit id; unexpected extra argument: {other}"
                )))
            }
        }
    }

    let unit = unit.ok_or_else(|| {
        ArgsError("block-id requires a <unit-id> like \"@source:path/to/file\"".to_string())
    })?;
    let line = line.ok_or_else(|| ArgsError("block-id requires --line <n>".to_string()))?;
    Ok((Command::BlockId { unit, line }, json, addr))
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

    /// R7: the documented `USAGE` lists `--json`/`--addr` under
    /// "GLOBAL OPTIONS", shown *before* the subcommand — but only the
    /// trailing position used to parse. `tephractl --json source
    /// list` must work exactly like `tephractl source list --json`.
    #[test]
    fn leading_json_and_addr_flags_before_the_subcommand_are_accepted() {
        let parsed = run(&["--json", "source", "list"]).unwrap();
        assert_eq!(parsed.command, Command::SourceList);
        assert!(parsed.json);

        let parsed = run(&["--addr", "127.0.0.1:48199", "status"]).unwrap();
        assert_eq!(parsed.command, Command::Status);
        assert_eq!(parsed.addr, Some("127.0.0.1:48199".to_string()));
    }

    /// R7 (companion): leading and trailing global flags both work
    /// together, and can even both be given on the same invocation.
    #[test]
    fn leading_and_trailing_global_flags_can_combine() {
        let parsed = run(&[
            "--addr",
            "127.0.0.1:48199",
            "source",
            "add",
            "notes",
            "/home/dan/Notes",
            "--json",
        ])
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceAdd {
                name: "notes".to_string(),
                path: "/home/dan/Notes".to_string(),
            }
        );
        assert!(parsed.json);
        assert_eq!(parsed.addr, Some("127.0.0.1:48199".to_string()));
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

    fn ls(source: &str, folder: &str, recursive: bool) -> Command {
        Command::Ls {
            source: source.to_string(),
            folder: folder.to_string(),
            recursive,
        }
    }

    #[test]
    fn ls_parses_source_alone_with_the_root_as_folder() {
        let parsed = run(&["ls", "notes"]).unwrap();
        assert_eq!(parsed.command, ls("notes", "", false));
        assert!(!parsed.json);
        assert_eq!(parsed.addr, None);
    }

    #[test]
    fn ls_parses_source_and_folder() {
        let parsed = run(&["ls", "tephra", "crates/tephrad/src"]).unwrap();
        assert_eq!(parsed.command, ls("tephra", "crates/tephrad/src", false));
    }

    #[test]
    fn ls_recursive_flag_before_or_after_the_folder() {
        for args in [
            ["ls", "src", "folder", "-r"],
            ["ls", "src", "-r", "folder"],
            ["ls", "-r", "src", "folder"],
        ] {
            assert_eq!(
                run(&args).unwrap().command,
                ls("src", "folder", true),
                "{args:?}"
            );
        }
        assert_eq!(
            run(&["ls", "src", "--recursive"]).unwrap().command,
            ls("src", "", true)
        );
    }

    #[test]
    fn ls_accepts_an_explicit_empty_folder_as_the_root() {
        assert_eq!(
            run(&["ls", "src", ""]).unwrap().command,
            ls("src", "", false)
        );
    }

    #[test]
    fn ls_json_and_addr_flags_work_before_or_after_the_command() {
        let leading = run(&["--json", "--addr", "127.0.0.1:1", "ls", "src", "-r"]).unwrap();
        assert!(leading.json);
        assert_eq!(leading.addr.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(leading.command, ls("src", "", true));

        let trailing = run(&["ls", "src", "docs", "--json", "--addr", "127.0.0.1:2"]).unwrap();
        assert!(trailing.json);
        assert_eq!(trailing.addr.as_deref(), Some("127.0.0.1:2"));
        assert_eq!(trailing.command, ls("src", "docs", false));

        let between = run(&["ls", "src", "--addr", "127.0.0.1:3", "docs", "-r", "--json"]).unwrap();
        assert_eq!(between.addr.as_deref(), Some("127.0.0.1:3"));
        assert_eq!(between.command, ls("src", "docs", true));
    }

    #[test]
    fn ls_without_a_source_is_an_args_error() {
        let err = run(&["ls"]).unwrap_err();
        assert!(err.0.contains("ls requires <source>"), "{err}");
        assert!(run(&["ls", "-r"]).is_err());
        assert!(run(&["ls", "--json"]).is_err());
    }

    #[test]
    fn ls_rejects_a_source_name_with_a_space_slash_or_at_sign() {
        for bad in ["my notes", "a/b", "@notes", "no\ttab"] {
            let err = run(&["ls", bad]).unwrap_err();
            assert!(err.0.contains("invalid source name"), "{bad}: {err}");
        }
    }

    #[test]
    fn ls_rejects_extra_positionals_and_unknown_flags() {
        assert!(run(&["ls", "src", "a", "b"]).is_err());
        assert!(run(&["ls", "src", "--all"]).is_err());
        assert!(run(&["ls", "src", "-x"]).is_err());
        assert!(run(&["ls", "src", "--addr"]).is_err());
    }

    fn search(query: &str, limit: Option<u64>) -> Command {
        Command::Search {
            query: query.to_string(),
            limit,
        }
    }

    #[test]
    fn search_parses_one_quoted_query() {
        let parsed = run(&["search", "atlas outside"]).unwrap();
        assert_eq!(parsed.command, search("atlas outside", None));
        assert!(!parsed.json);
        assert_eq!(parsed.addr, None);
    }

    #[test]
    fn search_json_and_addr_work_leading_and_trailing_and_between() {
        let leading = run(&["--json", "--addr", "127.0.0.1:1", "search", "atlas"]).unwrap();
        assert!(leading.json);
        assert_eq!(leading.addr.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(leading.command, search("atlas", None));

        let trailing = run(&["search", "atlas", "--json", "--addr", "127.0.0.1:2"]).unwrap();
        assert!(trailing.json);
        assert_eq!(trailing.addr.as_deref(), Some("127.0.0.1:2"));
        assert_eq!(trailing.command, search("atlas", None));

        let between = run(&["search", "--addr", "127.0.0.1:3", "atlas", "--json"]).unwrap();
        assert!(between.json);
        assert_eq!(between.addr.as_deref(), Some("127.0.0.1:3"));
    }

    #[test]
    fn search_limit_must_be_a_positive_whole_number() {
        assert_eq!(
            run(&["search", "a", "--limit", "7"]).unwrap().command,
            search("a", Some(7))
        );
        assert_eq!(
            run(&["search", "--limit", "1", "a"]).unwrap().command,
            search("a", Some(1))
        );
        for bad in ["0", "-1", "1.5", "ten", ""] {
            let err = run(&["search", "a", "--limit", bad]).unwrap_err();
            assert!(err.0.contains("--limit"), "{bad}: {err}");
        }
        assert!(run(&["search", "a", "--limit"]).is_err());
    }

    #[test]
    fn search_passes_an_empty_or_blank_query_on_for_the_daemon_to_refuse() {
        assert_eq!(run(&["search", ""]).unwrap().command, search("", None));
        assert_eq!(run(&["search", "  "]).unwrap().command, search("  ", None));
    }

    #[test]
    fn search_needs_exactly_one_query() {
        let err = run(&["search"]).unwrap_err();
        assert!(err.0.contains("search requires a <query>"), "{err}");
        assert!(run(&["search", "--json"]).is_err());
        let err = run(&["search", "atlas", "outside"]).unwrap_err();
        assert!(err.0.contains("quote"), "{err}");
        assert!(run(&["search", "--unknown", "a"]).is_err());
    }

    #[test]
    fn search_keeps_every_character_of_the_query() {
        for query in [
            "café",
            "CAFÉ",
            "100%_done",
            "a, b",
            "@notes:x",
            "dir/file",
            "[[Link]]",
            ".*(",
            "it's \"q\"",
            "-dash",
        ] {
            let args: Vec<&str> = if query.starts_with('-') {
                vec!["search", "--", query]
            } else {
                vec!["search", query]
            };
            assert_eq!(run(&args).unwrap().command, search(query, None), "{query}");
        }
        // After `--` flags are text.
        assert_eq!(
            run(&["search", "--", "--json"]).unwrap().command,
            search("--json", None)
        );
    }

    /// A query starting with a single `#` or `^` is a search inside one
    /// file: refused here ("pick a file first"), before any RPC.
    #[test]
    fn search_refuses_a_hash_or_caret_query_with_pick_a_file_first() {
        for query in ["#x", "#", "^abc", "^", "#heading text", "##x"] {
            let err = run(&["search", query]).unwrap_err();
            assert!(err.0.contains("pick a file first"), "{query}: {err}");
        }
        // Flags don't change that.
        let err = run(&["search", "#x", "--json", "--addr", "127.0.0.1:1"]).unwrap_err();
        assert!(err.0.contains("pick a file first"), "{err}");
        let err = run(&["--json", "search", "^x"]).unwrap_err();
        assert!(err.0.contains("pick a file first"), "{err}");
    }

    /// `^^` (G10) is a block search across every file: what follows it
    /// is the query, and it is never sent as a names search.
    #[test]
    fn search_with_a_double_caret_is_an_all_blocks_search_with_the_sigil_stripped() {
        for (arg, want) in [("^^abc", "abc"), ("^^ a b", " a b"), ("^^^x", "^x")] {
            let parsed = run(&["search", arg]).unwrap();
            assert_eq!(
                parsed.command,
                Command::SearchAllBlocks {
                    query: want.to_string(),
                    limit: None
                },
                "{arg}"
            );
        }
        let parsed = run(&["--json", "search", "^^x", "--limit", "3"]).unwrap();
        assert!(parsed.json);
        assert_eq!(
            parsed.command,
            Command::SearchAllBlocks {
                query: "x".to_string(),
                limit: Some(3)
            }
        );
    }

    /// A bare or blank `^^` is refused while parsing, so no request is
    /// ever built for it.
    #[test]
    fn search_with_a_blank_double_caret_query_is_refused_as_empty() {
        for arg in ["^^", "^^ ", "^^   ", "^^\t"] {
            let err = run(&["search", arg]).unwrap_err();
            assert!(err.0.contains("empty query"), "{arg:?}: {err}");
        }
        let err = run(&["--json", "search", "^^", "--limit", "3"]).unwrap_err();
        assert!(err.0.contains("empty query"), "{err}");
    }

    /// Only a *leading* `#`/`^` is special; elsewhere they are text.
    #[test]
    fn search_allows_hash_and_caret_inside_a_query() {
        for query in ["c#", "a^b", "x #y", "  #x"] {
            assert_eq!(
                run(&["search", query]).unwrap().command,
                search(query, None),
                "{query:?}"
            );
        }
    }
}
