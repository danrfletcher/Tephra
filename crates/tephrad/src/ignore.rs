//! Default ignore filters (G2) plus the per-source `.tephraignore`
//! file's extra rules.
//!
//! Every source has exactly the same four default ignore rules, with
//! no per-source override in v1 (G2):
//! - `.git`, `node_modules`, `target`: directory (or file) names
//!   ignored at any depth, by exact component match.
//! - `.tephraignore` itself: the rules file is never treated as a
//!   content unit (it's config, not markdown).
//!
//! `.tephraignore`, if present at a source's root, adds further
//! patterns, one per non-empty/non-comment (`#`) line:
//! - A bare name with no `/` and no `*` (e.g. `drafts`) matches that
//!   exact path component at any depth, the same way the four
//!   defaults above do.
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
use std::path::Path;

/// Directory/file names ignored at any depth in every source, with no
/// per-source override (G2).
pub const DEFAULT_IGNORED_NAMES: [&str; 3] = [".git", "node_modules", "target"];

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

/// The complete set of ignore rules for one source: the four
/// hardcoded defaults (always active, never configurable) plus
/// whatever extra patterns its `.tephraignore` file contains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

impl IgnoreRules {
    /// Reads `<root>/.tephraignore` if present and parses its extra
    /// patterns. A missing file is not an error — it just means no
    /// patterns beyond the four hardcoded defaults.
    pub fn load(root: &Path) -> io::Result<IgnoreRules> {
        let path = root.join(TEPHRAIGNORE_FILE_NAME);
        let patterns = match fs::read_to_string(&path) {
            Ok(contents) => contents.lines().filter_map(Pattern::parse).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(IgnoreRules { patterns })
    }

    /// Whether `rel` (relative to the source root, using forward
    /// slashes) is ignored: a default name match, the `.tephraignore`
    /// file itself, or one of this source's extra patterns.
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
        }
    }

    /// Table-driven: (ignore rule lines, relative path, expected).
    /// Covers the full default set (`.git`, `node_modules`, `target`,
    /// `.tephraignore`) plus representative custom `.tephraignore`
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
