//! Fence regression checks on the spike scripts (PR-2): a table-driven scan
//! of `spikes/` for every forbidden pattern in the F2, F3 and F9 lines, plus
//! F9 over `docs/spikes/` too.
//!
//! - F2: no Drive delete, bin, move or rename request: `files.delete`,
//!   `trashed`, `addParents`, `removeParents` (and the bin-emptying call and
//!   a bare quoted `DELETE` method, which would be the same thing).
//! - F3: no `git push`, `--force`, `rebase`, `merge` or `pull`, and git only
//!   against a throwaway clone (every script that clones makes its directory
//!   with `mktemp`).
//! - F9: no token on disk: no `ghp_`, `github_pat_`, `ya29.` or
//!   `refresh_token` value. A bare key name is fine; a value is a run of 20 or
//!   more token characters after it.
//!
//! The matchers are plain functions with their own positive and negative
//! cases, so a regression in the scan fails a test too. No regex crate.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Every text file under `dir`, skipping build output, run output and the
/// downloaded binaries (`.bin`, `out`, `target`, `.git`).
fn text_files(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if matches!(name.as_str(), ".bin" | "out" | "target" | ".git") {
                continue;
            }
            text_files(&path, out);
        } else if let Ok(text) = fs::read_to_string(&path) {
            out.push((path, text));
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// True when `word` occurs in `text` with no word character on either side.
fn has_word(text: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = text[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// True when some line has both `git` and `push` as whole words (covers
/// `git push`, `git -C dir push`, `g push`-style wrappers on the same line).
fn has_git_push(text: &str) -> bool {
    text.lines()
        .any(|l| has_word(l, "git") && has_word(l, "push"))
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.'
}

/// True when `prefix` is followed by at least `min` token characters.
fn has_prefixed_token(text: &str, prefix: &str, min: usize) -> bool {
    let mut from = 0;
    while let Some(pos) = text[from..].find(prefix) {
        let after = from + pos + prefix.len();
        let run = text[after..].chars().take_while(|c| token_char(*c)).count();
        if run >= min {
            return true;
        }
        from = after;
    }
    false
}

/// True when `refresh_token` is followed (after optional quotes and spaces)
/// by `:` or `=` and then a value of 20 or more token characters.
fn has_refresh_token_value(text: &str) -> bool {
    let key = "refresh_token";
    let mut from = 0;
    while let Some(pos) = text[from..].find(key) {
        let after = from + pos + key.len();
        let rest = text[after..].trim_start_matches(['"', '\'', ' ']);
        if let Some(value) = rest.strip_prefix([':', '=']) {
            let value = value.trim_start_matches(['"', '\'', ' ']);
            if value.chars().take_while(|c| token_char(*c)).count() >= 20 {
                return true;
            }
        }
        from = after;
    }
    false
}

struct Rule {
    fence: &'static str,
    name: &'static str,
    matches: fn(&str) -> bool,
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

/// Rules scanned over `spikes/` (F2, F3) and, for the F9 rows, also over
/// `docs/spikes/`.
fn rules() -> Vec<Rule> {
    vec![
        Rule {
            fence: "F2",
            name: "files.delete",
            matches: |t| t.contains("files.delete"),
        },
        Rule {
            fence: "F2",
            name: "trashed",
            matches: |t| t.contains("trashed"),
        },
        Rule {
            fence: "F2",
            name: "addParents",
            matches: |t| t.contains("addParents"),
        },
        Rule {
            fence: "F2",
            name: "removeParents",
            matches: |t| t.contains("removeParents"),
        },
        Rule {
            fence: "F2",
            name: "emptyTrash",
            matches: |t| t.contains("emptyTrash"),
        },
        Rule {
            fence: "F2",
            name: "quoted DELETE method",
            matches: |t| contains_any(t, &["\"DELETE\"", "'DELETE'"]),
        },
        Rule {
            fence: "F3",
            name: "git push",
            matches: has_git_push,
        },
        Rule {
            fence: "F3",
            name: "--force",
            matches: |t| t.contains("--force"),
        },
        Rule {
            fence: "F3",
            name: "rebase",
            matches: |t| has_word(t, "rebase"),
        },
        Rule {
            fence: "F3",
            name: "merge",
            matches: |t| has_word(t, "merge"),
        },
        Rule {
            fence: "F3",
            name: "pull",
            matches: |t| has_word(t, "pull"),
        },
        Rule {
            fence: "F9",
            name: "ghp_ token",
            matches: |t| has_prefixed_token(t, "ghp_", 10),
        },
        Rule {
            fence: "F9",
            name: "github_pat_ token",
            matches: |t| has_prefixed_token(t, "github_pat_", 10),
        },
        Rule {
            fence: "F9",
            name: "ya29. token",
            matches: |t| has_prefixed_token(t, "ya29.", 10),
        },
        Rule {
            fence: "F9",
            name: "refresh_token value",
            matches: has_refresh_token_value,
        },
    ]
}

#[test]
fn spike_scripts_contain_no_forbidden_pattern() {
    let root = root();
    let mut spikes = Vec::new();
    text_files(&root.join("spikes"), &mut spikes);
    assert!(
        spikes.len() >= 12,
        "expected the spike scripts and READMEs, found {} files",
        spikes.len()
    );
    let mut docs = Vec::new();
    text_files(&root.join("docs/spikes"), &mut docs);

    let mut offenders = Vec::new();
    for rule in rules() {
        let scanned: Vec<&(PathBuf, String)> = if rule.fence == "F9" {
            spikes.iter().chain(docs.iter()).collect()
        } else {
            spikes.iter().collect()
        };
        for (path, text) in scanned {
            if (rule.matches)(text) {
                offenders.push(format!("{} {}: {}", rule.fence, rule.name, path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "forbidden patterns:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_script_that_clones_makes_a_throwaway_directory_first() {
    let root = root();
    let mut spikes = Vec::new();
    text_files(&root.join("spikes"), &mut spikes);
    let mut cloners = 0;
    let mut offenders = Vec::new();
    for (path, text) in &spikes {
        if path.extension().is_some_and(|e| e == "sh") && text.contains("clone ") {
            // "git clone" as a command, not as prose in a comment
            let runs_clone = text.lines().any(|l| {
                let t = l.trim_start();
                !t.starts_with('#') && t.contains("git clone")
            });
            if !runs_clone {
                continue;
            }
            cloners += 1;
            if !(text.contains("mktemp") || text.contains("spike_mktemp")) {
                offenders.push(path.display().to_string());
            }
        }
    }
    assert!(
        cloners >= 2,
        "expected git-half.sh and s3/run.sh to clone, found {cloners} scripts"
    );
    assert!(
        offenders.is_empty(),
        "F3: scripts that clone must work in a mktemp directory: {offenders:?}"
    );
}

#[test]
fn spike_scripts_read_tokens_only_through_security() {
    // F9: every credential read goes through the `security` tool, and no
    // script reads a token from the environment or a file.
    let root = root();
    let mut spikes = Vec::new();
    text_files(&root.join("spikes"), &mut spikes);
    let mut offenders = Vec::new();
    for (path, text) in &spikes {
        let name = path.file_name().unwrap().to_string_lossy();
        let is_script = matches!(path.extension().and_then(|e| e.to_str()), Some("sh" | "py"));
        if !is_script {
            continue;
        }
        for needle in [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "gh auth token",
            "GOOGLE_ACCESS_TOKEN",
            "os.environ[\"TOKEN\"]",
        ] {
            if text.contains(needle) {
                offenders.push(format!("{name}: mentions {needle}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "tokens must come from the Keychain only: {offenders:?}"
    );
}

#[test]
fn the_matchers_flag_what_they_should_and_nothing_else() {
    let by_name = |n: &str| {
        rules()
            .into_iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("no rule {n}"))
    };
    // (rule, text, should flag)
    let cases: &[(&str, &str, bool)] = &[
        ("files.delete", "api.files.delete(id)", true),
        ("files.delete", "files.list", false),
        ("trashed", "q = \"trashed = false\"", true),
        ("trashed", "send it to the bin", false),
        ("addParents", "params addParents=x", true),
        ("removeParents", "removeParents=y", true),
        ("quoted DELETE method", "call(\"DELETE\", url)", true),
        ("quoted DELETE method", "delete-me.txt", false),
        ("git push", "git push origin main", true),
        ("git push", "git -C \"$d\" push -q", true),
        ("git push", "to push a token, ask Dan", false),
        ("git push", "git fetch -q origin", false),
        ("--force", "git clone --force", true),
        ("--force", "force-pushed", false),
        ("rebase", "git rebase main", true),
        ("rebase", "unrebased", false),
        ("merge", "git merge x", true),
        ("merge", "emerged", false),
        ("pull", "git pull", true),
        ("pull", "pulled in", false),
        ("pull", "pull-request", true),
        ("ghp_ token", "ghp_abcdefghij1234567890", true),
        ("ghp_ token", "the ghp_ prefix", false),
        ("github_pat_ token", "github_pat_11ABCDEFG0abcdefghij", true),
        ("github_pat_ token", "github_pat_", false),
        ("ya29. token", "ya29.a0AfH6SMBabcdefghij", true),
        ("ya29. token", "ya29.", false),
        (
            "refresh_token value",
            "\"refresh_token\": \"1//0gAbCdEfGhIjKlMnOpQrStUv\"",
            true,
        ),
        (
            "refresh_token value",
            "refresh_token=1//0gAbCdEfGhIjKlMnOpQrStUv",
            true,
        ),
        (
            "refresh_token value",
            "\"refresh_token\": cred[\"refresh_token\"]",
            false,
        ),
        ("refresh_token value", "grant_type=refresh_token", false),
        ("refresh_token value", "the refresh_token field", false),
    ];
    for (rule, text, expected) in cases {
        let got = (by_name(rule).matches)(text);
        assert_eq!(got, *expected, "rule {rule:?} on {text:?}");
    }
}
