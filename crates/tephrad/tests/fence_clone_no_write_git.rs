//! PR-9 fence (F3): Tephra never writes through git in a GitHub source's
//! clone. This file is the static half: a scan of the clone module's source
//! (`src/remote/`) for any git command or flag that could change Dan's work.
//! The dynamic half (a recording `git` in every scenario, the allowlist over
//! its log, no merge commit, no token) is `remote_github_clone.rs`.
//!
//! What may run: `fetch`, `rev-parse`, `rev-list`, `merge-base`, `diff`,
//! `status`, `ls-files`, and `merge --ff-only --no-overwrite-ignore <ref>`.
//! The scan reads every string literal of the non-test code, so a command
//! named in a comment or doc does not trip it and one added to the code does.

use std::path::PathBuf;

/// Git commands that move or write Dan's work (or the clone's config), or
/// any file in it.
const FORBIDDEN_COMMANDS: [&str; 22] = [
    "commit",
    "push",
    "rebase",
    "reset",
    "checkout",
    "stash",
    "pull",
    "cherry-pick",
    "revert",
    "clean",
    "gc",
    "switch",
    "restore",
    "am",
    "apply",
    "rm",
    "mv",
    "add",
    "tag",
    "branch",
    "config",
    "update-ref",
];

/// Flags that force, discard or rewrite.
const FORBIDDEN_FLAGS: [&str; 9] = [
    "--force",
    "-f",
    "--force-with-lease",
    "--hard",
    "--no-ff",
    "--squash",
    "--autostash",
    "--allow-unrelated-histories",
    "--overwrite-ignore",
];

/// The string literals of `source`, in order, comments and the test module
/// left out. Handles escapes and raw-less literals only, which is all the
/// clone module uses.
fn literals(source: &str) -> Vec<String> {
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    let mut out = Vec::new();
    for line in code.lines() {
        let line = line.trim_start();
        if line.starts_with("//") {
            continue;
        }
        let mut chars = line.chars().peekable();
        let mut current: Option<String> = None;
        while let Some(c) = chars.next() {
            match (&mut current, c) {
                (None, '"') => current = Some(String::new()),
                (None, '/') if chars.peek() == Some(&'/') => break,
                (None, '\'') => {
                    // A char literal: skip it whole, a quote in it is no string.
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    chars.next();
                    chars.next();
                }
                (Some(_), '\\') => {
                    if let Some(next) = chars.next() {
                        current.as_mut().unwrap().push(next);
                    }
                }
                (Some(_), '"') => out.push(current.take().unwrap()),
                (Some(text), c) => text.push(c),
                _ => {}
            }
        }
    }
    out
}

/// What is wrong with `source`, as one line per problem; empty when clean.
fn problems(file: &str, source: &str) -> Vec<String> {
    let lits = literals(source);
    let mut found = Vec::new();
    for (i, lit) in lits.iter().enumerate() {
        if FORBIDDEN_COMMANDS.contains(&lit.as_str()) {
            found.push(format!("{file}: runs or names `git {lit}`"));
        }
        if FORBIDDEN_FLAGS.contains(&lit.as_str()) || lit.starts_with("--force") {
            found.push(format!("{file}: uses the flag `{lit}`"));
        }
        // A merge is only ever `merge --ff-only --no-overwrite-ignore <ref>`.
        if lit == "merge"
            && (lits.get(i + 1).map(String::as_str) != Some("--ff-only")
                || lits.get(i + 2).map(String::as_str) != Some("--no-overwrite-ignore"))
        {
            found.push(format!(
                "{file}: `merge` without `--ff-only --no-overwrite-ignore`"
            ));
        }
    }
    found
}

fn remote_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/remote");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("src/remote exists") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            files.push((name, std::fs::read_to_string(&path).unwrap()));
        }
    }
    files
}

#[test]
fn the_clone_module_runs_no_git_command_that_writes() {
    let files = remote_sources();
    assert!(
        files.iter().any(|(n, _)| n == "clone_state.rs")
            && files.iter().any(|(n, _)| n == "clone_sync.rs"),
        "the scan found the clone module"
    );
    let mut all = Vec::new();
    for (name, source) in &files {
        all.extend(problems(name, source));
    }
    assert!(all.is_empty(), "{}", all.join("\n"));
}

#[test]
fn git_is_started_in_one_place_only_and_only_through_the_allowlist() {
    for (name, source) in remote_sources() {
        let code = source.split("#[cfg(test)]").next().unwrap();
        let starts = code.matches("Command::new").count();
        if name == "clone_state.rs" {
            assert_eq!(starts, 1, "`Git::run` is the one place a process starts");
            // ...and it checks the allowlist before it does.
            let run = code.split("pub fn run(").nth(1).expect("Git::run");
            assert!(
                run.find("allowed(args)").unwrap() < run.find("Command::new").unwrap(),
                "the allowlist is checked first"
            );
        } else {
            assert_eq!(starts, 0, "{name} starts a process");
        }
    }
}

#[test]
fn the_scan_does_catch_what_it_is_for() {
    for bad in [
        r#"git.run(&["commit", "-m", "x"]);"#,
        r#"git.run(&["push", "origin"]);"#,
        r#"git.run(&["merge", "origin/main"]);"#,
        r#"git.run(&["merge", "--ff-only", "refs/remotes/origin/main"]);"#,
        r#"git.run(&["fetch", "--force", "origin"]);"#,
        r#"git.run(&["fetch", "--force-with-lease"]);"#,
        r#"git.run(&["reset", "--hard"]);"#,
        r#"git.run(&["pull", "--rebase"]);"#,
        r#"git.run(&["checkout", "main"]);"#,
        r#"git.run(&["stash"]);"#,
        r#"git.run(&["rebase", "main"]);"#,
    ] {
        assert!(!problems("x.rs", bad).is_empty(), "{bad} was not caught");
    }
    for fine in [
        r#"git.run(&["merge", "--ff-only", "--no-overwrite-ignore", &r]);"#,
        r#"git.run(&["fetch", "--no-tags", "origin", &spec]);"#,
        r#"git.run(&["rev-parse", "--verify", "-q", &spec]);"#,
        "// git.run(&[\"push\"]) in a comment\nlet a = 1;",
        "let a = 1;\n#[cfg(test)]\nmod t { fn f() { let _ = \"push\"; } }",
    ] {
        assert!(problems("x.rs", fine).is_empty(), "{fine} was flagged");
    }
}
