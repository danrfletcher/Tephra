//! Default ignore filters (G2, G15) plus the per-source `.tephraignore`
//! file's extra rules.
//!
//! Every source has exactly the same default ignore rules, with no
//! per-source override in v1 (G2):
//! - `.git`, `node_modules`, `target`, `.venv`, `__pycache__` and
//!   `.DS_Store`: directory (or file) names ignored at any depth, by
//!   exact component match ([`DEFAULT_IGNORED_NAMES`]).
//! - `Library` and `.Trash`: ignored only when their absolute path is
//!   a *direct child of `$HOME`* ([`HOME_ONLY_IGNORED_NAMES`]),
//!   whichever source contains them: a source rooted at `$HOME`
//!   (`~/Library`) and one rooted at `/Users` (`/Users/dan/Library`)
//!   both skip it, but a `Library` folder anywhere deeper (a vault's
//!   own `Notes/Library`) is ordinary content. `$HOME` is read once
//!   when [`IgnoreRules::load`] runs; [`IgnoreRules::load_with_home`]
//!   takes it as an argument (tests). A source whose *root* is itself
//!   `~/Library` or `~/.Trash` was registered on purpose and is
//!   scanned: the rule only prunes those folders when the walk would
//!   descend into them.
//! - `.tephraignore` itself: the rules file is never treated as a
//!   content unit (it's config, not a note).
//!
//! `.tephraignore`, if present at a source's root, adds further
//! patterns, one per non-empty/non-comment (`#`) line:
//! - A bare name with no `/` and no `*` (e.g. `drafts`) matches that
//!   exact path component at any depth, the same way the defaults
//!   above do.
//! - A pattern containing an *internal* `/` (e.g. `notes/scratch`,
//!   or `notes/scratch/` with its trailing slash stripped first) is
//!   anchored to the source root and matches that relative path or
//!   anything under it. A pattern whose only `/` was a *trailing*
//!   one (e.g. `drafts/`, a bare name with a directory marker) has
//!   no internal `/` left once that trailing slash is stripped, so
//!   it falls through to the bare-name case below instead — matching
//!   real `.gitignore` semantics, where only an internal slash
//!   anchors a pattern.
//! - A pattern containing `*` is matched with [`glob_match`] (the
//!   only wildcard supported is `*`): anchored to the full relative
//!   path if the pattern itself contains a `/`, otherwise matched
//!   against each path component (so `*.tmp` matches `scratch.tmp` at
//!   any depth).
//!
//! This is deliberately a small, documented subset of real
//! `.gitignore` semantics — enough for a handful of folder/file
//! patterns, not a general-purpose ignore engine.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Directory/file names ignored at any depth in every source, with no
/// per-source override (G2, G15).
pub const DEFAULT_IGNORED_NAMES: [&str; 6] = [
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".DS_Store",
];

/// Names ignored only when the folder's absolute path is a direct
/// child of `$HOME` (G15): macOS's `~/Library` and `~/.Trash` are huge
/// and not the user's content, but a `Library` inside a vault is.
pub const HOME_ONLY_IGNORED_NAMES: [&str; 2] = ["Library", ".Trash"];

/// The rules file read from a source's root. Its own path is always
/// ignored as a content unit (it's config, not a note/doc to parse).
pub const TEPHRAIGNORE_FILE_NAME: &str = ".tephraignore";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// No `/`, no `*`: matches this exact name at any path depth.
    NameAnywhere(String),
    /// Contains `/`, no `*`: anchored to the root; matches this exact
    /// relative path or anything nested under it.
    PathPrefix(String),
    /// Contains `*`. Matched against the whole relative path if the
    /// pattern itself contains `/`, otherwise against each path
    /// component individually.
    Glob(String),
}

impl Pattern {
    fn parse(line: &str) -> Option<Pattern> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        // Any trailing slash is a pure directory marker and is
        // stripped before classifying: once stripped, `foo/` and
        // `foo` are the same pattern. What's left is classified by
        // whether it still contains an *internal* slash — only that
        // anchors a pattern to the source root (`PathPrefix`); with
        // none left, it's a bare "match this component anywhere"
        // name (`NameAnywhere`), same as `foo` with no slash at all.
        let normalized = trimmed.trim_end_matches('/').to_string();
        if normalized.is_empty() {
            return None;
        }
        if normalized.contains('*') {
            Some(Pattern::Glob(normalized))
        } else if normalized.contains('/') {
            Some(Pattern::PathPrefix(normalized))
        } else {
            Some(Pattern::NameAnywhere(normalized))
        }
    }

    fn matches(&self, rel_str: &str, components: &[&str]) -> bool {
        match self {
            Pattern::NameAnywhere(name) => components.iter().any(|c| c == name),
            Pattern::PathPrefix(prefix) => {
                rel_str == prefix || rel_str.starts_with(&format!("{prefix}/"))
            }
            Pattern::Glob(pat) if pat.contains('/') => glob_match(pat, rel_str),
            Pattern::Glob(pat) => components.iter().any(|c| glob_match(pat, c)),
        }
    }
}

/// The complete set of ignore rules for one source: the hardcoded
/// defaults (always active, never configurable) plus whatever extra
/// patterns its `.tephraignore` file contains.
///
/// `Default` has no source root or `$HOME`, so the `$HOME`-relative
/// rule ([`HOME_ONLY_IGNORED_NAMES`]) never fires; every other default
/// still does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
    /// The source root and `$HOME`, both as the caller gave them
    /// (after a best-effort `canonicalize`, so a symlinked `/var`
    /// compares equal on both sides). `None` disables the
    /// `$HOME`-relative rule.
    home_rule: Option<HomeRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HomeRule {
    root: PathBuf,
    home: PathBuf,
}

/// `canonicalize`, falling back to the path as given when it doesn't
/// exist (a fake `$HOME` in a test, or a source that just went away).
fn canonical_or_given(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl IgnoreRules {
    /// Reads `<root>/.tephraignore` if present and parses its extra
    /// patterns. A missing file is not an error — it just means no
    /// patterns beyond the hardcoded defaults. `$HOME` comes from the
    /// environment (unset or empty disables the `$HOME`-relative
    /// rule); see [`IgnoreRules::load_with_home`] to inject it.
    pub fn load(root: &Path) -> io::Result<IgnoreRules> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        Self::load_with_home(root, home.as_deref())
    }

    /// [`IgnoreRules::load`] with `$HOME` supplied by the caller rather
    /// than read from the environment.
    pub fn load_with_home(root: &Path, home: Option<&Path>) -> io::Result<IgnoreRules> {
        let path = root.join(TEPHRAIGNORE_FILE_NAME);
        let patterns = match fs::read_to_string(&path) {
            Ok(contents) => contents.lines().filter_map(Pattern::parse).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let home_rule = home.map(|h| HomeRule {
            root: canonical_or_given(root),
            home: canonical_or_given(h),
        });
        Ok(IgnoreRules {
            patterns,
            home_rule,
        })
    }

    /// Whether some component of `components` is a
    /// [`HOME_ONLY_IGNORED_NAMES`] folder whose absolute path
    /// (`root` + the components up to and including it) is a direct
    /// child of `$HOME`. Purely lexical: nothing is read from disk, so
    /// this is cheap enough to run on every walk entry.
    fn is_home_only_ignored(&self, components: &[&str]) -> bool {
        let Some(rule) = &self.home_rule else {
            return false;
        };
        components.iter().enumerate().any(|(i, c)| {
            if !HOME_ONLY_IGNORED_NAMES.contains(c) {
                return false;
            }
            let mut abs = rule.root.clone();
            abs.extend(&components[..i]);
            // `abs` is now the *parent* of the candidate folder.
            abs == rule.home
        })
    }

    /// Whether `rel` (relative to the source root, using forward
    /// slashes) is ignored: a default name match, a `$HOME`-relative
    /// match, the `.tephraignore` file itself, or one of this source's
    /// extra patterns.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        if rel.as_os_str().is_empty() {
            return false;
        }

        let components: Vec<&str> = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();

        if components.iter().any(|c| DEFAULT_IGNORED_NAMES.contains(c)) {
            return true;
        }

        if self.is_home_only_ignored(&components) {
            return true;
        }

        if rel.file_name().and_then(|n| n.to_str()) == Some(TEPHRAIGNORE_FILE_NAME) {
            return true;
        }

        let rel_str = components.join("/");
        self.patterns
            .iter()
            .any(|p| p.matches(&rel_str, &components))
    }
}

/// Matches `text` against a simple glob `pattern` containing only the
/// `*` wildcard (zero or more of any character). No `?`, `**`,
/// character classes, or escaping — enough for a `.tephraignore`
/// pattern, not a general-purpose glob engine. Classic O(pattern *
/// text) dynamic-programming match.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (plen, tlen) = (p.len(), t.len());

    let mut dp = vec![vec![false; tlen + 1]; plen + 1];
    dp[0][0] = true;
    for i in 1..=plen {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=plen {
        for j in 1..=tlen {
            dp[i][j] = if p[i - 1] == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else if p[i - 1] == t[j - 1] {
                dp[i - 1][j - 1]
            } else {
                false
            };
        }
    }
    dp[plen][tlen]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rules_with(extra_lines: &[&str]) -> IgnoreRules {
        IgnoreRules {
            patterns: extra_lines
                .iter()
                .filter_map(|l| Pattern::parse(l))
                .collect(),
            home_rule: None,
        }
    }

    /// Table-driven: (ignore rule lines, relative path, expected).
    /// Covers the name-based default set (`.git`, `node_modules`,
    /// `target`, `.tephraignore`; the G15 additions have their own
    /// tables below) plus representative custom `.tephraignore`
    /// patterns, including nested matches.
    #[test]
    fn default_and_custom_rules_match_the_expected_paths() {
        let cases: &[(&[&str], &str, bool)] = &[
            // Defaults: exact component match at any depth.
            (&[], ".git/config", true),
            (&[], "notes/.git/HEAD", true),
            (&[], "node_modules/foo/bar.md", true),
            (&[], "src/node_modules_thing.md", false), // substring, not a real component
            (&[], "target/debug/foo", true),
            (&[], "mytarget/foo.md", false),
            (&[], ".tephraignore", true),
            (&[], "notes/one.md", false),
            // A trailing-slash-only pattern with no *internal* slash
            // (`drafts/`) still has no `/` left once the trailing
            // slash is stripped, so it's a bare "match anywhere"
            // name, same as `drafts` with no slash at all — matching
            // real `.gitignore` semantics, where only an *internal*
            // slash anchors a pattern to its root.
            (&["drafts/"], "drafts/x.md", true),
            (&["drafts/"], "drafts", true),
            (&["drafts/"], "notdrafts/x.md", false),
            (&["drafts/"], "sub/drafts/x.md", true),
            // An internal slash (`notes/drafts`) *does* anchor to the
            // source root: it matches that relative path or anything
            // under it, but not the same name nested elsewhere.
            (&["notes/drafts"], "notes/drafts", true),
            (&["notes/drafts"], "notes/drafts/x.md", true),
            (&["notes/drafts"], "sub/notes/drafts", false),
            (&["notes/drafts"], "notes/draftsother", false),
            // Bare name (no slash, no wildcard): matches anywhere.
            (&["scratch"], "scratch", true),
            (&["scratch"], "sub/scratch/file.md", true),
            (&["scratch"], "scratch2", false),
            // Wildcard pattern without a slash: matches by basename at any depth.
            (&["*.tmp"], "scratch.tmp", true),
            (&["*.tmp"], "sub/dir/scratch.tmp", true),
            (&["*.tmp"], "scratch.tmp.md", false),
            // Wildcard pattern with a slash: anchored to the full relative path.
            (&["build/*.log"], "build/out.log", true),
            (&["build/*.log"], "other/out.log", false),
            // Comments and blank lines are not patterns.
            (&["# a comment", "", "   "], "anything.md", false),
        ];

        for (extra, rel, expected) in cases {
            let rules = rules_with(extra);
            let got = rules.is_ignored(&PathBuf::from(rel));
            assert_eq!(
                got, *expected,
                "rules {extra:?} against {rel:?}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn load_missing_tephraignore_yields_empty_extra_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let rules = IgnoreRules::load(dir.path()).expect("load");
        assert!(rules.patterns.is_empty());
        assert!(!rules.is_ignored(&PathBuf::from("notes/one.md")));
        assert!(rules.is_ignored(&PathBuf::from(".git/HEAD")));
    }

    #[test]
    fn load_reads_patterns_from_tephraignore_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".tephraignore"),
            "drafts/\n*.tmp\n# comment\n",
        )
        .unwrap();
        let rules = IgnoreRules::load(dir.path()).expect("load");
        assert!(rules.is_ignored(&PathBuf::from("drafts/x.md")));
        assert!(rules.is_ignored(&PathBuf::from("scratch.tmp")));
        assert!(!rules.is_ignored(&PathBuf::from("keep.md")));
    }

    fn rules_for(root: &str, home: &str) -> IgnoreRules {
        // Neither path exists, so `canonical_or_given` keeps them as
        // written; `$HOME` is injected, never read from the env.
        IgnoreRules::load_with_home(Path::new(root), Some(Path::new(home))).expect("load")
    }

    /// G15: the new always-ignored names, at depth 0 and depth 3, and
    /// as near-miss names that must not match.
    #[test]
    fn g15_new_default_names_are_ignored_at_any_depth() {
        let rules = rules_for("/vault", "/Users/dan");
        let cases: &[(&str, bool)] = &[
            (".venv", true),
            (".venv/lib/x.py", true),
            ("a/b/c/.venv", true),
            ("a/b/c/.venv/pyvenv.cfg", true),
            ("__pycache__", true),
            ("__pycache__/m.cpython-312.pyc", true),
            ("a/b/c/__pycache__/m.pyc", true),
            (".DS_Store", true),
            ("a/b/c/.DS_Store", true),
            // Exact component match only.
            ("venv/x.py", false),
            (".venv2/x.py", false),
            ("my__pycache__/x", false),
            (".DS_Store.md", false),
            ("DS_Store", false),
            // The existing defaults are untouched.
            (".git/HEAD", true),
            ("a/node_modules/x.js", true),
            ("a/b/target/debug/x", true),
            (".tephraignore", true),
        ];
        for (rel, expected) in cases {
            assert_eq!(
                rules.is_ignored(Path::new(rel)),
                *expected,
                "{rel:?} in /vault"
            );
        }
        // And the same names are still ignored with no home known at all.
        let no_home = IgnoreRules::default();
        assert!(no_home.is_ignored(Path::new("a/b/c/.venv")));
        assert!(no_home.is_ignored(Path::new(".DS_Store")));
    }

    /// G15: `Library` and `.Trash` are ignored only as a direct child of
    /// `$HOME`, whichever source contains them. Table: (source root,
    /// `$HOME`, relative path, expected).
    #[test]
    fn g15_library_and_trash_are_ignored_only_directly_under_home() {
        let home = "/Users/dan";
        let cases: &[(&str, &str, bool)] = &[
            // Source rooted at $HOME.
            ("/Users/dan", "Library", true),
            ("/Users/dan", "Library/Caches/x.txt", true),
            ("/Users/dan", ".Trash", true),
            ("/Users/dan", ".Trash/old.md", true),
            // One level down is ordinary content.
            ("/Users/dan", "Documents/Library", false),
            ("/Users/dan", "Documents/Library/x.md", false),
            ("/Users/dan", "Documents/.Trash/x.md", false),
            ("/Users/dan", "a/b/Library", false),
            // Not an exact name.
            ("/Users/dan", "Libraries/x.md", false),
            ("/Users/dan", "Library.md", false),
            // Source rooted at $HOME's parent: dan's Library is still
            // a direct child of $HOME, another user's is not.
            ("/Users", "dan/Library", true),
            ("/Users", "dan/Library/x.txt", true),
            ("/Users", "dan/.Trash", true),
            ("/Users", "other/Library", false),
            ("/Users", "other/.Trash/x", false),
            ("/Users", "dan/Documents/Library", false),
            // Sources elsewhere: `Library` is just a folder name.
            ("/vault", "Library", false),
            ("/vault", ".Trash/x.md", false),
            ("/Users/dan/Documents/Notes", "Library/x.md", false),
            ("/Users/dan/Documents/Notes", ".Trash/x.md", false),
            // A source rooted directly in $HOME's Library was
            // registered on purpose: its own contents are not pruned.
            ("/Users/dan/Library", "Application Support/x.txt", false),
        ];
        for (root, rel, expected) in cases {
            let rules = rules_for(root, home);
            assert_eq!(
                rules.is_ignored(Path::new(rel)),
                *expected,
                "source {root:?}, rel {rel:?}, $HOME {home:?}"
            );
        }
    }

    #[test]
    fn g15_home_rule_follows_the_injected_home_not_the_environment() {
        // Same source and path, different injected $HOME.
        let under = rules_for("/srv/x", "/srv/x");
        let elsewhere = rules_for("/srv/x", "/home/someone");
        let none = IgnoreRules::load_with_home(Path::new("/srv/x"), None).unwrap();
        assert!(under.is_ignored(Path::new("Library")));
        assert!(!elsewhere.is_ignored(Path::new("Library")));
        assert!(!none.is_ignored(Path::new("Library")));
    }

    #[test]
    fn g15_home_rule_survives_symlinked_roots() {
        // On macOS a temp dir lives under the `/var` -> `/private/var`
        // symlink: a root and a `$HOME` given in different spellings
        // must still compare equal.
        let dir = tempfile::tempdir().unwrap();
        let real_home = fs::canonicalize(dir.path()).unwrap();
        let rules = IgnoreRules::load_with_home(&real_home, Some(dir.path())).unwrap();
        assert!(rules.is_ignored(Path::new("Library/Caches")));
        assert!(rules.is_ignored(Path::new(".Trash")));
        assert!(!rules.is_ignored(Path::new("Notes/Library")));
    }

    #[test]
    fn glob_match_handles_star_at_various_positions() {
        assert!(glob_match("*.md", "foo.md"));
        assert!(glob_match("foo*", "foobar"));
        assert!(glob_match("*foo*", "xxfooyy"));
        assert!(!glob_match("*.md", "foo.txt"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }
}
