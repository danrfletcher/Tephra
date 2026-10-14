//! `tests/integration/watcher_update.rs` (spec: "feed a simulated
//! PR-4 change-notification event (file edited) into `UnitGraph`'s
//! incremental-update path; confirm edges reflect the new content
//! without a full graph rebuild"). Uses the same real
//! `tephra-cache`/`tephra-parser` pipeline as `federation.rs`, via a
//! [`tephra_graph::ChangeNotification`] the caller (a later PR's
//! watcher) is responsible for deriving from a real
//! `tephrad::registry::DaemonEvent::SourceChanged` — this crate
//! never touches the filesystem directly and has no dependency on
//! `tephrad` (F3).

use std::path::Path;
use std::time::Instant;

use tempfile::tempdir;

use crate::fixtures::open_temp_cache;
use tephra_cache::{Cache, NewLink, NewUnit, SourceStatus};
use tephra_graph::{ChangeNotification, EdgeTarget, UnitGraph};

/// Writes `rel_path` under `source_root` with `content`, inserting
/// (or, on a second call for the same path, replacing) its row in
/// `cache` directly — standing in for a real file write followed by
/// PR-4's watcher noticing and re-parsing it. Returns nothing; the
/// test drives the actual update through
/// [`UnitGraph::apply_change_notification`], not through this helper.
fn write_and_cache(
    cache: &tephra_cache::Cache,
    source_id: i64,
    source_root: &Path,
    rel_path: &str,
    content: &str,
) {
    let abs_path = source_root.join(rel_path);
    std::fs::create_dir_all(abs_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs_path, content).expect("write fixture file");

    let parsed =
        tephra_parser::parse_unit_bytes(&abs_path, content.as_bytes()).expect("valid UTF-8");

    if let Some(existing) = cache
        .get_unit_by_rel_path(source_id, rel_path)
        .expect("get_unit_by_rel_path")
    {
        cache.delete_unit(existing.id).expect("delete_unit");
    }

    let unit_id = cache
        .insert_unit(&parsed.to_new_unit(source_id, rel_path, None))
        .expect("insert_unit");
    for link in parsed.to_new_links(unit_id) {
        cache.insert_link(&link).expect("insert_link");
    }
    for tag in parsed.to_new_tags(unit_id) {
        cache.insert_tag(&tag).expect("insert_tag");
    }
    for block in parsed.to_new_blocks(unit_id) {
        cache.insert_block(&block).expect("insert_block");
    }
}

#[test]
fn applying_a_change_notification_upserts_without_a_full_rebuild() {
    let cache_dir = tempdir().expect("cache tempdir");
    let source_dir = tempdir().expect("source tempdir");
    let cache = open_temp_cache(&cache_dir);

    let source_id = cache
        .register_source(
            "notes",
            &source_dir.path().to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .expect("register_source");

    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "One.md",
        "# One\n\nNo links yet.\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert!(graph.unit("@notes:One.md").is_some());
    assert!(graph.unit("@notes:Two.md").is_none());
    assert!(graph.get_forward_links("@notes:One.md").is_empty());

    // Simulate a real edit: One.md is rewritten to link to a
    // brand-new Two.md, and Two.md is created — both "files changed"
    // in the same notification, as a watcher's debounced batch would
    // report them together.
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "One.md",
        "# One\n\nSee [[Two]].\n",
    );
    write_and_cache(&cache, source_id, source_dir.path(), "Two.md", "# Two\n");

    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["One.md".to_string(), "Two.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert!(graph.unit("@notes:Two.md").is_some());
    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:Two.md".to_string())
    );
    let backlinks = graph.get_backlinks("@notes:Two.md");
    assert_eq!(backlinks.len(), 1);
}

#[test]
fn applying_a_change_notification_for_a_deleted_file_removes_its_node_and_dangles_its_backlinks() {
    let cache_dir = tempdir().expect("cache tempdir");
    let source_dir = tempdir().expect("source tempdir");
    let cache = open_temp_cache(&cache_dir);

    let source_id = cache
        .register_source(
            "notes",
            &source_dir.path().to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .expect("register_source");

    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "One.md",
        "# One\n\nSee [[Two]].\n",
    );
    write_and_cache(&cache, source_id, source_dir.path(), "Two.md", "# Two\n");

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:One.md")[0].to,
        EdgeTarget::Resolved("@notes:Two.md".to_string())
    );

    // Two.md is deleted on disk and removed from the cache; the
    // notification names only the path that changed.
    std::fs::remove_file(source_dir.path().join("Two.md")).expect("remove file");
    let existing = cache
        .get_unit_by_rel_path(source_id, "Two.md")
        .expect("get_unit_by_rel_path")
        .expect("row still present before delete");
    cache.delete_unit(existing.id).expect("delete_unit");

    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["Two.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert!(graph.unit("@notes:Two.md").is_none());
    let forward = graph.get_forward_links("@notes:One.md");
    assert_eq!(forward.len(), 1);
    assert_eq!(forward[0].to, EdgeTarget::Dangling);
}

#[test]
fn apply_change_notification_rejects_an_unregistered_source_name() {
    let cache_dir = tempdir().expect("cache tempdir");
    let cache = open_temp_cache(&cache_dir);
    let mut graph = UnitGraph::new();

    let result = graph.apply_change_notification(
        &cache,
        &ChangeNotification {
            source: "ghost".to_string(),
            rel_paths: vec!["Anything.md".to_string()],
        },
    );
    assert!(matches!(
        result,
        Err(tephra_graph::GraphError::UnknownSource(_))
    ));
}

/// R3: `apply_change_notification`'s incremental update is scoped to
/// the changed unit's own links plus whatever already-dangling edges
/// might now resolve to it — not a full re-resolution of every link
/// in the source. A full rebuild (the pre-fix behaviour) costs
/// quadratically more as the source grows (the probe measured 0.63s
/// at 2,000 units, 2.7s at 4,000, 11.1s at 8,000, with 5 links per
/// unit); a properly scoped update costs roughly the same regardless
/// of source size.
#[test]
fn applying_one_edit_in_a_large_source_recomputes_in_roughly_constant_time_not_proportional_to_source_size(
) {
    fn seed(cache: &Cache, source_id: i64, n: usize) {
        for i in 0..n {
            let rel_path = format!("Unit{i}.md");
            let unit_id = cache
                .insert_unit(&NewUnit {
                    source_id,
                    rel_path: &rel_path,
                    mtime: None,
                    frontmatter_json: None,
                })
                .expect("insert unit");
            for k in 1..=5 {
                let target = format!("Unit{}", (i + k) % n);
                cache
                    .insert_link(&NewLink {
                        from_unit_id: unit_id,
                        to_raw: &target,
                        to_unit_id: None,
                        kind: tephra_parser::LINK_KIND_WIKILINK,
                    })
                    .expect("insert link");
            }
        }
    }

    fn time_one_edit(n: usize) -> std::time::Duration {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open(dir.path().join("cache.db")).expect("open cache");
        let source_id = cache
            .register_source("notes", "/tmp/notes", SourceStatus::Active)
            .expect("register_source");
        seed(&cache, source_id, n);

        // Building the full graph (one-time O(nodes + edges) cost) is
        // not counted against the per-edit bound below.
        let mut graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

        // Edit Unit0's own content: same scenario the probe measured
        // as quadratic before the fix ("editing one file").
        let existing = cache
            .get_unit_by_rel_path(source_id, "Unit0.md")
            .expect("get_unit_by_rel_path")
            .expect("Unit0.md exists");
        cache.delete_unit(existing.id).expect("delete_unit");
        let new_unit_id = cache
            .insert_unit(&NewUnit {
                source_id,
                rel_path: "Unit0.md",
                mtime: None,
                frontmatter_json: None,
            })
            .expect("re-insert unit");
        for k in 1..=5 {
            let target = format!("Unit{}", k % n);
            cache
                .insert_link(&NewLink {
                    from_unit_id: new_unit_id,
                    to_raw: &target,
                    to_unit_id: None,
                    kind: tephra_parser::LINK_KIND_WIKILINK,
                })
                .expect("insert link");
        }

        let started = Instant::now();
        graph
            .apply_change_notification(
                &cache,
                &ChangeNotification {
                    source: "notes".to_string(),
                    rel_paths: vec!["Unit0.md".to_string()],
                },
            )
            .expect("apply_change_notification");
        started.elapsed()
    }

    let small = time_one_edit(2_000);
    let large = time_one_edit(8_000);

    // A quadratic rebuild would cost roughly 16x more at 4x the
    // units; a properly scoped update costs roughly the same
    // regardless of source size. Generous multiplicative bound (plus
    // a fixed slack term) to avoid flaking on a slow CI box while
    // still catching a regression back to a full, O(units-in-source)
    // rebuild per edit.
    assert!(
        large.as_secs_f64() < small.as_secs_f64() * 4.0 + 0.05,
        "editing one unit at 8,000 units took {large:?}, at 2,000 units took {small:?} \
         — expected roughly constant time, not scaling with source size"
    );
    assert!(
        large.as_millis() < 100,
        "a single scoped update should stay well under 100ms even at 8,000 units: {large:?}"
    );
}
