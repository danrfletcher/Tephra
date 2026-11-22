//! G26 and F2, as a fence: the fix writer shares `compute_changes` and
//! `apply_changes_to_content` with the manual refactor but never calls
//! the manual refactor's apply routine (its 400 ms settle sleep, its full
//! source rescan, its mtime-only up-front check), and the RPC method list
//! in `rpc/dispatch.rs` is exactly today's.
//!
//! Reads the source text of the writer module, so it fails the build of
//! anything that quietly starts depending on what must not be reused.

use std::fs;
use std::path::Path;

fn manifest(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The writer's source above its unit tests (the tests name things the
/// writer must not use, to check them).
fn writer_source() -> String {
    let text = manifest("src/refactor/fix_write.rs");
    let (production, _tests) = text
        .split_once("#[cfg(test)]")
        .expect("fix_write.rs has a unit test module");
    production.to_string()
}

#[test]
fn g26_the_writer_never_references_the_manual_apply_routine() {
    let src = writer_source();
    for forbidden in [
        "RefactorEngine::apply",
        ".apply(",
        "verify_staleness",
        ".rescan(",
        "DEBOUNCE_WINDOW",
        "from_millis(400)",
        "from_millis(550)",
        "pause_all",
        "resume_all",
        "previews",
    ] {
        assert!(
            !src.contains(forbidden),
            "fix_write.rs must not use `{forbidden}` (G26)"
        );
    }
}

#[test]
fn g26_the_writer_has_exactly_one_sleep_and_it_is_the_injectable_one() {
    let src = writer_source();
    assert_eq!(
        src.matches("thread::sleep").count(),
        1,
        "the only real sleep is ThreadSleeper's"
    );
    let at = src.find("thread::sleep").unwrap();
    let before = &src[..at];
    assert!(
        before.rfind("impl Sleeper for ThreadSleeper").is_some(),
        "that sleep must be inside `impl Sleeper for ThreadSleeper`"
    );
}

#[test]
fn g26_the_writer_shares_the_pure_span_functions_and_the_locks_with_the_refactor() {
    let src = writer_source();
    for needed in [
        "compute_changes",
        "compute_fix_changes",
        "apply_changes_to_content",
        "file_lock::lock",
        "self.gate.lock()",
        "writer_gate()",
    ] {
        assert!(src.contains(needed), "fix_write.rs should use `{needed}`");
    }
    // The gate is the engine's own, not a second lock of the writer's.
    let engine = manifest("src/refactor/mod.rs");
    assert!(
        engine.contains("pub fn writer_gate"),
        "engine exposes its gate"
    );
    assert!(
        engine.contains("let _apply_guard = self.apply_lock.lock()"),
        "apply still takes the very lock the writer shares"
    );
}

/// Every `"a.b" =>` arm of `dispatch`'s match.
fn dispatch_methods() -> Vec<String> {
    let text = manifest("src/rpc/dispatch.rs");
    let start = text
        .find("pub fn dispatch(")
        .expect("dispatch function in dispatch.rs");
    let body = &text[start..];
    let end = body.find("other =>").expect("catch-all arm of dispatch");
    let mut out = Vec::new();
    for line in body[..end].lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('"') {
            if let Some((name, tail)) = rest.split_once('"') {
                if tail.trim_start().starts_with("=>") {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn f2_the_rpc_method_list_is_the_one_before_this_pr_plus_fix_list_rename_event_promote_unpromote_and_rename(
) {
    let mut expected: Vec<String> = [
        "unit.resolve_link",
        "graph.get_backlinks",
        "graph.get_forward_links",
        "graph.get_edges",
        "source.register",
        "source.unregister",
        "source.list",
        "source.rescan",
        // atlas-25 PR-12 (G11, F12): the one method that PR adds.
        "source.promote",
        // atlas-25 PR-13 (G12, F12): the one method that PR adds.
        "source.unpromote",
        // atlas-25 PR-14 (G13, F12): the one method that PR adds.
        "source.rename",
        "link.preview_refactor",
        "link.apply_refactor",
        "unit.list",
        "unit.search",
        "block.ensure_id",
        // atlas-25 PR-5 (G18, F12): the one method that PR adds.
        "fix.list",
        // atlas-25 PR-8 (G3, F12): the one method that PR adds.
        "link.rename_event",
        // atlas-27 PR-5: the one method that PR adds.
        "source.add_github",
        // atlas-27 PR-7: the one method that PR adds.
        "unit.fetch",
        // atlas-27 PR-8: the three methods that PR adds.
        "cache.status",
        "cache.set_limit",
        "source.set_cache",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(dispatch_methods(), expected);
}

#[test]
fn f2_the_manual_path_still_requires_a_preview_before_apply() {
    // The apply routine's own refusal text is still in place.
    let engine = manifest("src/refactor/mod.rs");
    assert!(engine.contains("no prior preview_refactor call for source="));
    assert!(engine.contains("call preview_refactor again before apply_refactor"));
}
