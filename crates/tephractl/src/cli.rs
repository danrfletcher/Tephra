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

use tephra_remote::credentials::Provider;

use crate::since::Since;

pub const USAGE: &str = "\
tephractl - command-line client for the tephrad daemon

USAGE:
    tephractl source add <name> <path> [--json]
    tephractl source add github <name> <owner/repo> [--branch <b>] [--json]
    tephractl source add drive <name> <folder> [--json]
    tephractl source promote <name> <path> [--json]
    tephractl source unpromote <name> [--json]
    tephractl source rename <old> <new> [--json]
    tephractl source remove <name> [--json]
    tephractl source list [--json]
    tephractl status [--json]
    tephractl ls <source> [folder] [-r] [--json]
    tephractl search <query> [--limit <n>] [--json]
    tephractl search --in <id> \"#<query>\" | \"^<query>\" [--limit <n>] [--json]
    tephractl search \"^^<query>\" [--limit <n>] [--json]
    tephractl block-id <unit-id> --line <n> | --symbol <name> [--json]
    tephractl backlinks <id> [--json]
    tephractl fetch <id> [--json]
    tephractl pin <id> [--yes] [--json]
    tephractl unpin <id> [--json]
    tephractl resolve <id> --mine | --theirs [--json]
    tephractl cache status [--json]
    tephractl cache set-limit <limit> [--json]
    tephractl cache on <source> [--json]
    tephractl cache off <source> [--json]
    tephractl fixes [--since <30m|2h|7d|date>] [--json]
    tephractl link resolve [--source <name>] <target> [--json]
    tephractl link refactor --source <name> --old <path> --new <path> [--apply] [--yes] [--json]
    tephractl daemon install [--from <path>] [--json]
    tephractl daemon uninstall
    tephractl daemon status [--json]
    tephractl auth github [-]
    tephractl auth google
    tephractl auth status [--json]
    tephractl auth logout github|google
    tephractl -h | --help

GITHUB SOURCES:
    source add github <name> <owner/repo> [--branch <b>]
                          Register ONE GitHub repository on ONE branch (default: the repo's
                          default branch). Needs `tephractl auth github` first. The whole file
                          list shows within seconds, every file `remote` with 0 bytes cached;
                          nothing is written to disk. Text files are then read once through
                          the API in the background (`source list` shows indexing done/total,
                          then active) for links, headings, blocks and symbols, and the bytes
                          are dropped. Only owner/repo is accepted: an owner alone, owner/*,
                          a URL or a path inside the repo is refused (pick one repo).
                          A local source literally named github: `source add github <path>`
                          (ONE argument after `github`) adds the folder <path> as the source
                          named github; TWO arguments after `github` are always the GitHub
                          form.

DRIVE SOURCES:
    source add drive <name> <folder>
                          Register ONE Google Drive folder from My Drive. <folder> is its path
                          from My Drive (Notes/2026), its id, or its drive.google.com URL;
                          quote it when it has spaces. Needs `tephractl auth google` first.
                          The listing shows within seconds, every file `remote` with 0 bytes
                          cached; nothing is written to disk. Text files and Google Docs are
                          then read once in the background (`source list` shows indexing
                          done/total, then active) and the bytes are dropped; Sheets, Slides,
                          Forms, Drawings, shortcuts, binaries and files over 20 MB are listed
                          by name only. A folder in a Shared Drive, or all of Drive, is refused.
                          Two files with one name: the later one is `name (2)`.
                          A local source literally named drive: `source add drive <path>`
                          (ONE argument after `drive`); TWO arguments are always the Drive form.

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

BACKLINKS:
    backlinks <id>        List the links to one file or one block (graph.get_backlinks). <id> is
                          @source:path/to/file for a file, or @source:path/to/file#^blockid for a
                          block. One row per link: the file that holds it and its line,
                          file:line, then the link text. A link to a block that now sits in
                          another file (it moved) still counts, once it finds the block there.
                          An id nothing links to prints \"no backlinks\" and exits 0.

FETCH:
    fetch <id>            Download ONE file of a remote (GitHub) source into Tephra's own cache
                          (unit.fetch) and print where it is: <data_dir>/remote/<source>/clone/
                          <path>, and nothing else, so `cat \"$(tephractl fetch @g:README.md)\"`
                          works. <id> is @source:path/to/file. A file already fetched is not
                          downloaded again. Nothing is downloaded unless you ask: this is the
                          only command that does it. A file with edits of yours waiting to be
                          pushed is printed as it is, never overwritten. Exits 1 with the reason
                          on stderr when there is no file: the source is offline or needs
                          `tephractl auth github`, the download failed (the file stays
                          remote and `ls` says why), the disk is full, or the item is never
                          fetched (Google Docs, Sheets, Slides, Forms and Drawings, shortcuts,
                          symlinks, submodules and Git LFS pointers: the message says where to
                          open it).

PIN:
    pin <id>              Keep a remote file or folder downloaded (unit.pin). <id> is
                          @source:path/to/file or @source:path/to/folder/ (a Drive folder with a
                          space in it needs quotes). It is stored as a path prefix, so a folder
                          pin covers the files in it now and the ones added later. Prints how
                          many files and how many bytes (1000-based) will be downloaded, and
                          which kinds are skipped because they are never downloaded (Google
                          Docs, Sheets, Slides and Forms, symlinks, submodules, Git LFS
                          pointers). The files come down in the background as the pin is
                          stored, then are never dropped by the cache: `ls` shows them as
                          downloading, then pinned. A pin larger than a fifth of the cache
                          limit asks first on a terminal (`[y/N]`, on stderr); --yes skips the
                          question. Without a terminal and without --yes a pin that needs the
                          question exits 1 and pins nothing. A pin over the limit itself only
                          warns. Pinning what is already pinned exits 0, changes nothing and
                          says so. Pinning a Google Doc or Sheet exits 1 with where to open
                          it. Nothing is written to the remote.
    unpin <id>            Remove a pin. The files stay cached but may be dropped again when the
                          cache needs room, and stay listed. A file under a pinned folder stays
                          pinned while the folder is. Unpinning what is not pinned exits 0,
                          changes nothing and says so.

RESOLVE:
    resolve <id> --mine|--theirs
                          Settle a Google Drive file that you edited and that was also changed on
                          Drive (`ls` shows it as needs_attention: changed on Drive). Nothing was
                          sent to Drive for it, and your edits are still on disk. <id> is
                          @source:path/to/file (the @ may be left off: gdrive:Plan.md).
                          --mine    your version is sent to Drive as a new revision on top of what
                                    Drive has now; the other change stays in Drive's history.
                          --theirs  your edits are kept beside the file as <name>.mine (a number
                                    is added if that name is taken) and the file is refreshed from
                                    Drive.
                          Exactly one of the two is required. An id that is not a file of a Drive
                          source, or a file with no change on Drive to settle, exits 1 with the
                          reason on stderr and nothing is changed.

CACHE:
    cache status          What Tephra's content cache of remote files takes, against its limit:
                          bytes cached, the limit, `over limit` while it is true, the bytes
                          pinned files take, and the `.git` bytes on a line of their own (not
                          counted toward the limit, never dropped). A warning says so when pinned
                          files alone are over the limit. One line per source with its cache flag.
    cache set-limit <l>   Change the limit now, save it to config.toml and run an eviction pass.
                          <l> is a number and a unit: MB, GB (1000-based) or MiB, GiB, e.g.
                          `10GB` or `500 MiB`; `0` is allowed and drops everything that may go.
                          A value that does not parse is refused with the reason and the old
                          limit is kept. Over the limit, the least recently used files go back to
                          `remote` (still listed, links intact) once unused for 10 minutes;
                          pinned files, edits not yet sent and files in use are never dropped.
    cache on <source>     Keep fetched files of a remote source until the limit needs the room
                          (the default).
    cache off <source>    Drop a fetched file of a remote source about 10 minutes after its last
                          use, even under the limit. Pinned files stay. The flag is kept across
                          restarts. Nothing is ever sent to the remote: a dropped file is only
                          the local copy.

FIXES:
    fixes                 The log of automatic link fixes (fix.list), newest first, one line per
                          fix: time kind old -> new (n files) status. status is applied, partial,
                          needs_manual_fix or uncertain. An uncertain line has no new name: the
                          file vanished with no certain pair and its links are broken. Shows the
                          newest 50. It only reads: nothing is undone or replayed.
    --since <when>        Only fixes from <when> on: 30m, 2h or 7d ago, or a date such as
                          2026-11-16 or 2026-11-16T09:30 (your local time; add Z or +hh:mm for
                          another zone).

LINK RESOLVE:
    link resolve <target> A block link [[@source:path#^blockid]] prints the block: its id, its
                          line, and \"moved\" when it was found in another file of the source.
                          An ID found in two or more files, or twice in one file, is an error
                          that lists every place. A heading or a code symbol name after the '#'
                          resolves to the file, with no block.

BLOCK-ID:
    block-id <unit-id> --line <n>
    block-id <unit-id> --symbol <name>
                          Give a block of a file an ID and print the link to it (<unit-id> is
                          @source:path/to/file). In a note, --line <n> (lines count from 1)
                          picks the paragraph or list item holding that line. In a code file,
                          --line <n> picks the symbol whose range holds the line (its
                          attribute, decorator or doc-comment lines too) and --symbol <name>
                          picks it by name (Parent.name when several share a name). Give one
                          of the two, not both. A block that already has an ID is not changed:
                          its link is printed. A heading prints its #Heading link. This is the
                          one command that writes to your files: in a note it appends \" ^id\"
                          to the end of the block's last line; in a code file it writes
                          \"// ^id\", \"# ^id\" or \"-- ^id\" at the end of the symbol's
                          definition line (or \" ^id\" at the end of the line comment already
                          there), and nothing else. It refuses a line that ends inside a
                          string or a block comment. md, markdown, txt, org, rst and adoc
                          files and code files only.

DAEMON (macOS only; need no token and no running daemon):
    daemon install    Copy tephrad to ~/Library/Application Support/tephra/bin, sign it
                      with a local \"Tephra Local\" identity and load it as a launchd agent
                      (--from <path> installs that tephrad instead of the one next to tephractl)
    daemon uninstall  Unload the agent and remove the plist and the installed copy
    daemon status     Show whether the agent is installed and running

AUTH (need no token and no running daemon; credentials go to the macOS Keychain only):
    auth github [-]       Sign in to GitHub. Reads the token with echo off, or one line from
                          stdin with `-` (so it can be piped in). A token is never taken as an
                          argument. Checks it with GET /user and stores it only if GitHub
                          accepts it. Needs a fine-grained read-only token.
    auth google           Sign in to Google Drive in your browser (loopback redirect, PKCE).
                          Needs google_client_json in config.toml: your own Desktop-app OAuth
                          client, published In production (in Testing mode Google expires
                          refresh tokens after 7 days). Waits up to 5 minutes.
    auth status [--json]  One line per provider: signed in or auth_required. Reads the
                          credential store only; makes no network call and shows no token.
    auth logout <provider>  Delete that provider's stored credential (github or google). It
                          is local only: nothing is revoked at GitHub or Google.

GLOBAL OPTIONS:
    --json          Print raw structured JSON instead of human-readable text
    --addr <addr>   Daemon address to connect to (default: 127.0.0.1:4812)
    -h, --help      Print this help and exit

EXIT CODES:
    0 done, 1 the command ran and failed, 2 a usage error (nothing was sent)
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

/// What `block-id` points at: a line of the file, or a symbol of a code
/// file by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTarget {
    Line(u64),
    Symbol(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SourceAdd {
        name: String,
        path: String,
    },
    /// `source add github <name> <owner/repo> [--branch <b>]`: one repo, one
    /// branch (`None`: the repo's default branch).
    SourceAddGithub {
        name: String,
        repo: String,
        branch: Option<String>,
    },
    /// `source add drive <name> <folder>`: one Drive folder (a path from My
    /// Drive, an id or a URL).
    SourceAddDrive {
        name: String,
        folder: String,
    },
    SourceRemove {
        name: String,
    },
    /// `source promote <name> <path>`: a folder of a registered source
    /// becomes the source `name`.
    SourcePromote {
        name: String,
        path: String,
    },
    /// `source unpromote <name>`: the promoted source `name` folds back
    /// into the source it was promoted out of.
    SourceUnpromote {
        name: String,
    },
    /// `source rename <old> <new>`: the source `old` is called `new` from
    /// now on and every `@old:` link is rewritten to `@new:`.
    SourceRename {
        old: String,
        new: String,
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
    /// `block-id <unit-id> --line <n> | --symbol <name>`:
    /// `block.ensure_id`. A `line` counts from 1; the parser refuses 0 and
    /// anything that is not a whole number before any request, and
    /// refuses both flags, or neither.
    BlockId {
        unit: String,
        target: BlockTarget,
    },
    /// `backlinks <id>`: `graph.get_backlinks`. `id` is a unit id
    /// (`@source:path`) or a block id (`@source:path#^blockid`); the
    /// parser refuses anything else before any request.
    Backlinks {
        id: String,
    },
    /// `fetch <id>`: `unit.fetch`. `unit` is a unit id, `@source:path`.
    Fetch {
        unit: String,
    },
    /// `pin <id> [--yes]`: `unit.pin`. `unit` is a unit id, `@source:path`.
    /// `yes` skips the confirmation a large pin asks for.
    Pin {
        unit: String,
        yes: bool,
    },
    /// `unpin <id>`: `unit.unpin`.
    Unpin {
        unit: String,
    },
    /// `resolve <id> --mine|--theirs`: `resolve`. `unit` is a unit id,
    /// `@source:path`; `keep` is `mine` or `theirs`.
    Resolve {
        unit: String,
        keep: String,
    },
    /// `cache status`: `cache.status`.
    CacheStatus,
    /// `cache set-limit <limit>`: `cache.set_limit`. `limit` goes to the
    /// daemon as typed (it owns the size rules), words joined by a space.
    CacheSetLimit {
        limit: String,
    },
    /// `cache on|off <source>`: `source.set_cache`.
    CacheSet {
        source: String,
        on: bool,
    },
    /// `fixes [--since <when>]`: `fix.list`. Read-only (F9).
    Fixes {
        since: Option<Since>,
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
    /// `auth github [-]`. `stdin` is the `-`: read the token from one
    /// line of stdin instead of the hidden prompt.
    AuthGithub {
        stdin: bool,
    },
    /// `auth google`.
    AuthGoogle,
    /// `auth status`.
    AuthStatus,
    /// `auth logout <provider>`.
    AuthLogout {
        provider: Provider,
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
/// containing invalid characters (spaces, `/`, `@`, `:`, `*`) is rejected
/// client-side"). Whitespace is checked separately via
/// `char::is_whitespace` so it covers more than just the literal
/// space character. `:` and `*` join the set with `source rename` (G13):
/// the daemon refuses them in a new name too.
const SOURCE_NAME_FORBIDDEN_CHARS: [char; 4] = ['/', '@', ':', '*'];

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
            "invalid source name \"{name}\": must not contain spaces, '/', '@', ':' or '*'"
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
        "backlinks" => parse_backlinks(&mut iter)?,
        "fetch" => parse_fetch(&mut iter)?,
        "pin" => parse_pin(&mut iter, true)?,
        "unpin" => parse_pin(&mut iter, false)?,
        "resolve" => parse_resolve(&mut iter)?,
        "cache" => parse_cache(&mut iter)?,
        "fixes" => parse_fixes(&mut iter)?,
        "daemon" => parse_daemon(&mut iter)?,
        "auth" => {
            let (command, json) = parse_auth(&mut iter)?;
            // `--json` and `--addr` before the word: only `auth status`
            // has JSON output, and no `auth` command talks to the daemon.
            if (leading_json && !matches!(command, Command::AuthStatus)) || leading_addr.is_some() {
                return Err(ArgsError(
                    "auth commands take no --addr, and --json only on auth status".to_string(),
                ));
            }
            (command, json, None)
        }
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
        ArgsError(
            "expected a source subcommand: add, promote, unpromote, rename, remove, or list"
                .to_string(),
        )
    })?;

    match sub {
        "add" => {
            let name = iter
                .next()
                .ok_or_else(|| ArgsError("source add requires <name> <path>".to_string()))?
                .to_string();
            if name == "github" {
                return parse_source_add_github(iter);
            }
            if name == "drive" {
                return parse_source_add_drive(iter);
            }
            validate_source_name(&name)?;
            let path = iter
                .next()
                .ok_or_else(|| ArgsError("source add requires <name> <path>".to_string()))?
                .to_string();
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceAdd { name, path }, json, addr))
        }
        "promote" => {
            let name = iter
                .next()
                .ok_or_else(|| ArgsError("source promote requires <name> <path>".to_string()))?
                .to_string();
            validate_source_name(&name)?;
            let path = iter
                .next()
                .ok_or_else(|| ArgsError("source promote requires <name> <path>".to_string()))?
                .to_string();
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourcePromote { name, path }, json, addr))
        }
        "unpromote" => {
            let name = iter
                .next()
                .ok_or_else(|| ArgsError("source unpromote requires <name>".to_string()))?
                .to_string();
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceUnpromote { name }, json, addr))
        }
        "rename" => {
            let old = iter
                .next()
                .ok_or_else(|| ArgsError("source rename requires <old> <new>".to_string()))?
                .to_string();
            // The name rules apply to the new name only: an existing name is
            // never re-checked (FR14), so `<old>` goes to the daemon as typed.
            let new = iter
                .next()
                .ok_or_else(|| ArgsError("source rename requires <old> <new>".to_string()))?
                .to_string();
            validate_source_name(&new)?;
            let (json, addr) = parse_trailing_flags(iter)?;
            Ok((Command::SourceRename { old, new }, json, addr))
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
            "unrecognized source subcommand: {other} (expected add, promote, unpromote, rename, remove, or list)"
        ))),
    }
}

/// What follows `source add github`: the GitHub form (`<name> <owner/repo>
/// [--branch <b>]`) when two positional arguments come before any flag, and
/// otherwise a local source literally named `github` (`source add github
/// <path>`, one positional). The rule looks at the argument list alone, so
/// it needs no daemon: a local `<path>` is never taken for a repo, and a
/// repo is never taken for a path.
fn parse_source_add_github<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let rest: Vec<&str> = iter.collect();
    let positionals = rest.iter().take_while(|t| !t.starts_with("--")).count();
    match positionals {
        0 => Err(ArgsError(
            "source add requires <name> <path> (or `source add github <name> <owner/repo>`)"
                .to_string(),
        )),
        1 => {
            // A local source named "github".
            let mut tail = rest[1..].iter().copied();
            let (json, addr) = parse_trailing_flags(&mut tail)?;
            Ok((
                Command::SourceAdd {
                    name: "github".to_string(),
                    path: rest[0].to_string(),
                },
                json,
                addr,
            ))
        }
        2 => {
            let name = rest[0].to_string();
            validate_source_name(&name)?;
            validate_repo_spec(rest[1])?;
            let repo = rest[1].to_string();
            let mut json = false;
            let mut addr = None;
            let mut branch = None;
            let mut tail = rest[2..].iter().copied();
            while let Some(arg) = tail.next() {
                match arg {
                    "--json" => json = true,
                    "--addr" => addr = Some(take_value(&mut tail, "--addr")?),
                    "--branch" => {
                        if branch.is_some() {
                            return Err(ArgsError(
                                "--branch given twice: a GitHub source has one branch".to_string(),
                            ));
                        }
                        let b = take_value(&mut tail, "--branch")?;
                        validate_branch(&b)?;
                        branch = Some(b);
                    }
                    other => {
                        return Err(ArgsError(format!("unrecognized argument: {other}")));
                    }
                }
            }
            Ok((Command::SourceAddGithub { name, repo, branch }, json, addr))
        }
        _ => Err(ArgsError(
            "source add github takes <name> <owner/repo>: pick one repo".to_string(),
        )),
    }
}

/// What follows `source add drive`: the Drive form (`<name> <folder>`) when
/// two positional arguments come before any flag, otherwise a local source
/// literally named `drive` (`source add drive <path>`). Like the GitHub
/// form, the argument list alone decides: no daemon is asked.
fn parse_source_add_drive<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let rest: Vec<&str> = iter.collect();
    let positionals = rest.iter().take_while(|t| !t.starts_with("--")).count();
    match positionals {
        0 => Err(ArgsError(
            "source add requires <name> <path> (or `source add drive <name> <folder>`)".to_string(),
        )),
        1 => {
            let mut tail = rest[1..].iter().copied();
            let (json, addr) = parse_trailing_flags(&mut tail)?;
            Ok((
                Command::SourceAdd {
                    name: "drive".to_string(),
                    path: rest[0].to_string(),
                },
                json,
                addr,
            ))
        }
        2 => {
            let name = rest[0].to_string();
            validate_source_name(&name)?;
            let folder = rest[1].to_string();
            if folder.trim().is_empty() || folder.chars().any(char::is_control) {
                return Err(ArgsError(
                    "source add drive needs the folder as a path from My Drive, an id or a URL"
                        .to_string(),
                ));
            }
            let mut tail = rest[2..].iter().copied();
            let (json, addr) = parse_trailing_flags(&mut tail)?;
            Ok((Command::SourceAddDrive { name, folder }, json, addr))
        }
        _ => Err(ArgsError(
            "source add drive takes <name> <folder>: quote a folder that has spaces".to_string(),
        )),
    }
}

/// `text` with control characters shown as `?`, for an error message.
fn shown(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// F4: a GitHub source is exactly one `owner/repo`. An owner alone, a
/// wildcard, a URL, a path inside the repo and anything else that could name
/// more than one repository is refused here, before the daemon (and GitHub)
/// hear of it. Mirrors the daemon's own check.
fn validate_repo_spec(repo: &str) -> Result<(), ArgsError> {
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let mut parts = repo.split('/');
    let (owner, name, extra) = (parts.next(), parts.next(), parts.next());
    match (owner, name, extra) {
        (Some(o), Some(n), None) if part_ok(o) && part_ok(n) => Ok(()),
        _ => Err(ArgsError(format!(
            "\"{}\" is not one repository: give owner/repo (pick one repo; a whole account or a \
             list of repos cannot be added)",
            shown(repo)
        ))),
    }
}

fn validate_branch(branch: &str) -> Result<(), ArgsError> {
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(char::is_control) {
        return Err(ArgsError(format!(
            "invalid branch name \"{}\"",
            shown(branch)
        )));
    }
    Ok(())
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

/// `auth ...`. The messages here never repeat an argument: a token
/// typed after `auth github` by mistake must not be echoed back into the
/// terminal scrollback, a log or a CI transcript.
fn parse_auth<'a>(iter: &mut impl Iterator<Item = &'a str>) -> Result<(Command, bool), ArgsError> {
    let sub = iter.next().ok_or_else(|| {
        ArgsError("expected an auth subcommand: github, google, status, or logout".to_string())
    })?;
    match sub {
        "github" => match (iter.next(), iter.next()) {
            (None, _) => Ok((Command::AuthGithub { stdin: false }, false)),
            (Some("-"), None) => Ok((Command::AuthGithub { stdin: true }, false)),
            _ => Err(ArgsError(
                "auth github takes no token argument (a token on the command line would \
                 reach your shell history and the process list); run `auth github` and \
                 type it at the prompt, or pipe it in with `auth github -`"
                    .to_string(),
            )),
        },
        "google" => match iter.next() {
            None => Ok((Command::AuthGoogle, false)),
            Some(_) => Err(ArgsError("auth google takes no arguments".to_string())),
        },
        "status" => match (iter.next(), iter.next()) {
            (None, _) => Ok((Command::AuthStatus, false)),
            (Some("--json"), None) => Ok((Command::AuthStatus, true)),
            _ => Err(ArgsError(
                "auth status takes no arguments except --json".to_string(),
            )),
        },
        "logout" => {
            let provider = match iter.next() {
                Some("github") => Provider::Github,
                Some("google") => Provider::Google,
                _ => {
                    return Err(ArgsError(
                        "auth logout needs one provider: github or google".to_string(),
                    ))
                }
            };
            match iter.next() {
                None => Ok((Command::AuthLogout { provider }, false)),
                Some(_) => Err(ArgsError(
                    "auth logout takes exactly one provider: github or google".to_string(),
                )),
            }
        }
        _ => Err(ArgsError(
            "unrecognized auth subcommand (expected github, google, status, or logout)".to_string(),
        )),
    }
}

fn parse_block_id<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut unit: Option<String> = None;
    let mut line: Option<u64> = None;
    let mut symbol: Option<String> = None;
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
            "--symbol" => {
                let raw = take_value(iter, "--symbol")?;
                if raw.trim().is_empty() {
                    return Err(ArgsError("--symbol needs a symbol name".to_string()));
                }
                symbol = Some(raw);
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
    let target = match (line, symbol) {
        (Some(line), None) => BlockTarget::Line(line),
        (None, Some(symbol)) => BlockTarget::Symbol(symbol),
        (Some(_), Some(_)) => {
            return Err(ArgsError(
                "block-id takes --line <n> or --symbol <name>, not both".to_string(),
            ))
        }
        (None, None) => {
            return Err(ArgsError(
                "block-id requires --line <n> or --symbol <name>".to_string(),
            ))
        }
    };
    Ok((Command::BlockId { unit, target }, json, addr))
}

/// Checks `id` is a unit id (`@source:path`) or a block id
/// (`@source:path#^blockid`, the ID being letters, digits and hyphens).
/// Anything else is refused with a message that says what is expected,
/// so a typo is an error and not an empty list. A '#' elsewhere in the
/// path is part of the file name (`@s:C# notes.md` is a unit), exactly
/// as the daemon reads it; only a `#^` suffix is taken as a block.
fn validate_backlinks_id(id: &str) -> Result<(), ArgsError> {
    let bad = |why: &str| {
        ArgsError(format!(
            "backlinks needs an id like \"@source:path/to/file\" or \
             \"@source:path/to/file#^blockid\", got \"{id}\": {why}"
        ))
    };
    let Some((source, rest)) = id.strip_prefix('@').and_then(|r| r.split_once(':')) else {
        return Err(bad("it must start with @source:"));
    };
    if source.is_empty() {
        return Err(bad("the source name is empty"));
    }
    let (path, block) = match rest.rsplit_once("#^") {
        Some((path, block)) => (path, Some(block)),
        None => (rest, None),
    };
    if path.is_empty() {
        return Err(bad("the path is empty"));
    }
    if let Some(block) = block {
        if block.is_empty() || !block.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(bad(
                "a block id is letters, digits and hyphens after \"#^\"",
            ));
        }
    }
    Ok(())
}

fn parse_backlinks<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut id: Option<String> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if id.is_none() => id = Some(other.to_string()),
            other => {
                return Err(ArgsError(format!(
                    "backlinks takes one id; unexpected extra argument: {other}"
                )))
            }
        }
    }

    let id = id.ok_or_else(|| {
        ArgsError("backlinks requires an <id> like \"@source:path/to/file\"".to_string())
    })?;
    validate_backlinks_id(&id)?;
    Ok((Command::Backlinks { id }, json, addr))
}

/// `fetch <id> [--json] [--addr <addr>]`. A bad id is a usage error, decided
/// here, before any request.
fn parse_fetch<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut unit: Option<String> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if unit.is_none() => unit = Some(other.to_string()),
            other => {
                return Err(ArgsError(format!(
                    "fetch takes one id; unexpected extra argument: {other}"
                )))
            }
        }
    }

    let unit = unit.ok_or_else(|| {
        ArgsError("fetch requires an <id> like \"@source:path/to/file\"".to_string())
    })?;
    match unit.strip_prefix('@').and_then(|r| r.split_once(':')) {
        Some((source, path)) if !source.is_empty() && !path.is_empty() => {}
        _ => {
            return Err(ArgsError(format!(
                "fetch needs an id like \"@source:path/to/file\", got \"{unit}\""
            )))
        }
    }
    Ok((Command::Fetch { unit }, json, addr))
}

/// `pin <id> [--yes] [--json] [--addr <addr>]` and `unpin <id> [--json]
/// [--addr <addr>]`. A bad id is a usage error, decided here, before any
/// request. An id may name a folder (`@g:docs/`), and its path may hold
/// spaces: it is one argument.
fn parse_pin<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
    pin: bool,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let verb = if pin { "pin" } else { "unpin" };
    let mut unit: Option<String> = None;
    let mut yes = false;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--yes" if pin => yes = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if unit.is_none() => unit = Some(other.to_string()),
            other => {
                return Err(ArgsError(format!(
                    "{verb} takes one id; unexpected extra argument: {other}"
                )))
            }
        }
    }

    let unit = unit.ok_or_else(|| {
        ArgsError(format!(
            "{verb} requires an <id> like \"@source:path/to/file\" or \"@source:path/to/folder/\""
        ))
    })?;
    match unit.strip_prefix('@').and_then(|r| r.split_once(':')) {
        Some((source, path)) if !source.is_empty() && !path.is_empty() => {}
        _ => {
            return Err(ArgsError(format!(
                "{verb} needs an id like \"@source:path/to/file\", got \"{unit}\""
            )))
        }
    }
    let command = if pin {
        Command::Pin { unit, yes }
    } else {
        Command::Unpin { unit }
    };
    Ok((command, json, addr))
}

/// `resolve <id> --mine|--theirs [--json] [--addr <addr>]`. The id may come
/// without its `@` (`gdrive:Plan.md`); it is sent with one. Exactly one of
/// the two flags: a usage error otherwise, before any request.
fn parse_resolve<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut unit: Option<String> = None;
    let mut keep: Option<&'static str> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            "--mine" | "--theirs" => {
                let this = if arg == "--mine" { "mine" } else { "theirs" };
                if keep.is_some() {
                    return Err(ArgsError(
                        "resolve takes --mine or --theirs, not both and not twice".to_string(),
                    ));
                }
                keep = Some(this);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other if unit.is_none() => unit = Some(other.to_string()),
            other => {
                return Err(ArgsError(format!(
                    "resolve takes one id; unexpected extra argument: {other}"
                )))
            }
        }
    }

    let unit = unit.ok_or_else(|| {
        ArgsError("resolve requires an <id> like \"@source:path/to/file\"".to_string())
    })?;
    let keep = keep.ok_or_else(|| {
        ArgsError(
            "resolve needs --mine (send your version to Drive) or --theirs (keep yours as \
             <name>.mine and take Drive's)"
                .to_string(),
        )
    })?;
    let unit = if unit.starts_with('@') {
        unit
    } else {
        format!("@{unit}")
    };
    match unit.strip_prefix('@').and_then(|r| r.split_once(':')) {
        Some((source, path)) if !source.is_empty() && !path.is_empty() => {}
        _ => {
            return Err(ArgsError(format!(
                "resolve needs an id like \"@source:path/to/file\", got \"{unit}\""
            )))
        }
    }
    Ok((
        Command::Resolve {
            unit,
            keep: keep.to_string(),
        },
        json,
        addr,
    ))
}

/// `cache status | set-limit <limit> | on <source> | off <source>`, each
/// with `[--json] [--addr <addr>]`. The limit is NOT checked here: the
/// daemon owns the size rules, and `abc`, `-5MB` or an empty value must
/// come back as its refusal, with the old limit named. `set-limit 10 GB`
/// (two words) is read as `10 GB`.
fn parse_cache<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let sub = iter.next().ok_or_else(|| {
        ArgsError("expected a cache subcommand: status, set-limit, on, or off".to_string())
    })?;
    let mut words: Vec<String> = Vec::new();
    let mut json = false;
    let mut addr = None;
    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            other if other.starts_with("--") => {
                return Err(ArgsError(format!("unrecognized argument: {other}")))
            }
            other => words.push(other.to_string()),
        }
    }
    let command = match sub {
        "status" => {
            if let Some(extra) = words.first() {
                return Err(ArgsError(format!(
                    "cache status takes no argument; unexpected: {extra}"
                )));
            }
            Command::CacheStatus
        }
        "set-limit" => {
            if words.is_empty() {
                return Err(ArgsError(
                    "cache set-limit requires a limit like \"10GB\" or \"500 MiB\"".to_string(),
                ));
            }
            if words.len() > 2 {
                return Err(ArgsError(
                    "cache set-limit takes one limit, like \"10GB\" or \"500 MiB\"".to_string(),
                ));
            }
            Command::CacheSetLimit {
                limit: words.join(" "),
            }
        }
        "on" | "off" => {
            let on = sub == "on";
            match words.as_slice() {
                [source] => Command::CacheSet {
                    source: source.clone(),
                    on,
                },
                [] => return Err(ArgsError(format!("cache {sub} requires a <source>"))),
                [_, extra, ..] => {
                    return Err(ArgsError(format!(
                        "cache {sub} takes one source; unexpected extra argument: {extra}"
                    )))
                }
            }
        }
        other => {
            return Err(ArgsError(format!(
                "unrecognized cache subcommand: {other} (expected status, set-limit, on, or off)"
            )))
        }
    };
    Ok((command, json, addr))
}

/// `fixes [--since <when>] [--json] [--addr <addr>]`. A bad `--since` is
/// a usage error, decided here, before any request.
fn parse_fixes<'a>(
    iter: &mut impl Iterator<Item = &'a str>,
) -> Result<(Command, bool, Option<String>), ArgsError> {
    let mut since: Option<Since> = None;
    let mut json = false;
    let mut addr = None;

    while let Some(arg) = iter.next() {
        match arg {
            "--json" => json = true,
            "--addr" => addr = Some(take_value(iter, "--addr")?),
            "--since" => {
                if since.is_some() {
                    return Err(ArgsError("--since was given twice".to_string()));
                }
                let value = take_value(iter, "--since")?;
                since = Some(Since::parse(&value).map_err(|e| ArgsError(e.0))?);
            }
            other => {
                return Err(ArgsError(format!(
                "fixes takes no arguments besides --since, --json and --addr; unexpected: {other}"
            )))
            }
        }
    }
    Ok((Command::Fixes { since }, json, addr))
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
    fn source_unpromote_parses_a_name_and_flags_and_needs_the_name() {
        let parsed = run(&["source", "unpromote", "tephrad"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceUnpromote {
                name: "tephrad".to_string()
            }
        );
        let json = run(&["source", "unpromote", "tephrad", "--json"]).unwrap();
        assert!(json.json);
        assert!(parse_args(["source", "unpromote"]).is_err());
        assert!(parse_args(["source", "unpromote", "a", "b"]).is_err());
    }

    #[test]
    fn source_rename_parses_both_names_and_flags_and_needs_both() {
        let parsed = run(&["source", "rename", "tephrad", "daemon"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceRename {
                old: "tephrad".to_string(),
                new: "daemon".to_string()
            }
        );
        assert!(!parsed.json);
        let json = run(&["source", "rename", "tephrad", "daemon", "--json"]).unwrap();
        assert!(json.json);
        // Missing arguments.
        let err = parse_args(["source", "rename"]).unwrap_err();
        assert!(
            err.0.contains("source rename requires <old> <new>"),
            "{}",
            err.0
        );
        let err = parse_args(["source", "rename", "tephrad"]).unwrap_err();
        assert!(
            err.0.contains("source rename requires <old> <new>"),
            "{}",
            err.0
        );
        // A third name is not accepted.
        assert!(parse_args(["source", "rename", "a", "b", "c"]).is_err());
    }

    /// The new name is checked client-side with the widened rule (`:` and
    /// `*` join `/`, `@` and whitespace); the old name never is (FR14).
    #[test]
    fn source_rename_checks_the_new_name_only_and_with_the_widened_rule() {
        for bad in ["", "bad/name", "a@b", "a:b", "a*b", "a b", "a\tb"] {
            let err = parse_args(["source", "rename", "tephrad", bad]).unwrap_err();
            let want = if bad.is_empty() {
                "source name must not be empty"
            } else {
                "invalid source name"
            };
            assert!(err.0.contains(want), "{bad:?}: {}", err.0);
        }
        let err = parse_args(["source", "rename", "tephrad", "a:b"]).unwrap_err();
        assert!(err.0.contains("':'") && err.0.contains("'*'"), "{}", err.0);
        // An existing name that breaks today's rules can still be renamed away.
        let parsed = run(&["source", "rename", "old*name", "fresh"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::SourceRename {
                old: "old*name".to_string(),
                new: "fresh".to_string()
            }
        );
        // `source add` and `source promote` take the widened rule too.
        assert!(parse_args(["source", "add", "a:b", "/p"]).is_err());
        assert!(parse_args(["source", "add", "a*b", "/p"]).is_err());
        assert!(parse_args(["source", "promote", "a:b", "/p"]).is_err());
    }

    #[test]
    fn source_promote_parses_name_and_path_and_validates_the_name() {
        let parsed = run(&[
            "source",
            "promote",
            "tephrad",
            "/home/dan/Tephra/crates/tephrad",
        ])
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::SourcePromote {
                name: "tephrad".to_string(),
                path: "/home/dan/Tephra/crates/tephrad".to_string(),
            }
        );
        let json = run(&["source", "promote", "t", "/p", "--json"]).unwrap();
        assert!(json.json);
        assert!(parse_args(["source", "promote", "t"]).is_err());
        assert!(parse_args(["source", "promote"]).is_err());
        assert!(parse_args(["source", "promote", "my notes", "/p"]).is_err());
        assert!(parse_args(["source", "promote", "a@b", "/p"]).is_err());
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
