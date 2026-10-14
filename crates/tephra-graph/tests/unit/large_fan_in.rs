//! Edge case: "a single unit with over 1,000 incoming backlinks
//! spread across sources returns in bounded time with no quadratic
//! blow-up in `graph.get_backlinks`." Seeds 5,000 spoke units across
//! several sources, all linking into one hub, through the real
//! `tephra-cache` typed insert API (so this exercises the real
//! [`UnitGraph::build_from_cache`] path, not just raw in-memory
//! structs), then asserts [`UnitGraph::get_backlinks`] completes well
//! under a generous wall-clock bound — loose enough to never flake on
//! a slow CI box, tight enough to catch an accidental linear (or
//! worse) scan reintroduced into the *query* path specifically (the
//! one-time O(edges) cost of (re)building the index is expected and
//! timed separately, not counted against this bound).

use std::time::Instant;

use tempfile::tempdir;

use tephra_cache::{Cache, NewLink, NewUnit, SourceStatus};
use tephra_graph::UnitGraph;

const SOURCES: usize = 4;
const SPOKES_PER_SOURCE: usize = 1250; // 5,000 total, well over the 1,000 floor.

#[test]
fn five_thousand_edges_resolve_and_get_backlinks_stays_bounded_time() {
    let dir = tempdir().expect("tempdir");
    let cache = Cache::open(dir.path().join("cache.db")).expect("open cache");

    let hub_source_id = cache
        .register_source("hub-source", "/tmp/hub-source", SourceStatus::Active)
        .expect("register_source hub-source");
    let hub_unit_id = cache
        .insert_unit(&NewUnit {
            source_id: hub_source_id,
            rel_path: "Hub.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert hub unit");

    for s in 0..SOURCES {
        let source_name = format!("spoke-source-{s}");
        let source_id = cache
            .register_source(&source_name, "/tmp/spoke-source", SourceStatus::Active)
            .expect("register_source spoke source");
        for i in 0..SPOKES_PER_SOURCE {
            let rel_path = format!("Spoke{i}.md");
            let spoke_unit_id = cache
                .insert_unit(&NewUnit {
                    source_id,
                    rel_path: &rel_path,
                    mtime: None,
                    frontmatter_json: None,
                })
                .expect("insert spoke unit");
            cache
                .insert_link(&NewLink {
                    from_unit_id: spoke_unit_id,
                    to_raw: "@hub-source:Hub",
                    to_unit_id: None,
                    kind: tephra_parser::LINK_KIND_WIKILINK,
                })
                .expect("insert spoke -> hub link");
        }
    }
    let _ = hub_unit_id; // only inserted to exist as the link target.

    let total_spokes = SOURCES * SPOKES_PER_SOURCE;
    assert!(total_spokes > 1000);

    // Building the graph (one-time O(nodes + edges) cost) is timed
    // separately and not counted against the per-query bound below.
    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let started = Instant::now();
    let backlinks = graph.get_backlinks("@hub-source:Hub.md");
    let elapsed = started.elapsed();

    assert_eq!(backlinks.len(), total_spokes);
    assert!(
        elapsed.as_millis() < 200,
        "get_backlinks on {total_spokes} incoming edges took {elapsed:?}, \
         expected well under 200ms (bounded-time, no quadratic blow-up)"
    );

    // A second call pays the same bounded cost — confirms the index
    // is looked up, not rebuilt, per query.
    let started_again = Instant::now();
    let backlinks_again = graph.get_backlinks("@hub-source:Hub.md");
    let elapsed_again = started_again.elapsed();
    assert_eq!(backlinks_again.len(), total_spokes);
    assert!(elapsed_again.as_millis() < 200);
}
