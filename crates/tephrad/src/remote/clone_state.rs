//! What a GitHub source's git clone says, and what Tephra may do about it
//! (atlas-27 PR-9, G13 and G15).
//!
//! The clone is Dan's: he commits, pushes and runs `git pull --rebase`
//! himself. Tephra only LOOKS (`rev-parse`, `rev-list`, `merge-base`, `diff`,
//! `status`), FETCHES (which writes only inside `.git`), and in one case
//! moves the branch: `merge --ff-only`, when the clone is clean and strictly
//! behind. Nothing else is ever run: [`Git::run`] refuses any argument list
//! that is not on the allowlist before a process is started (F3), and no
//! command line carries a token (the fetch uses whatever the clone's own
//! `origin` and git's own credential helper give).
//!
//! This file has three parts: the pure rules ([`decide`], [`both_sides`]),
//! the parsers of git's NUL-delimited output, and the runner of git itself.
//! The pass that uses them (when it runs, what it writes) is
//! `remote::clone_sync`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The rules (G15, E12, E23).
// ---------------------------------------------------------------------------

/// What a pass knows of the clone before it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Commits on the clone's HEAD that the tracked remote branch lacks.
    pub ahead: u64,
    /// Commits on the tracked remote branch that HEAD lacks.
    pub behind: u64,
    /// A tracked file has an edit (staged or not).
    pub dirty: bool,
    /// A file git does not track (and does not ignore) is there.
    pub untracked: bool,
    /// HEAD is the tracked branch.
    pub on_branch: bool,
    /// The remote's history was rewritten: the tip recorded before is not an
    /// ancestor of the new one.
    pub rewritten: bool,
}

/// Why a clone needs attention as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// A force-push (E12).
    Rewritten,
    /// HEAD is another branch, or no branch at all.
    WrongBranch,
}

/// What a pass does with the clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Move the branch forward with `merge --ff-only`: the clone is clean
    /// and strictly behind.
    FastForward,
    /// Fetch only: the working tree, index, HEAD and branch are untouched.
    FetchOnly,
    /// Fetch only, and the source shows why.
    NeedsAttention(Attention),
}

/// The pull rule. A fast-forward needs ALL of: the clone is on the tracked
/// branch, the history was not rewritten, it is strictly behind (behind and
/// not ahead), no tracked file has an edit and no untracked file exists. Any
/// other state is a fetch and nothing more.
pub fn decide(f: &Facts) -> Decision {
    if f.rewritten {
        return Decision::NeedsAttention(Attention::Rewritten);
    }
    if !f.on_branch {
        return Decision::NeedsAttention(Attention::WrongBranch);
    }
    if f.behind > 0 && f.ahead == 0 && !f.dirty && !f.untracked {
        Decision::FastForward
    } else {
        Decision::FetchOnly
    }
}

/// The files changed on both sides since the merge base: Dan's (committed or
/// not) and the remote's. Only these need attention; a file changed on one
/// side keeps its normal state.
pub fn both_sides(local: &BTreeSet<String>, remote: &BTreeSet<String>) -> BTreeSet<String> {
    local.intersection(remote).cloned().collect()
}

// ---------------------------------------------------------------------------
// Parsers of git's NUL-delimited output.
// ---------------------------------------------------------------------------

/// One entry of `git diff --name-status -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatus {
    /// `M`, `A`, `D`, `T`, `U`, `R` or `C` (a rename or copy has a score
    /// after it in git's output; it is dropped here).
    pub status: char,
    pub path: String,
    /// The old path of a rename or a copy.
    pub old_path: Option<String>,
}

/// Splits `bytes` at NUL; a final empty piece (the trailing NUL) is dropped.
fn fields(bytes: &[u8]) -> Vec<&[u8]> {
    let mut parts: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    parts
}

/// A path as text. A name that is not UTF-8 is shown with U+FFFD; it still
/// counts as one file.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `git diff --name-status -z`: `M NUL path NUL`, and for a rename or copy
/// `R<score> NUL old NUL new NUL`. A truncated last entry is dropped.
pub fn parse_name_status_z(out: &[u8]) -> Vec<NameStatus> {
    let parts = fields(out);
    let mut i = 0;
    let mut entries = Vec::new();
    while i < parts.len() {
        let status = parts[i].first().map(|b| *b as char).unwrap_or('?');
        if status == 'R' || status == 'C' {
            let (Some(old), Some(new)) = (parts.get(i + 1), parts.get(i + 2)) else {
                break;
            };
            entries.push(NameStatus {
                status,
                path: text(new),
                old_path: Some(text(old)),
            });
            i += 3;
        } else {
            let Some(path) = parts.get(i + 1) else { break };
            entries.push(NameStatus {
                status,
                path: text(path),
                old_path: None,
            });
            i += 2;
        }
    }
    entries
}

/// One entry of `git diff --raw -z --no-abbrev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// `M`, `A`, `D`, `T`, `U` (`R` and `C` keep their letter).
    pub status: char,
    /// The blob (or commit, for a submodule) the new side holds; all zeros
    /// for a delete.
    pub new_sha: String,
    pub path: String,
}

/// `git diff --raw -z --no-abbrev`: `:<mode> <mode> <sha> <sha> <S> NUL path
/// NUL`, and for a rename or copy two paths. A truncated last entry is
/// dropped.
pub fn parse_raw_z(out: &[u8]) -> Vec<RawEntry> {
    let parts = fields(out);
    let mut i = 0;
    let mut entries = Vec::new();
    while i < parts.len() {
        let meta = String::from_utf8_lossy(parts[i]).into_owned();
        i += 1;
        let mut words = meta.trim_start_matches(':').split(' ');
        let (Some(_), Some(_), Some(_), Some(new_sha), Some(status)) = (
            words.next(),
            words.next(),
            words.next(),
            words.next(),
            words.next(),
        ) else {
            continue;
        };
        let letter = status.chars().next().unwrap_or('?');
        let paths = if letter == 'R' || letter == 'C' { 2 } else { 1 };
        let Some(path) = parts.get(i + paths - 1) else {
            break;
        };
        i += paths;
        entries.push(RawEntry {
            status: letter,
            new_sha: new_sha.to_string(),
            path: text(path),
        });
    }
    entries
}

/// The paths a name-status list says differ: the new path of every entry,
/// and the old path of a rename (that file is gone from where it was).
pub fn changed_paths(entries: &[NameStatus]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for e in entries {
        set.insert(e.path.clone());
        if e.status == 'R' {
            if let Some(old) = &e.old_path {
                set.insert(old.clone());
            }
        }
    }
    set
}

/// What `git status --porcelain=v2 -z` lists.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusEntries {
    /// Tracked files with a change (ordinary and renamed entries): the new
    /// path, and for a rename the old path too.
    pub tracked: Vec<String>,
    /// Paths with an unresolved merge conflict.
    pub unmerged: Vec<String>,
    /// Files git does not track and does not ignore.
    pub untracked: Vec<String>,
}

impl StatusEntries {
    /// A tracked file has an edit.
    pub fn dirty(&self) -> bool {
        !self.tracked.is_empty() || !self.unmerged.is_empty()
    }
}

/// `git status --porcelain=v2 -z`: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI>
/// <path>`, `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path> NUL
/// <orig>`, `u ... <path>`, `? <path>`, `! <path>`. The path is the rest of
/// the entry after the fixed fields, so it may hold spaces. A `#` header and
/// an ignored entry say nothing about changes.
pub fn parse_status_v2_z(out: &[u8]) -> StatusEntries {
    let parts = fields(out);
    let mut entries = StatusEntries::default();
    let mut i = 0;
    while i < parts.len() {
        let line = parts[i];
        i += 1;
        let Some(kind) = line.first() else { continue };
        // The path follows `skip` spaces. Spaces inside a name are not
        // counted: only the fixed fields are split off.
        let path_after = |skip: usize| -> Option<&[u8]> {
            let mut seen = 0;
            for (at, b) in line.iter().enumerate() {
                if *b == b' ' {
                    seen += 1;
                    if seen == skip {
                        return line.get(at + 1..);
                    }
                }
            }
            None
        };
        match kind {
            b'1' => {
                if let Some(p) = path_after(8) {
                    entries.tracked.push(text(p));
                }
            }
            b'2' => {
                if let Some(p) = path_after(9) {
                    entries.tracked.push(text(p));
                    if let Some(orig) = parts.get(i) {
                        entries.tracked.push(text(orig));
                    }
                }
                i += 1;
            }
            b'u' => {
                if let Some(p) = path_after(10) {
                    entries.unmerged.push(text(p));
                }
            }
            b'?' => {
                if let Some(p) = path_after(1) {
                    entries.untracked.push(text(p));
                }
            }
            _ => {}
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// git itself.
// ---------------------------------------------------------------------------

/// Why a git call gave no answer.
#[derive(Debug)]
pub enum GitError {
    /// The argument list is not on the allowlist: nothing was started.
    NotAllowed(String),
    /// The program could not be started.
    Spawn(std::io::Error),
    /// It ran past its bound and was killed.
    TimedOut,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::NotAllowed(what) => write!(f, "git {what} is not allowed"),
            GitError::Spawn(e) => write!(f, "git could not be started: {}", e.kind()),
            GitError::TimedOut => f.write_str("git took too long"),
        }
    }
}

impl std::error::Error for GitError {}

/// What a git call printed. `stderr` is never shown or logged as it is: a
/// clone's `origin` URL can carry a person's own credentials.
#[derive(Debug)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The first line of stdout, trimmed.
    pub fn line(&self) -> String {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// The git commands Tephra runs that only read.
const READ_ONLY: [&str; 6] = [
    "fetch",
    "rev-parse",
    "rev-list",
    "merge-base",
    "diff",
    "status",
];

/// `true` when `args` is a command Tephra may run (F3): a read, a fetch, a
/// `ls-files`, or `merge --ff-only --no-overwrite-ignore <ref>` (never any
/// other merge). `--no-overwrite-ignore` makes git refuse a fast-forward that
/// would replace one of Dan's ignored files (a `.env`, say), which git treats
/// as expendable otherwise. Nothing else is on the list.
pub fn allowed(args: &[&str]) -> bool {
    match args {
        ["merge", "--ff-only", "--no-overwrite-ignore", target] => !target.starts_with('-'),
        [cmd, ..] => READ_ONLY.contains(cmd) || *cmd == "ls-files",
        [] => false,
    }
}

/// How long a fetch may take, and any other call.
const FETCH_BOUND: Duration = Duration::from_secs(120);
const READ_BOUND: Duration = Duration::from_secs(60);

/// git in one clone: `program` run with `dir` as its working directory.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    dir: PathBuf,
}

impl Git {
    pub fn new(program: PathBuf, dir: PathBuf) -> Self {
        Git { program, dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs `args` (the first is the git command) in the clone. The
    /// allowlist is checked first, so a call that is not on it never starts a
    /// process. The environment is the person's own, less the variables that
    /// point git elsewhere, and with prompts and optional index writes off:
    /// a status never refreshes the index, and nothing waits for a password.
    pub fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        if !allowed(args) {
            return Err(GitError::NotAllowed(
                args.first().copied().unwrap_or("").to_string(),
            ));
        }
        let bound = if args[0] == "fetch" {
            FETCH_BOUND
        } else {
            READ_BOUND
        };
        let mut cmd = Command::new(&self.program);
        cmd.args(args.iter().map(OsStr::new))
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C");
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        let mut child = cmd.spawn().map_err(GitError::Spawn)?;
        // Both pipes are read while the process runs: a big listing fills a
        // pipe and would block git otherwise.
        let mut out = child.stdout.take().expect("stdout is piped");
        let mut err = child.stderr.take().expect("stderr is piped");
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() > bound => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        match status {
            Some(status) => Ok(GitOutput {
                code: status.code(),
                stdout,
                stderr,
            }),
            None => Err(GitError::TimedOut),
        }
    }
}

// ---------------------------------------------------------------------------
// Why a fetch failed.
// ---------------------------------------------------------------------------

/// What a failed `git fetch` means for the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailure {
    /// The remote cannot be reached.
    Offline,
    /// The remote refused the credentials git has (a 401 or 403).
    Auth,
    /// The tracked branch is not on the remote any more.
    NoRemoteBranch,
}

/// Sorts git's complaint (English: the runner sets `LC_ALL=C`). An
/// authentication failure is told first: git says both "unable to access" and
/// "401" in one message. A complaint nobody recognises counts as offline: the
/// last values are kept and the next pass tries again.
pub fn classify_fetch_failure(stderr: &[u8]) -> FetchFailure {
    let text = String::from_utf8_lossy(stderr).to_lowercase();
    const AUTH: [&str; 9] = [
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "returned error: 401",
        "returned error: 403",
        "permission denied (publickey",
        "http 401",
        "invalid credentials",
    ];
    if AUTH.iter().any(|m| text.contains(m)) {
        return FetchFailure::Auth;
    }
    if text.contains("couldn't find remote ref") {
        return FetchFailure::NoRemoteBranch;
    }
    FetchFailure::Offline
}

// ---------------------------------------------------------------------------
// The tracked branch.
// ---------------------------------------------------------------------------

/// `true` when `branch` is safe to put in a refspec: not empty, no leading
/// dash, no control character, space or any of git's forbidden characters.
pub fn safe_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("@{")
        && branch != "@"
        && !branch.chars().any(|c| {
            c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '+')
        })
}

/// The marker files and folders in a git dir that say Dan is in the middle
/// of a rebase, a merge, a cherry-pick or a revert, or that git itself holds
/// the index.
pub const IN_PROGRESS_MARKERS: [&str; 6] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "index.lock",
];

/// The first marker of [`IN_PROGRESS_MARKERS`] present in `git_dir`.
pub fn operation_in_progress(git_dir: &Path) -> Option<&'static str> {
    IN_PROGRESS_MARKERS
        .into_iter()
        .find(|m| std::fs::symlink_metadata(git_dir.join(m)).is_ok())
}

// ---------------------------------------------------------------------------
// A look at the clone.
// ---------------------------------------------------------------------------

/// What one look at a clone found (nothing was changed to find it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// `HEAD`'s branch, or `HEAD` when it is detached.
    pub branch_now: String,
    /// The commit HEAD is at.
    pub head: String,
    /// The tracked remote branch's tip, as the last fetch left it.
    pub tip: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub dirty: bool,
    pub untracked: bool,
    /// Files Dan changed since the merge base: the working tree against it
    /// (edits, new and deleted files, and everything in unpushed commits),
    /// plus untracked files.
    pub local: BTreeSet<String>,
    /// Files the remote changed since the merge base.
    pub remote: BTreeSet<String>,
}

/// Why a look failed.
#[derive(Debug)]
pub enum LookError {
    Git(GitError),
    /// A command exited non-zero where it must not (an unborn HEAD, say).
    Failed(&'static str),
}

impl From<GitError> for LookError {
    fn from(e: GitError) -> Self {
        LookError::Git(e)
    }
}

impl std::fmt::Display for LookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookError::Git(e) => write!(f, "{e}"),
            LookError::Failed(what) => write!(f, "git {what} failed"),
        }
    }
}

/// The remote-tracking ref of `branch`.
pub fn tracking_ref(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// The sha `refspec` names (a commit), or `None` when there is none.
pub fn resolve(git: &Git, refspec: &str) -> Result<Option<String>, GitError> {
    let spec = format!("{refspec}^{{commit}}");
    let out = git.run(&["rev-parse", "--verify", "-q", &spec])?;
    Ok(out.ok().then(|| out.line()).filter(|s| !s.is_empty()))
}

/// `true` when `old` is an ancestor of `new`. An answer git cannot give (an
/// object it does not have) is `false`: the caller then takes the safe side.
pub fn is_ancestor(git: &Git, old: &str, new: &str) -> Result<bool, GitError> {
    let out = git.run(&["merge-base", "--is-ancestor", old, new])?;
    Ok(out.code == Some(0))
}

/// What the tip holds, for the files git does not track: path -> blob sha of
/// every file the tip has and HEAD lacks or holds differently, from ONE
/// `git diff` of the two commits. The untracked files are checked against it
/// without a git call each (there can be thousands).
fn tip_blobs(
    git: &Git,
    head: &str,
    tip: &str,
) -> Result<std::collections::HashMap<String, String>, LookError> {
    let out = git.run(&[
        "diff",
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-renames",
        "--no-ext-diff",
        "--ignore-submodules=all",
        head,
        tip,
        "--",
    ])?;
    if !out.ok() {
        return Err(LookError::Failed("diff"));
    }
    Ok(parse_raw_z(&out.stdout)
        .into_iter()
        .filter(|e| e.status != 'D')
        .map(|e| (e.path, e.new_sha))
        .collect())
}

/// `true` when the history was rewritten upstream and Dan has not yet
/// brought his clone to the new one (E12). `old` is the tip recorded by the
/// last pass, `tip` the tracked branch now. A rewrite is a new tip that does
/// not descend from the old one: the branch was replaced (amend, rebase) or
/// moved back (its last commits dropped). It stays open for as long as the
/// clone has not left the old history: HEAD is still behind the new tip, still
/// holds the old tip, or sits strictly inside the dropped commits. It closes
/// when HEAD is the new tip or has it below Dan's own commits and does not
/// hold the old tip, which is what his own `git pull --rebase` (or a reset)
/// leaves.
pub fn history_rewritten(
    git: &Git,
    old: &str,
    tip: &str,
    head: &str,
    behind: u64,
) -> Result<bool, GitError> {
    if old == tip || is_ancestor(git, old, tip)? {
        return Ok(false);
    }
    if behind > 0 || is_ancestor(git, old, head)? {
        return Ok(true);
    }
    Ok(head != tip && is_ancestor(git, head, old)?)
}

/// Looks at the clone against the tracked `branch`'s remote-tracking ref.
/// `None` when the clone has no such ref yet (it was never fetched).
pub fn snapshot(git: &Git, branch: &str) -> Result<Option<Snapshot>, LookError> {
    let head = git.run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !head.ok() {
        return Err(LookError::Failed("rev-parse HEAD"));
    }
    let branch_now = head.line();
    let remote_ref = tracking_ref(branch);
    let Some(tip) = resolve(git, &remote_ref)? else {
        return Ok(None);
    };
    let Some(head_sha) = resolve(git, "HEAD")? else {
        return Err(LookError::Failed("rev-parse HEAD"));
    };

    // Left is HEAD's own commits, right the remote's.
    let range = format!("{head_sha}...{tip}");
    let counts = git.run(&["rev-list", "--left-right", "--count", &range])?;
    if !counts.ok() {
        return Err(LookError::Failed("rev-list"));
    }
    let line = counts.line();
    let mut nums = line.split_whitespace().map(|n| n.parse::<u64>().ok());
    let (Some(Some(ahead)), Some(Some(behind))) = (nums.next(), nums.next()) else {
        return Err(LookError::Failed("rev-list"));
    };

    // The base Dan's changes are measured from: where the two histories
    // part. With none (unrelated histories) it is the tip itself.
    let mb = git.run(&["merge-base", &head_sha, &tip])?;
    let base = if mb.ok() && !mb.line().is_empty() {
        mb.line()
    } else {
        tip.clone()
    };

    let status = git.run(&[
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ])?;
    if !status.ok() {
        return Err(LookError::Failed("status"));
    }
    let status = parse_status_v2_z(&status.stdout);

    // The working tree against the base: what Dan changed, committed or not.
    let mine = git.run(&[
        "diff",
        "--name-status",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--ignore-submodules=all",
        &base,
        "--",
    ])?;
    if !mine.ok() {
        return Err(LookError::Failed("diff"));
    }
    let mut local = changed_paths(&parse_name_status_z(&mine.stdout));
    // A file git does not track counts, unless it is exactly the tip's file
    // at that path (what a `fetch` of a file added on GitHub leaves before
    // the clone fast-forwards): it does not differ from the tip. The tip's
    // blobs come from one `git diff`, and only an untracked file whose path
    // the tip has is hashed: no git call per file.
    if !status.untracked.is_empty() {
        let tips = tip_blobs(git, &head_sha, &tip)?;
        for path in &status.untracked {
            let is_tips_file = tips.get(path).is_some_and(|sha| {
                let on_disk = git.dir().join(path);
                std::fs::symlink_metadata(&on_disk).is_ok_and(|m| m.file_type().is_file())
                    && crate::remote::poll::file_has_blob_sha(&on_disk, sha)
            });
            if !is_tips_file {
                local.insert(path.clone());
            }
        }
    }
    let untracked_all = !status.untracked.is_empty();
    local.extend(status.unmerged.iter().cloned());

    // The remote's changes since the base.
    let theirs = git.run(&[
        "diff",
        "--name-status",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--ignore-submodules=all",
        &base,
        &tip,
        "--",
    ])?;
    if !theirs.ok() {
        return Err(LookError::Failed("diff"));
    }
    let remote = changed_paths(&parse_name_status_z(&theirs.stdout));

    Ok(Some(Snapshot {
        branch_now,
        head: head_sha,
        tip: Some(tip),
        ahead,
        behind,
        dirty: status.dirty(),
        untracked: untracked_all,
        local,
        remote,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(ahead: u64, behind: u64, dirty: bool, untracked: bool) -> Facts {
        Facts {
            ahead,
            behind,
            dirty,
            untracked,
            on_branch: true,
            rewritten: false,
        }
    }

    /// Every rule of G15, E12 and E23 as one row: `(ahead, behind, dirty,
    /// untracked, on the branch, rewritten)` and what is done.
    #[test]
    fn the_pull_rules_for_every_combination() {
        use Attention::*;
        use Decision::*;
        let table: [(Facts, Decision, &str); 19] = [
            // G15 / E23: clean and strictly behind.
            (facts(0, 1, false, false), FastForward, "clean, behind 1"),
            (facts(0, 7, false, false), FastForward, "clean, behind 7"),
            // Nothing to pull.
            (facts(0, 0, false, false), FetchOnly, "up to date"),
            (facts(2, 0, false, false), FetchOnly, "only ahead"),
            // G15 / E23: any other state is a fetch and nothing more.
            (facts(1, 1, false, false), FetchOnly, "ahead and behind"),
            (facts(3, 4, true, true), FetchOnly, "everything at once"),
            (
                facts(0, 1, true, false),
                FetchOnly,
                "behind, a tracked edit",
            ),
            (
                facts(0, 1, false, true),
                FetchOnly,
                "behind, an untracked file",
            ),
            (
                facts(0, 1, true, true),
                FetchOnly,
                "behind, edit and untracked",
            ),
            (facts(0, 0, true, false), FetchOnly, "edit, not behind"),
            (
                facts(1, 3, true, false),
                FetchOnly,
                "ahead, behind and an edit",
            ),
            // E12: a rewritten history is never pulled.
            (
                Facts {
                    rewritten: true,
                    ..facts(0, 1, false, false)
                },
                NeedsAttention(Rewritten),
                "a force-push, clean",
            ),
            (
                Facts {
                    rewritten: true,
                    ..facts(2, 5, true, true)
                },
                NeedsAttention(Rewritten),
                "a force-push with local work",
            ),
            // A force-push that moved the branch back: the clone is not
            // behind (the new tip is in its history), and it is still a
            // rewrite.
            (
                Facts {
                    rewritten: true,
                    ..facts(0, 0, false, false)
                },
                NeedsAttention(Rewritten),
                "a force-push back, clean, not behind",
            ),
            (
                Facts {
                    rewritten: true,
                    ..facts(1, 0, false, false)
                },
                NeedsAttention(Rewritten),
                "a force-push back, one dropped commit counted ahead",
            ),
            (
                Facts {
                    rewritten: true,
                    ..facts(2, 0, true, true)
                },
                NeedsAttention(Rewritten),
                "a force-push back with local work",
            ),
            // Another branch or a detached HEAD.
            (
                Facts {
                    on_branch: false,
                    ..facts(0, 1, false, false)
                },
                NeedsAttention(WrongBranch),
                "not on the branch, clean and behind",
            ),
            (
                Facts {
                    on_branch: false,
                    ..facts(0, 0, false, false)
                },
                NeedsAttention(WrongBranch),
                "not on the branch, up to date",
            ),
            // A rewritten history outranks the branch.
            (
                Facts {
                    on_branch: false,
                    rewritten: true,
                    ..facts(0, 1, false, false)
                },
                NeedsAttention(Rewritten),
                "rewritten and on another branch",
            ),
        ];
        for (f, want, what) in table {
            assert_eq!(decide(&f), want, "{what}: {f:?}");
        }
    }

    #[test]
    fn only_a_file_changed_on_both_sides_is_in_both() {
        let set =
            |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| n.to_string()).collect() };
        let local = set(&["a.md", "b.md", "new.md"]);
        let remote = set(&["b.md", "c.md"]);
        assert_eq!(both_sides(&local, &remote), set(&["b.md"]));
        assert!(both_sides(&local, &set(&[])).is_empty());
        assert!(both_sides(&set(&[]), &remote).is_empty());
    }

    #[test]
    fn name_status_z_reads_every_kind_of_entry() {
        let out = b"M\0README.md\0A\0docs/new file.md\0D\0gone.md\0\
                    R100\0old name.md\0new name.md\0C75\0orig.md\0copy.md\0\
                    M\0caf\xc3\xa9/\xe6\x97\xa5\xe6\x9c\xac.md\0M\0-dash.md\0M\0two\nlines.md\0";
        let got = parse_name_status_z(out);
        let m = |status, path: &str, old: Option<&str>| NameStatus {
            status,
            path: path.to_string(),
            old_path: old.map(str::to_string),
        };
        assert_eq!(
            got,
            vec![
                m('M', "README.md", None),
                m('A', "docs/new file.md", None),
                m('D', "gone.md", None),
                m('R', "new name.md", Some("old name.md")),
                m('C', "copy.md", Some("orig.md")),
                m('M', "café/日本.md", None),
                m('M', "-dash.md", None),
                m('M', "two\nlines.md", None),
            ]
        );
        // A rename is two files that differ; a copy one.
        let paths = changed_paths(&got);
        assert!(paths.contains("old name.md") && paths.contains("new name.md"));
        assert!(paths.contains("copy.md") && !paths.contains("orig.md"));
        assert_eq!(paths.len(), 9);
    }

    #[test]
    fn raw_z_reads_the_new_blob_of_every_entry() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let zero = "0".repeat(40);
        let mut out: Vec<u8> = Vec::new();
        let mut push = |s: &str| {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        };
        push(&format!(":000000 100644 {zero} {a} A"));
        push("docs/new file.md");
        push(&format!(":100644 100644 {a} {b} M"));
        push("caf\u{e9}/\u{65e5}\u{672c}.md");
        push(&format!(":100644 000000 {a} {zero} D"));
        push("gone.md");
        push(&format!(":100644 100644 {a} {a} R100"));
        push("old name.md");
        push("-new name.md");
        push(&format!(":000000 100644 {zero} {b} A"));
        push("two\nlines.md");
        let got = parse_raw_z(&out);
        let e = |status, sha: &str, path: &str| RawEntry {
            status,
            new_sha: sha.to_string(),
            path: path.to_string(),
        };
        assert_eq!(
            got,
            vec![
                e('A', &a, "docs/new file.md"),
                e('M', &b, "caf\u{e9}/\u{65e5}\u{672c}.md"),
                e('D', &zero, "gone.md"),
                e('R', &a, "-new name.md"),
                e('A', &b, "two\nlines.md"),
            ]
        );
        assert!(parse_raw_z(b"").is_empty());
        // A cut entry is dropped, not guessed at.
        assert!(parse_raw_z(format!(":000000 100644 {zero} {a} A\0").as_bytes()).is_empty());
    }

    #[test]
    fn name_status_z_of_nothing_or_a_cut_entry() {
        assert!(parse_name_status_z(b"").is_empty());
        assert!(parse_name_status_z(b"M\0").is_empty());
        assert!(parse_name_status_z(b"R90\0old\0").is_empty());
        let got = parse_name_status_z(b"M\0ok.md\0D\0");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn status_v2_z_reads_every_kind_of_entry() {
        let mut out: Vec<u8> = Vec::new();
        let mut push = |s: &[u8]| {
            out.extend_from_slice(s);
            out.push(0);
        };
        push(b"# branch.oid abc");
        push(b"1 .M N... 100644 100644 100644 aaaa bbbb README.md");
        push(b"1 A. N... 000000 100644 100644 0000 cccc docs/a file with spaces.md");
        push(b"1 .D N... 100644 100644 000000 aaaa aaaa gone.md");
        push(b"2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.md");
        push(b"old name.md");
        push(b"u UU N... 100644 100644 100644 100644 a b c conflicted.md");
        push("? caf\u{e9}/\u{65e5}\u{672c}.md".as_bytes());
        push(b"? -leading dash.md");
        push(b"? two\nlines.md");
        push(b"! ignored.log");
        let got = parse_status_v2_z(&out);
        assert_eq!(
            got.tracked,
            vec![
                "README.md",
                "docs/a file with spaces.md",
                "gone.md",
                "new name.md",
                "old name.md",
            ]
        );
        assert_eq!(got.unmerged, vec!["conflicted.md"]);
        assert_eq!(
            got.untracked,
            vec!["café/日本.md", "-leading dash.md", "two\nlines.md"]
        );
        assert!(got.dirty());
    }

    #[test]
    fn status_v2_z_of_a_clean_clone_is_clean() {
        let got = parse_status_v2_z(b"");
        assert_eq!(got, StatusEntries::default());
        assert!(!got.dirty());
        let only_untracked = parse_status_v2_z(b"? new.md\0");
        assert!(!only_untracked.dirty());
        assert_eq!(only_untracked.untracked, vec!["new.md"]);
    }

    #[test]
    fn the_allowlist_has_the_reads_a_fetch_and_a_fast_forward_only() {
        for ok in [
            &["fetch", "--no-tags", "origin"][..],
            &["rev-parse", "HEAD"],
            &["rev-list", "--count", "a...b"],
            &["merge-base", "a", "b"],
            &["diff", "--name-status", "-z"],
            &["status", "--porcelain=v2", "-z"],
            &["ls-files", "-z"],
            &[
                "merge",
                "--ff-only",
                "--no-overwrite-ignore",
                "refs/remotes/origin/main",
            ],
        ] {
            assert!(allowed(ok), "{ok:?}");
        }
        for bad in [
            &["commit", "-m", "x"][..],
            &["push"],
            &["rebase", "main"],
            &["reset", "--hard"],
            &["checkout", "main"],
            &["stash"],
            &["pull"],
            &["merge", "origin/main"],
            &["merge", "--no-ff", "origin/main"],
            &["merge", "--ff-only"],
            // A fast-forward that may overwrite an ignored file is not allowed.
            &["merge", "--ff-only", "refs/remotes/origin/main"],
            &["merge", "--no-overwrite-ignore", "refs/remotes/origin/main"],
            &["merge", "--ff-only", "--no-overwrite-ignore"],
            &["merge", "--ff-only", "--no-overwrite-ignore", "--squash"],
            &[
                "merge",
                "--ff-only",
                "--overwrite-ignore",
                "refs/remotes/origin/main",
            ],
            &["merge", "--ff-only", "--strategy=ours"],
            &["merge", "--ff-only", "a", "b"],
            &["cherry-pick", "x"],
            &["clean", "-fd"],
            &["gc"],
            &[],
        ] {
            assert!(!allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_command_off_the_list_starts_no_process() {
        let dir = tempfile::tempdir().unwrap();
        // The program does not exist: if the allowlist did not stop the call
        // first the error would be `Spawn`.
        let git = Git::new(PathBuf::from("/nonexistent/git"), dir.path().to_path_buf());
        assert!(matches!(
            git.run(&["push"]),
            Err(GitError::NotAllowed(cmd)) if cmd == "push"
        ));
        assert!(matches!(git.run(&["status"]), Err(GitError::Spawn(_))));
    }

    #[test]
    fn a_failed_fetch_is_told_by_its_complaint() {
        use FetchFailure::*;
        for (stderr, want) in [
            ("fatal: Authentication failed for 'https://github.com/o/r.git/'", Auth),
            (
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
                Auth,
            ),
            (
                "fatal: unable to access 'https://github.com/o/r.git/': The requested URL returned error: 401",
                Auth,
            ),
            (
                "fatal: unable to access 'x': The requested URL returned error: 403",
                Auth,
            ),
            ("git@github.com: Permission denied (publickey).", Auth),
            (
                "fatal: unable to access 'https://github.com/o/r.git/': Could not resolve host: github.com",
                Offline,
            ),
            (
                "fatal: unable to access 'http://127.0.0.1:9/': Failed to connect to 127.0.0.1 port 9",
                Offline,
            ),
            ("fatal: couldn't find remote ref refs/heads/gone", NoRemoteBranch),
            ("something nobody has seen before", Offline),
            ("", Offline),
        ] {
            assert_eq!(classify_fetch_failure(stderr.as_bytes()), want, "{stderr}");
        }
    }

    #[test]
    fn a_branch_name_is_checked_before_it_goes_into_a_refspec() {
        for good in [
            "main",
            "release/1.2",
            "feature/with-dash",
            "caf\u{e9}",
            "v1.0",
        ] {
            assert!(safe_branch(good), "{good}");
        }
        for bad in [
            "",
            "-main",
            "--upload-pack=x",
            "a b",
            "a..b",
            "a:b",
            "a~1",
            "a^",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "+main",
            "main.lock",
            "x/",
            "/x",
            "a\nb",
            "@",
            "a@{1}",
        ] {
            assert!(!safe_branch(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_operation_in_progress_is_seen_by_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(operation_in_progress(dir.path()), None);
        for marker in IN_PROGRESS_MARKERS {
            let sub = dir.path().join(marker.replace('.', "_"));
            std::fs::create_dir(&sub).unwrap();
            let target = sub.join(marker);
            if marker.starts_with("rebase") {
                std::fs::create_dir(&target).unwrap();
            } else {
                std::fs::write(&target, b"").unwrap();
            }
            assert_eq!(operation_in_progress(&sub), Some(marker));
        }
    }

    // ---- real git ------------------------------------------------------

    fn real_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        real_git(dir, &["add", "-A"]);
        real_git(dir, &["commit", "-m", text]);
        real_git(dir, &["rev-parse", "HEAD"])
    }

    /// E12: a rewrite is a new tip that is no descendant of the old one,
    /// whether the branch was replaced or moved back; it stays open until
    /// the clone has left the old history.
    #[test]
    fn a_rewrite_is_told_from_a_fast_forward_and_closes_when_the_clone_moves_on() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        real_git(d, &["init", "-q", "-b", "main", "."]);
        let c1 = commit(d, "f", "1");
        let c2 = commit(d, "f", "2");
        let c3 = commit(d, "f", "3");
        // The replaced history: c2 amended into x2 (and x3 on top of it).
        real_git(d, &["checkout", "-q", "-b", "other", &c1]);
        let x2 = commit(d, "f", "x2");
        // Dan's own commit on the new history, and on the dropped one.
        let dan_on_new = commit(d, "dan", "dan");
        real_git(d, &["checkout", "-q", "-b", "dan-old", &c3]);
        let dan_on_old = commit(d, "dan", "dan");
        let git = Git::new(PathBuf::from("git"), d.to_path_buf());
        let rewritten = |old: &str, tip: &str, head: &str, behind: u64| {
            history_rewritten(&git, old, tip, head, behind).unwrap()
        };

        // Nothing moved, or the tip moved forward: no rewrite.
        assert!(!rewritten(&c2, &c2, &c2, 0));
        assert!(!rewritten(&c2, &c3, &c2, 1));
        assert!(!rewritten(&c2, &c3, &c3, 0));

        // The branch moved back (its last commits dropped).
        assert!(rewritten(&c3, &c1, &c3, 0), "HEAD still holds the old tip");
        assert!(rewritten(&c3, &c1, &dan_on_old, 0), "and Dan's work on it");
        assert!(rewritten(&c3, &c1, &c2, 0), "HEAD inside the dropped part");
        assert!(!rewritten(&c3, &c1, &c1, 0), "HEAD is the new tip");

        // The branch was replaced.
        assert!(rewritten(&c2, &x2, &c2, 1), "behind the new tip");
        assert!(rewritten(&c2, &x2, &dan_on_old, 1));
        assert!(!rewritten(&c2, &x2, &x2, 0), "reset to the new tip");
        assert!(
            !rewritten(&c2, &x2, &dan_on_new, 0),
            "rebased: the new tip is below Dan's commit"
        );
    }

    /// R2: counting untracked files makes no git call per file. A thousand
    /// of them (and names git quotes without `-z`) are counted by a handful
    /// of calls, whatever their number.
    #[test]
    fn untracked_files_are_counted_without_a_git_call_each() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        real_git(d, &["init", "-q", "-b", "main", "."]);
        commit(d, "README.md", "readme");
        real_git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::create_dir(d.join("many")).unwrap();
        for i in 0..2000 {
            std::fs::write(d.join(format!("many/file {i}.md")), format!("{i}")).unwrap();
        }
        for odd in ["-dash.md", "two\nlines.md", "caf\u{e9} \u{65e5}\u{672c}.md"] {
            std::fs::write(d.join(odd), "x").unwrap();
        }
        let git = Git::new(PathBuf::from("git"), d.to_path_buf());
        let started = std::time::Instant::now();
        let snap = snapshot(&git, "main").unwrap().unwrap();
        let took = started.elapsed();
        assert_eq!(snap.local.len(), 2003);
        assert!(snap.local.contains("two\nlines.md"));
        assert!(snap.local.contains("-dash.md"));
        assert!(snap.local.contains("many/file 1999.md"));
        assert!(snap.untracked && !snap.dirty);
        // ~12 calls at a few ms each; a call per file would be minutes.
        assert!(took < Duration::from_secs(5), "{took:?}");
    }
}
