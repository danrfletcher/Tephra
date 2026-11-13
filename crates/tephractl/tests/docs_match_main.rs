//! The docs describe `main` as it is (G18): every RPC method, every
//! `tephractl` subcommand and every workspace crate is named, and the
//! stale wording the old docs carried is gone.
//!
//! This lives in `tephractl`'s tests rather than `tephrad`'s because it
//! reads both crates' public surfaces (`tephrad`'s closed method list and
//! `tephractl`'s usage text) and `tephractl` already depends on
//! `tephrad`. `tephractl` has no argument-parsing crate (see
//! `cli.rs`), so its command tree is the usage text that `--help` prints,
//! checked below against what `parse_args` accepts.

use std::fs;
use std::path::{Path, PathBuf};

use tephractl::cli::{parse_args, Action, USAGE};
use tephrad::rpc::dispatch::METHOD_NAMES;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn doc(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The words of every `tephractl ...` usage line up to its first
/// argument or flag: `["source", "add"]`, `["search"]`, ...
fn subcommands() -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for line in USAGE.lines() {
        let Some(rest) = line.trim_start().strip_prefix("tephractl ") else {
            continue;
        };
        let words: Vec<String> = rest
            .split_whitespace()
            .take_while(|w| {
                w.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !w.starts_with('-')
            })
            .map(str::to_string)
            .collect();
        if !words.is_empty() && !out.contains(&words) {
            out.push(words);
        }
    }
    out
}

#[test]
fn the_dispatch_closed_list_is_thirteen_and_every_method_is_in_architecture() {
    assert_eq!(METHOD_NAMES.len(), 13);
    let architecture = doc("docs/ARCHITECTURE.md");
    for method in METHOD_NAMES {
        assert!(
            architecture.contains(&format!("`{method}`")),
            "docs/ARCHITECTURE.md never names `{method}`"
        );
    }
}

#[test]
fn every_tephractl_subcommand_is_in_the_readme() {
    let subs = subcommands();
    // The tree has what the spec lists, so a parse failure of USAGE
    // cannot make this test vacuous.
    for want in [
        vec!["source", "add"],
        vec!["status"],
        vec!["link", "resolve"],
        vec!["link", "refactor"],
        vec!["ls"],
        vec!["search"],
        vec!["block-id"],
        vec!["backlinks"],
        vec!["daemon", "install"],
        vec!["daemon", "uninstall"],
        vec!["daemon", "status"],
    ] {
        assert!(subs.iter().any(|s| s == &want), "{want:?} not in {subs:?}");
    }
    let readme = doc("README.md");
    for words in subs {
        let name = words.join(" ");
        assert!(
            readme.contains(&format!("`{name}")),
            "README.md never names `{name}`"
        );
    }
    // The search forms the spec names.
    for form in ["#<query>", "^<query>", "^^<query>"] {
        assert!(readme.contains(form), "README.md never shows {form}");
    }
}

#[test]
fn the_usage_text_commands_are_ones_the_parser_accepts() {
    // `--help` is not a command the docs need; every other bare word the
    // usage text lists must be a head the parser knows (an unknown one is
    // `Err`, a known one with no arguments is `Err` too but a different
    // one than "unrecognized command").
    for words in subcommands() {
        let err = match parse_args(words.iter().map(String::as_str)) {
            Ok(Action::Help) => continue,
            Ok(Action::Run(_)) => continue,
            Err(e) => e.0,
        };
        assert!(
            !err.contains("unrecognized command"),
            "{words:?} is in the usage text but not accepted: {err}"
        );
    }
}

#[test]
fn the_docs_carry_none_of_the_stale_wording() {
    for rel in ["README.md", "docs/ARCHITECTURE.md", "docs/index.md"] {
        let text = doc(rel);
        for stale in [
            "vector/atlas-3/pr-7",
            "not yet merged",
            "Only Markdown files",
            "only Markdown files",
            "link refactoring not built",
        ] {
            assert!(!text.contains(stale), "{rel} still says {stale:?}");
        }
    }
}

#[test]
fn every_workspace_crate_is_in_the_architecture_crate_table() {
    let manifest = doc("Cargo.toml");
    let members: Vec<&str> = manifest
        .lines()
        .skip_while(|l| !l.starts_with("members"))
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with(']'))
        .filter_map(|l| {
            l.trim()
                .trim_end_matches(',')
                .trim_matches('"')
                .strip_prefix("crates/")
        })
        .collect();
    assert!(members.len() >= 5, "{members:?}");
    let architecture = doc("docs/ARCHITECTURE.md");
    let table: Vec<&str> = architecture
        .lines()
        .skip_while(|l| !l.starts_with("## Crates"))
        .take_while(|l| !l.starts_with("## How a source plugs in"))
        .collect();
    for krate in &members {
        assert!(
            table
                .iter()
                .any(|row| row.starts_with(&format!("| `{krate}` |"))),
            "the crate table has no row for {krate}"
        );
    }
    // And no row for a crate that is not a member.
    for row in table.iter().filter(|r| r.starts_with("| `")) {
        let name = row.trim_start_matches("| `").split('`').next().unwrap();
        assert!(members.contains(&name), "the crate table lists {name}");
    }
}

#[test]
fn the_docs_say_what_they_must_about_writes_and_resolution() {
    let architecture = doc("docs/ARCHITECTURE.md");
    assert!(architecture.contains("## What Tephra writes"));
    for writer in [
        "link.apply_refactor",
        "block.ensure_id",
        "duplicate block ID rename",
    ] {
        assert!(architecture.contains(writer), "no writer named {writer}");
    }
    // F7: an unprefixed link resolves only in its own source; search never
    // changes resolution.
    assert!(architecture.contains("resolves within the note's own source"));
    assert!(architecture.contains("does not feed link resolution"));
    // F2: these stay under "Not built yet".
    let not_built = architecture
        .split("## Not built yet")
        .nth(1)
        .expect("a Not built yet section");
    for item in [
        "Remote sources",
        "`source.rename`",
        "rename detection",
        "OCR",
    ] {
        assert!(not_built.contains(item), "Not built yet lacks {item}");
    }
}
