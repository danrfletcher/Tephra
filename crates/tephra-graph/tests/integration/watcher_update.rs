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

/// R13: the previous fix made resolution itself O(1)-average per
/// link, but `recompute_scoped` still cloned every edge in the graph
/// and rebuilt every index from scratch on every call, so one edit's
/// *wall-clock* cost still grew linearly with the graph's total edge
/// count (measured: 15ms at 10k units, 122ms at 50k, 316ms at 100k —
/// roughly linear, 5 links/unit). A 4x-data/"4x+50ms" bound (the
/// previous scaling test above) cannot catch a linear regression,
/// since linear growth satisfies it too; this test uses a much
/// larger data ratio (10x) with a tight multiplicative bound a
/// linear-in-total-edges cost cannot satisfy, to actually catch the
/// regression R13 describes.
#[test]
fn applying_one_edit_does_not_grow_linearly_with_total_graph_size() {
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

        let mut graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

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

    let small = time_one_edit(10_000);
    let large = time_one_edit(100_000);

    // A cost linear in total edges would take roughly 10x longer at
    // 10x the units (100k vs 10k); an in-place scoped update (R13)
    // costs roughly the same regardless of total graph size. A tight
    // multiplicative bound (3x) catches a linear-in-total-edges
    // regression that a looser one would miss, while still leaving
    // slack for machine noise.
    assert!(
        large.as_secs_f64() < small.as_secs_f64() * 3.0 + 0.05,
        "editing one unit at 100,000 units took {large:?}, at 10,000 units took {small:?} \
         — this grows with source size, which means the per-edit cost scales with total \
         edges again, not with what the edit actually touched"
    );
    assert!(
        large.as_millis() < 150,
        "a single scoped update should stay well under 150ms even at 100,000 units: {large:?}"
    );
}

/// R10: inserting a shorter same-named candidate must pull an
/// already-resolved edge away from a longer one, matching what a
/// fresh rebuild from the same cache state would produce — not leave
/// the edge pointing at the old, no-longer-shortest, candidate.
#[test]
fn inserting_a_shorter_candidate_repoints_an_already_resolved_edge_to_match_a_fresh_rebuild() {
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
        "N.md",
        "See [[Shared]].\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamA/Shared.md",
        "# Shared A\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string())
    );

    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "Shared.md",
        "# Shared root\n",
    );
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["Shared.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:Shared.md".to_string()),
        "E4: the newly inserted shorter root candidate must win"
    );

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(
        graph.get_edges(),
        fresh.get_edges(),
        "incremental update must agree with a full rebuild from the same cache state"
    );
}

/// R10: inserting a second, equal-length candidate must turn a
/// previously unique resolution into an ambiguous (dangling) one —
/// not leave the edge pointing at its old candidate as if nothing
/// changed.
#[test]
fn inserting_a_tying_candidate_dangles_a_previously_resolved_edge_to_match_a_fresh_rebuild() {
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
        "N.md",
        "See [[Shared]].\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamA/Shared.md",
        "# Shared A\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string())
    );

    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamB/Shared.md",
        "# Shared B\n",
    );
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["teamB/Shared.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Dangling,
        "E4: an equal-length second candidate must turn a unique match into a tie"
    );

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(graph.get_edges(), fresh.get_edges());
}

/// R10: removing one half of an existing tie must let the edge
/// resolve to the one remaining candidate — not leave it dangling
/// just because it was already dangling before the removal.
#[test]
fn removing_one_half_of_a_tie_resolves_the_edge_to_the_remaining_candidate() {
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
        "N.md",
        "See [[Shared]].\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamA/Shared.md",
        "# Shared A\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamB/Shared.md",
        "# Shared B\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Dangling,
        "an equal-length tie starts out dangling"
    );

    let teamb = cache
        .get_unit_by_rel_path(source_id, "teamB/Shared.md")
        .expect("get_unit_by_rel_path")
        .expect("teamB/Shared.md exists");
    cache.delete_unit(teamb.id).expect("delete_unit");
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["teamB/Shared.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string()),
        "R10: removing one half of a tie must resolve to the remaining candidate, \
         not stay dangling"
    );

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(graph.get_edges(), fresh.get_edges());
}

/// R10: removing the shortest candidate must fall the edge back to
/// the next-shortest remaining one, rather than leaving it dangling.
#[test]
fn removing_the_shortest_candidate_falls_back_to_the_next_shortest() {
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
        "N.md",
        "See [[Shared]].\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "Shared.md",
        "# Shared root\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamA/Shared.md",
        "# Shared A\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:Shared.md".to_string())
    );

    let root = cache
        .get_unit_by_rel_path(source_id, "Shared.md")
        .expect("get_unit_by_rel_path")
        .expect("Shared.md exists");
    cache.delete_unit(root.id).expect("delete_unit");
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["Shared.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string()),
        "R10: removing the shortest candidate must fall back to the next-shortest, \
         not go dangling"
    );

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(graph.get_edges(), fresh.get_edges());
}

/// R9 (via `apply_change_notification`): re-applying a change
/// notification for the exact same, unchanged path — a watcher's
/// debounced "touch" event — must not make the path ambiguous with
/// itself. A file that later links to it must still resolve.
#[test]
fn reapplying_a_change_notification_for_the_same_path_does_not_self_ambiguate() {
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

    write_and_cache(&cache, source_id, source_dir.path(), "One.md", "# One\n");
    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");

    for _ in 0..3 {
        write_and_cache(&cache, source_id, source_dir.path(), "One.md", "# One\n");
        graph
            .apply_change_notification(
                &cache,
                &ChangeNotification {
                    source: "notes".to_string(),
                    rel_paths: vec!["One.md".to_string()],
                },
            )
            .expect("apply_change_notification");
    }

    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "Two.md",
        "# Two\n\nSee [[One]].\n",
    );
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["Two.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    let forward = graph.get_forward_links("@notes:Two.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:One.md".to_string()),
        "R9: repeated re-upserts of One.md must not make it ambiguous with itself"
    );
}

/// R12: when `apply_change_notification` observes a source's status
/// flip (Active -> Offline in the cache), it must re-annotate every
/// edge in the graph, not only the edges this particular batch's
/// scoped recompute happens to touch.
#[test]
fn apply_change_notification_offline_transition_reannotates_every_edge() {
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

    write_and_cache(&cache, source_id, source_dir.path(), "A.md", "See [[B]].\n");
    write_and_cache(&cache, source_id, source_dir.path(), "B.md", "# B\n");
    write_and_cache(&cache, source_id, source_dir.path(), "C.md", "# C\n");

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert!(!graph.get_forward_links("@notes:A.md")[0].offline);

    cache
        .transition_source_status("notes", tephra_cache::SourceStatus::Offline)
        .expect("active -> offline is a legal transition");

    // The notification only names C.md — entirely unrelated to the
    // A->B edge — the way a watcher batch for one specific file
    // change would.
    write_and_cache(&cache, source_id, source_dir.path(), "C.md", "# C still\n");
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["C.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert!(
        graph.unit("@notes:A.md").unwrap().offline,
        "A's own node metadata reflects the new status"
    );
    assert!(
        graph.get_forward_links("@notes:A.md")[0].offline,
        "R12: A->B's edge must also be re-annotated offline even though this notification \
         only named C.md"
    );
}

/// R16 (probe case B): two raw links from the same unit that
/// currently resolve to the *same* target must not permanently lose
/// one of them from storage — once a later insert makes the two
/// targets diverge, both must independently re-resolve, matching a
/// fresh `build_from_cache` on the same cache state.
#[test]
fn two_raw_links_resolving_to_the_same_target_both_survive_and_independently_re_resolve() {
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
        "N.md",
        "See [[Shared]] and [[teamA/Shared]].\n",
    );
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "teamA/Shared.md",
        "# Shared A\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    // Both raw links currently resolve to the same unit, so the
    // query-output collapses them to one row (R2/R16's query-time
    // dedup) — but the probe's bug was that one of the two raw links
    // was discarded from *storage*, not merely folded at query time.
    let forward = graph.get_forward_links("@notes:N.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:teamA/Shared.md".to_string())
    );

    // Insert a shorter root-level Shared.md: E4 says the bare
    // `[[Shared]]` link must now prefer it (shortest match), while
    // the `[[teamA/Shared]]` link (a full relative path) is
    // unaffected and must keep pointing at teamA/Shared.md — two
    // genuinely different targets now, not one.
    write_and_cache(
        &cache,
        source_id,
        source_dir.path(),
        "Shared.md",
        "# Shared root\n",
    );
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["Shared.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    let forward_after = graph.get_forward_links("@notes:N.md");
    assert_eq!(
        forward_after.len(),
        2,
        "both raw links must now be independently resolved, not just one: {forward_after:?}"
    );
    assert!(forward_after
        .iter()
        .any(|e| e.to_raw == "Shared" && e.to == EdgeTarget::Resolved("@notes:Shared.md".into())));
    assert!(forward_after.iter().any(|e| e.to_raw == "teamA/Shared"
        && e.to == EdgeTarget::Resolved("@notes:teamA/Shared.md".into())));

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(
        graph.get_edges(),
        fresh.get_edges(),
        "R16: incremental update must agree with a full rebuild from the same cache state"
    );
}

/// R16 (probe case D): a note's self-reference edge, reached via two
/// differently-spelled raw links (`[[A]]` and `[[#H]]`), must not
/// permanently discard either raw link's own record — once a shorter
/// same-named candidate is inserted elsewhere and pulls the bare-name
/// link away from the self-edge, the anchor-only link must still
/// correctly stay a self-edge, and the bare-name link must correctly
/// repoint, matching a fresh `build_from_cache`.
#[test]
fn a_self_edge_reached_via_two_raw_links_still_lets_one_of_them_independently_repoint() {
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
        "sub/A.md",
        "See [[A]] and [[#H]].\n",
    );

    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    // Before a root A.md exists, both raw links resolve to the
    // referring unit itself, so query output collapses them to one
    // self-edge row.
    let forward = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(forward.len(), 1, "{forward:?}");
    assert_eq!(
        forward[0].to,
        EdgeTarget::Resolved("@notes:sub/A.md".to_string())
    );

    // A shorter root A.md now exists: E4 says the bare `[[A]]` link
    // must repoint to it (shortest match wins), while the
    // anchor-only `[[#H]]` link always resolves to the referring
    // unit itself regardless.
    write_and_cache(&cache, source_id, source_dir.path(), "A.md", "# A root\n");
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "notes".to_string(),
                rel_paths: vec!["A.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    let forward_after = graph.get_forward_links("@notes:sub/A.md");
    assert_eq!(
        forward_after.len(),
        2,
        "the bare-name link and the anchor-only link must now resolve to different \
         targets, not stay collapsed into one stale self-edge: {forward_after:?}"
    );
    assert!(forward_after
        .iter()
        .any(|e| e.to_raw == "A" && e.to == EdgeTarget::Resolved("@notes:A.md".into())));
    assert!(forward_after
        .iter()
        .any(|e| e.to_raw == "#H" && e.to == EdgeTarget::Resolved("@notes:sub/A.md".into())));

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(
        graph.get_edges(),
        fresh.get_edges(),
        "R16: incremental update must agree with a full rebuild from the same cache state"
    );
}

/// R18 (probe case C): a federated wikilink to a source that is not
/// yet registered must still be re-resolved once that source is
/// registered and populated through `apply_change_notification` —
/// not stay dangling forever just because it was never indexed at
/// all while the target source was unknown.
#[test]
fn a_link_to_a_not_yet_registered_source_resolves_once_that_source_is_populated() {
    let notes_cache_dir = tempdir().expect("cache tempdir");
    let notes_dir = tempdir().expect("notes source tempdir");
    let repo_dir = tempdir().expect("repo source tempdir");
    let cache = open_temp_cache(&notes_cache_dir);

    let notes_id = cache
        .register_source(
            "notes",
            &notes_dir.path().to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .expect("register_source notes");

    write_and_cache(
        &cache,
        notes_id,
        notes_dir.path(),
        "N.md",
        "See [[@atlas-repo:README]].\n",
    );

    // atlas-repo does not exist in the graph yet at all.
    let mut graph = UnitGraph::build_from_cache(&cache).expect("initial build");
    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Dangling,
        "atlas-repo isn't registered yet, so the link must be dangling, not a panic \
         or a silent empty result"
    );

    // atlas-repo is registered and gets its first unit, applied
    // incrementally exactly as PR-4's registry would do it at
    // runtime after the daemon has already started.
    let repo_id = cache
        .register_source(
            "atlas-repo",
            &repo_dir.path().to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .expect("register_source atlas-repo");
    write_and_cache(&cache, repo_id, repo_dir.path(), "README.md", "# Readme\n");

    graph.register_source("atlas-repo");
    graph
        .apply_change_notification(
            &cache,
            &ChangeNotification {
                source: "atlas-repo".to_string(),
                rel_paths: vec!["README.md".to_string()],
            },
        )
        .expect("apply_change_notification");

    assert_eq!(
        graph.get_forward_links("@notes:N.md")[0].to,
        EdgeTarget::Resolved("@atlas-repo:README.md".to_string()),
        "R18: a federated link to a source registered after the graph was built must \
         still be re-resolved once that source gets its matching unit"
    );

    let fresh = UnitGraph::build_from_cache(&cache).expect("fresh rebuild");
    assert_eq!(
        graph.get_edges(),
        fresh.get_edges(),
        "the incrementally-updated graph must agree with a full rebuild from the \
         same cache state"
    );
}
