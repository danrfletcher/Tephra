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

use tempfile::tempdir;

use crate::fixtures::open_temp_cache;
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
