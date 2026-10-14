//! Tests: R17 — `links(from_unit_id)`, `links(to_unit_id)`,
//! `tags(unit_id)` and `blocks(unit_id)` must be indexed, so a
//! per-file delete (`delete_unit`, and the same subqueries in
//! `delete_units_for_source`/`delete_units_under_prefix`) costs
//! roughly the size of *that file's own* rows, not a full scan of
//! every link/tag/block in the whole cache across every source.
//!
//! Before this fix, `schema.rs` created no indexes at all: every one
//! of `delete_unit`'s four cascading statements
//! (`UPDATE/DELETE ... WHERE {from,to}_unit_id = ?1` on `links`,
//! `WHERE unit_id = ?1` on `tags`/`blocks`) was a full table scan. The
//! review's probe measured this directly against a 100k-file source:
//! 1 modified file cost 8.6ms, 100 cost 0.86s, 1000 cost 8.2s, and
//! 1000 deleted files cost 13.8s — scaling with the *cache's* total
//! size, not with the size of the batch being applied, which is
//! exactly what turns an ordinary `git checkout`-sized burst (E5,
//! ~1000 files) into a write-lock hold far past the 5-second
//! `busy_timeout`, stalling every other source's watcher and the
//! registry's own `list`/`register`/`rescan` calls (GP6).

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tephra_cache::{Cache, NewBlock, NewLink, NewTag, NewUnit, SourceStatus};

/// Confirms SQLite's own query planner actually uses an index for
/// each of `delete_unit`'s four statements — not just that an index
/// with the right name exists, but that it's the one actually chosen
/// for exactly the query shape `delete_unit` issues. Deterministic
/// and fast: no large fixture, no timing, so this is the primary
/// proof for this review finding; the timing-based test below is a
/// secondary, best-effort sanity check on top of it.
#[test]
fn delete_unit_queries_use_an_index_not_a_full_table_scan() {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");
    let conn = cache.connection();

    let checks: &[(&str, &str)] = &[
        (
            "links by to_unit_id (the UPDATE ... SET to_unit_id = NULL step)",
            "UPDATE links SET to_unit_id = NULL WHERE to_unit_id = 1",
        ),
        (
            "links by from_unit_id",
            "DELETE FROM links WHERE from_unit_id = 1",
        ),
        ("tags by unit_id", "DELETE FROM tags WHERE unit_id = 1"),
        ("blocks by unit_id", "DELETE FROM blocks WHERE unit_id = 1"),
    ];

    for (label, sql) in checks {
        let plan = explain_query_plan(conn, sql);
        let plan_upper = plan.to_uppercase();
        assert!(
            plan_upper.contains("INDEX") && !plan_upper.contains("SCAN"),
            "{label} must use an index, not a full table scan (R17); \
             EXPLAIN QUERY PLAN for `{sql}` was: {plan}"
        );
    }
}

fn explain_query_plan(conn: &Connection, sql: &str) -> String {
    let full = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = conn.prepare(&full).expect("prepare EXPLAIN QUERY PLAN");
    let mut rows = stmt.query([]).expect("query EXPLAIN QUERY PLAN");
    let mut details = Vec::new();
    while let Some(row) = rows.next().expect("step EXPLAIN QUERY PLAN") {
        // Columns are (id, parent, notused, detail); `detail` is the
        // human-readable plan fragment ("SEARCH links USING INDEX
        // idx_links_from_unit_id (from_unit_id=?)" vs "SCAN links").
        let detail: String = row.get(3).expect("detail column");
        details.push(detail);
    }
    details.join("; ")
}

/// Best-effort timing sanity check: deleting a bounded batch of units
/// belonging to one source stays fast even when the cache also holds
/// a large number of *other* sources' links/tags/blocks rows that an
/// unindexed scan would have to wade through on every single one of
/// the batch's deletes. Not the primary proof (the exact-plan check
/// above is), and deliberately generous on the bound to avoid
/// flaking on a slow CI machine — this only needs to catch a true
/// O(cache size) regression, which would be far slower than this
/// bound even accounting for machine variance.
#[test]
fn deleting_a_batch_from_a_large_cache_stays_fast() {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let noise_source_id = cache
        .register_source("noise", "/noise", SourceStatus::Active)
        .expect("register noise source");
    let target_source_id = cache
        .register_source("target", "/target", SourceStatus::Active)
        .expect("register target source");

    // A large number of unrelated units, each with its own link, tag
    // and block row — the "haystack" an unindexed scan would have to
    // wade through on every single statement below.
    const NOISE_UNITS: usize = 40_000;
    seed_units_with_rows(&cache, noise_source_id, NOISE_UNITS, "noise");

    // The batch under test: a modest number of units in a different
    // source, each with their own link/tag/block row too.
    const TARGET_UNITS: usize = 300;
    let target_unit_ids = seed_units_with_rows(&cache, target_source_id, TARGET_UNITS, "target");

    let started = Instant::now();
    for unit_id in &target_unit_ids {
        cache.delete_unit(*unit_id).expect("delete_unit");
    }
    let elapsed = started.elapsed();

    for unit_id in &target_unit_ids {
        assert!(
            cache
                .connection()
                .query_row("SELECT COUNT(*) FROM units WHERE id = ?1", [unit_id], |r| r
                    .get::<_, i64>(0))
                .unwrap()
                == 0,
            "every targeted unit must actually be gone"
        );
    }
    let remaining_noise: i64 = cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM units WHERE source_id = ?1",
            [noise_source_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining_noise, NOISE_UNITS as i64,
        "deleting the target batch must not touch any noise source row"
    );

    assert!(
        elapsed < Duration::from_secs(5),
        "deleting {TARGET_UNITS} units from a source should stay well under 5s even with \
         {NOISE_UNITS} unrelated units' links/tags/blocks rows in the cache — took {elapsed:?}. \
         A full-table-scan regression would be far slower than this (R17's own probe: 1000 \
         deletes against a 100k-unit cache took 13.8s on its own, with only one source, no \
         extra noise, and no index at all)."
    );
}

/// Inserts `count` units into `source_id`, each with one link, one
/// tag and one block row of its own, and returns their unit ids.
fn seed_units_with_rows(cache: &Cache, source_id: i64, count: usize, label: &str) -> Vec<i64> {
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let rel_path = format!("{label}/note-{i:06}.md");
        let unit_id = cache
            .insert_unit(&NewUnit {
                source_id,
                rel_path: &rel_path,
                mtime: Some(0),
                frontmatter_json: None,
            })
            .expect("insert_unit");
        cache
            .insert_link(&NewLink {
                from_unit_id: unit_id,
                to_raw: "Somewhere",
                to_unit_id: None,
                kind: "wikilink",
            })
            .expect("insert_link");
        cache
            .insert_tag(&NewTag {
                unit_id,
                tag: "seeded",
            })
            .expect("insert_tag");
        cache
            .insert_block(&NewBlock {
                unit_id,
                block_id: "^blk",
                line: 1,
            })
            .expect("insert_block");
        ids.push(unit_id);
    }
    ids
}
