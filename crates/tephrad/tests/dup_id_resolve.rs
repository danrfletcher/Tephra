//! G19: after a duplicate block ID is renamed, links still point where
//! they did. A link to the original resolves to the original's line, the
//! copy resolves as `#^summary-1`, and the original's backlinks are
//! unchanged. Real tempdir source, real cache, the watcher's real change
//! batch (which does the rename) and the real graph built from the cache
//! (what `unit.resolve_link` and `graph.get_backlinks` read).

mod fixtures;

use std::collections::HashSet;
use std::path::PathBuf;

use tephra_cache::{Cache, SourceStatus};
use tephra_graph::{ResolveResult, UnitGraph};
use tephrad::ignore::IgnoreRules;
use tephrad::scan;

fn resolve(graph: &UnitGraph, raw: &str) -> ResolveResult {
    graph
        .resolve_link(raw, Some("notes"))
        .unwrap_or_else(|e| panic!("{raw} did not resolve: {e:?}"))
}

fn block_of(result: &ResolveResult) -> (String, i64, bool) {
    let b = result.block.as_ref().expect("a block link has a block");
    (b.id.clone(), b.line, b.moved)
}

#[test]
fn after_a_rename_the_original_link_keeps_its_line_and_backlinks_and_the_copy_resolves() {
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    fixture.write(
        "Linker.md",
        "See [[@notes:Plan#^summary]].\n\nAlso [[Plan#^summary]].\n",
    );
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(cache_dir.path().join("cache.db")).unwrap();
    let root = fixture.root().canonicalize().unwrap();
    let source_id = cache
        .register_source("notes", &root.to_string_lossy(), SourceStatus::Active)
        .unwrap();
    scan::scan_source(&cache, source_id, &root, &IgnoreRules::default()).unwrap();

    let before = UnitGraph::build_from_cache(&cache).unwrap();
    let original_before = block_of(&resolve(&before, "[[@notes:Plan#^summary]]"));
    assert_eq!(original_before, ("summary".into(), 3, false));
    let backlinks_before = before.get_backlinks("@notes:Plan.md#^summary");
    assert_eq!(backlinks_before.len(), 2, "{backlinks_before:?}");

    // A copy of the paragraph is appended: Tephra renames the copy.
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n\npara ^summary\n");
    let changed: HashSet<PathBuf> = [root.join("Plan.md")].into();
    scan::apply_change_batch(&cache, source_id, &root, &changed, &IgnoreRules::default()).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("Plan.md")).unwrap(),
        "# Plan\n\npara ^summary\n\npara ^summary-1\n"
    );

    let after = UnitGraph::build_from_cache(&cache).unwrap();
    // The original link still resolves to the original line, not moved,
    // and not as an ambiguity.
    for raw in ["[[@notes:Plan#^summary]]", "[[Plan#^summary]]"] {
        assert_eq!(
            block_of(&resolve(&after, raw)),
            ("summary".into(), 3, false),
            "{raw}"
        );
    }
    // The copy resolves under its new ID, at its own line.
    assert_eq!(
        block_of(&resolve(&after, "[[@notes:Plan#^summary-1]]")),
        ("summary-1".into(), 5, false)
    );
    // The original's backlinks are exactly what they were; the copy has
    // none.
    assert_eq!(
        after.get_backlinks("@notes:Plan.md#^summary"),
        backlinks_before
    );
    assert!(after.get_backlinks("@notes:Plan.md#^summary-1").is_empty());
}

#[test]
fn an_original_that_the_copy_pushed_down_keeps_the_links_pointing_at_it() {
    // The copy lands above the original; the original is still the one
    // the cache held, so the copy is renamed and the link follows the
    // original down to its new line.
    let fixture = fixtures::empty_source();
    fixture.write("Plan.md", "# Plan\n\npara ^summary\n");
    fixture.write("Linker.md", "See [[@notes:Plan#^summary]].\n");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(cache_dir.path().join("cache.db")).unwrap();
    let root = fixture.root().canonicalize().unwrap();
    let source_id = cache
        .register_source("notes", &root.to_string_lossy(), SourceStatus::Active)
        .unwrap();
    scan::scan_source(&cache, source_id, &root, &IgnoreRules::default()).unwrap();

    fixture.write("Plan.md", "para ^summary\n\n# Plan\n\npara ^summary\n");
    let changed: HashSet<PathBuf> = [root.join("Plan.md")].into();
    scan::apply_change_batch(&cache, source_id, &root, &changed, &IgnoreRules::default()).unwrap();

    let after = UnitGraph::build_from_cache(&cache).unwrap();
    assert_eq!(
        block_of(&resolve(&after, "[[@notes:Plan#^summary]]")),
        ("summary".into(), 5, false)
    );
    assert_eq!(
        block_of(&resolve(&after, "[[@notes:Plan#^summary-1]]")),
        ("summary-1".into(), 1, false)
    );
}
