//! F5, as a fence: the event path of `link.rename_event` never guesses.
//! The two paths an event carries are the rename; nothing about the file
//! is matched by name similarity, fuzzy search, a search ranking or a
//! content hash, and the manual refactor routines are not called from
//! it (`link.preview_refactor` and `link.apply_refactor` keep behaving
//! exactly as before: their own tests are untouched).
//!
//! A plain source grep in the style of
//! `tephra-graph/tests/fence_resolver_no_search.rs`: comment lines are
//! skipped, so the module may still explain in prose what it does not do.
//! The unit tests at the bottom of a file are not part of the path.

use std::fs;
use std::path::Path;

/// The files the event path is made of: the handler and the decision.
const EVENT_PATH: &[&str] = &["src/fixes/event.rs", "src/fixes/state.rs"];

const FORBIDDEN: &[&str] = &[
    "fuzzy",
    "similar",
    "levenshtein",
    "jaro",
    "trigram",
    "sha256",
    "content_hash",
    "hash_identity",
    "units_by_identity",
    "inode",
    "rename_detect",
    "RenameDetector",
    "unit.search",
    "search_names",
    "NameHit",
    "NameCandidate",
    "compare_hits",
    "tephra_cache::search",
    "crate::search",
    "apply_refactor",
    "preview_refactor",
    "RefactorEngine",
];

fn code_only(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()));
    let production = text
        .split_once("#[cfg(test)]")
        .map_or(text.as_str(), |(p, _)| p);
    production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_event_path_calls_no_similarity_search_hash_or_manual_refactor_code() {
    for file in EVENT_PATH {
        let code = code_only(file).to_lowercase();
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(&forbidden.to_lowercase()),
                "{file} mentions `{forbidden}`: an exact rename event never guesses (F5)"
            );
        }
    }
}

#[test]
fn the_event_path_files_exist_and_are_not_empty() {
    // So the fence cannot pass by pointing at nothing.
    for file in EVENT_PATH {
        assert!(code_only(file).contains("fn "), "{file} has no code");
    }
}

#[test]
fn the_event_is_dispatched_to_the_fix_backend_alone() {
    let dispatch = code_only("src/rpc/dispatch.rs");
    let at = dispatch
        .find("fn link_rename_event")
        .expect("dispatch.rs has link_rename_event");
    let body = &dispatch[at..];
    let end = body.find("\nfn ").unwrap_or(body.len());
    let body = &body[..end];
    assert!(body.contains("fixes.rename_event"), "{body}");
    for other in ["backends.refactor", "backends.sources", "backends.graph"] {
        assert!(!body.contains(other), "the event handler reaches {other}");
    }
}
