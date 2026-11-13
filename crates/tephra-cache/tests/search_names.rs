//! `Cache::search_names` (`unit.search`, G8) against a real temp SQLite
//! cache: the title rule, the offline and stale flags, v1 rows kept
//! across the schema upgrade, `limit`, the order of results, and the
//! shape of the one statement a search runs. No mocks; the scoring
//! closure here is deliberately trivial (the real matching rule has its
//! own tests in `tephrad`), so what is under test is the cache side.

mod common;

use rusqlite::Connection;
use tephra_cache::{Cache, NameHit, NewHeading, NewUnit, SourceStatus, NAME_SEARCH_SQL};

fn add(
    cache: &Cache,
    source_id: i64,
    rel_path: &str,
    kind: &str,
    frontmatter: Option<&str>,
) -> i64 {
    cache
        .insert_unit(&NewUnit {
            source_id,
            rel_path,
            mtime: Some(1_700_000_000),
            frontmatter_json: frontmatter,
            kind,
            size: Some(10),
            too_large: false,
        })
        .expect("insert_unit")
}

fn heading(cache: &Cache, unit_id: i64, text: &str, line: i64) {
    cache
        .insert_heading(&NewHeading {
            unit_id,
            level: 1,
            text,
            line,
        })
        .expect("insert_heading");
}

/// Every unit matches with the same score, so the order comes only from
/// the tie-break.
fn everything(cache: &Cache, limit: usize) -> Vec<NameHit> {
    cache.search_names(limit, |_| Some(0.5)).expect("search")
}

fn by_substring(cache: &Cache, needle: &str, limit: usize) -> Vec<NameHit> {
    cache
        .search_names(limit, |c| c.name.contains(needle).then_some(0.5))
        .expect("search")
}

fn paths(hits: &[NameHit]) -> Vec<String> {
    hits.iter()
        .map(|h| format!("@{}:{}", h.source, h.rel_path))
        .collect()
}

#[test]
fn the_title_rule_is_frontmatter_then_first_heading_then_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .unwrap();

    add(
        &cache,
        s,
        "fm.md",
        "prose",
        Some(r#"{"title":"From Frontmatter"}"#),
    );
    let both = add(&cache, s, "both.md", "prose", Some(r#"{"title":"Wins"}"#));
    heading(&cache, both, "Loses", 1);
    // The first heading is the one with the lowest line, however the
    // rows were inserted.
    let h = add(&cache, s, "h.md", "prose", None);
    heading(&cache, h, "Second", 9);
    heading(&cache, h, "First", 2);
    let none = add(&cache, s, "bare.md", "prose", None);
    let _ = none;
    // Not a text title: falls through to the heading, then the name.
    let odd = add(&cache, s, "odd.md", "prose", Some(r#"{"title":42}"#));
    heading(&cache, odd, "Heading Of Odd", 1);
    add(&cache, s, "list.md", "prose", Some(r#"{"title":["a"]}"#));
    // A blank title or heading counts as absent.
    let blank = add(&cache, s, "blank.md", "prose", Some(r#"{"title":"  "}"#));
    heading(&cache, blank, "", 1);
    // Malformed frontmatter must not fail the search.
    add(&cache, s, "broken.md", "prose", Some("{not json"));

    let hits = everything(&cache, 100);
    let title = |p: &str| {
        hits.iter()
            .find(|h| h.rel_path == p)
            .unwrap_or_else(|| panic!("{p} missing from {:?}", paths(&hits)))
            .title
            .clone()
    };
    assert_eq!(title("fm.md"), "From Frontmatter");
    assert_eq!(title("both.md"), "Wins");
    assert_eq!(title("h.md"), "First");
    assert_eq!(title("bare.md"), "bare.md");
    assert_eq!(title("odd.md"), "Heading Of Odd");
    assert_eq!(title("list.md"), "list.md");
    assert_eq!(title("blank.md"), "blank.md");
    assert_eq!(title("broken.md"), "broken.md");
}

#[test]
fn a_non_prose_title_is_the_name_without_its_extension() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("tephra", "/repo", SourceStatus::Active)
        .unwrap();
    // A frontmatter-looking title and headings on a non-prose row are
    // never read.
    let code = add(
        &cache,
        s,
        "crates/atlas/outside.rs",
        "code",
        Some(r#"{"title":"No"}"#),
    );
    heading(&cache, code, "Nope", 1);
    add(&cache, s, "docs/Spec Sheet.pdf", "document", None);
    add(&cache, s, "data/export.tar.gz", "text-other", None);
    add(&cache, s, "img/logo.png", "binary", None);
    add(&cache, s, "Makefile", "text-other", None);
    add(&cache, s, ".gitignore", "text-other", None);

    let hits = everything(&cache, 100);
    let title = |p: &str| hits.iter().find(|h| h.rel_path == p).unwrap().title.clone();
    assert_eq!(title("crates/atlas/outside.rs"), "outside");
    assert_eq!(title("docs/Spec Sheet.pdf"), "Spec Sheet");
    assert_eq!(title("data/export.tar.gz"), "export.tar");
    assert_eq!(title("img/logo.png"), "logo");
    assert_eq!(title("Makefile"), "Makefile");
    assert_eq!(title(".gitignore"), ".gitignore");
}

#[test]
fn every_indexed_kind_is_a_candidate_including_a_too_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("mixed", "/mixed", SourceStatus::Active)
        .unwrap();
    for (path, kind) in [
        ("a.md", "prose"),
        ("b.rs", "code"),
        ("c.pdf", "document"),
        ("d.csv", "text-other"),
        ("e.png", "binary"),
    ] {
        add(&cache, s, path, kind, None);
    }
    cache
        .insert_unit(&NewUnit {
            source_id: s,
            rel_path: "huge/dump.bin",
            mtime: Some(1),
            frontmatter_json: None,
            kind: "binary",
            size: Some(30_000_000),
            too_large: true,
        })
        .unwrap();

    let hits = everything(&cache, 100);
    assert_eq!(hits.len(), 6);
    let mut kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds, ["binary", "code", "document", "prose", "text-other"]);
    assert!(hits.iter().any(|h| h.rel_path == "huge/dump.bin"));
    // Folders are derived, never stored: no candidate is a folder or has
    // a trailing slash.
    assert!(hits.iter().all(|h| !h.rel_path.ends_with('/')));
    assert!(
        by_substring(&cache, "huge", 10).is_empty(),
        "the name is dump.bin, not the folder"
    );
}

#[test]
fn offline_and_stale_sources_are_searched_and_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    for (name, status) in [
        ("live", SourceStatus::Active),
        ("old", SourceStatus::Stale),
        ("gone", SourceStatus::Offline),
    ] {
        let s = cache
            .register_source(name, &format!("/vault/{name}"), status)
            .unwrap();
        add(&cache, s, "Plan.md", "prose", None);
    }

    let hits = everything(&cache, 10);
    assert_eq!(hits.len(), 3, "no source's units are dropped");
    let flags = |source: &str| {
        let h = hits.iter().find(|h| h.source == source).unwrap();
        (h.offline, h.stale)
    };
    assert_eq!(flags("live"), (false, false));
    assert_eq!(flags("old"), (false, true));
    assert_eq!(flags("gone"), (true, false));
}

#[test]
fn units_of_a_v1_cache_are_still_returned_with_the_title_falling_back_to_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    common::write_v1_cache(&db);
    let cache = Cache::open_with_kind_inferrer(&db, |p| {
        if p.ends_with(".md") {
            "prose"
        } else if p.ends_with(".rs") {
            "code"
        } else {
            "text-other"
        }
    })
    .expect("open v1");

    let hits = cache.search_names(10, |_| Some(0.5)).unwrap();
    let get = |p: &str| {
        hits.iter()
            .find(|h| h.rel_path == p)
            .unwrap_or_else(|| panic!("{p} dropped"))
    };
    // Only the offline source's rows are carried over from v1 (the online
    // source is simply rescanned); none of them is dropped.
    assert_eq!(hits.len(), 2);
    // The v1 frontmatter survives, so it still gives the title ...
    let notes = get("Café notes.md");
    assert_eq!((notes.title.as_str(), notes.offline), ("One", true));
    // ... and a row without one falls back to the name (a code file: to
    // the name without its extension). Neither has any headings yet.
    assert_eq!(
        get("src/main.RS").title,
        "src/main.RS"
            .rsplit('/')
            .next()
            .unwrap()
            .rsplit_once('.')
            .unwrap()
            .0
    );
    assert!(hits.iter().all(|h| h.offline));
    // The offline source's rows are the ones kept from v1 and flagged
    // for the next online rescan; they are returned all the same.
    let flagged: Vec<String> = {
        let mut stmt = cache
            .connection()
            .prepare("SELECT rel_path FROM units WHERE needs_rescan = 1 ORDER BY rel_path")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(flagged, ["Café notes.md", "src/main.RS"]);
}

#[test]
fn a_source_unregistered_after_a_scan_no_longer_appears() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    for name in ["keep", "drop"] {
        let s = cache
            .register_source(name, &format!("/vault/{name}"), SourceStatus::Active)
            .unwrap();
        add(&cache, s, "note.md", "prose", None);
    }
    assert_eq!(everything(&cache, 10).len(), 2);
    cache.delete_source("drop").unwrap();
    assert_eq!(paths(&everything(&cache, 10)), ["@keep:note.md"]);
}

#[test]
fn the_same_rel_path_in_two_sources_gives_two_results_in_a_fixed_order() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    for name in ["teamB", "teamA"] {
        let s = cache
            .register_source(name, &format!("/vault/{name}"), SourceStatus::Active)
            .unwrap();
        add(&cache, s, "teamA/Shared.md", "prose", None);
    }
    let first = paths(&everything(&cache, 10));
    assert_eq!(first, ["@teamA:teamA/Shared.md", "@teamB:teamA/Shared.md"]);
    for _ in 0..10 {
        assert_eq!(paths(&everything(&cache, 10)), first);
        assert_eq!(paths(&everything(&cache, 1)), first[..1]);
    }
}

#[test]
fn ties_go_to_the_shorter_path_then_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    // Inserted in no useful order.
    for p in [
        "deep/er/zeta.md",
        "b.md",
        "Z.md",
        "a/x.md",
        "z.md",
        "a.md",
        "A.md",
    ] {
        add(&cache, s, p, "prose", None);
    }
    // Shorter path first; among equal lengths, the name case-insensitively
    // (then by bytes: "A" before "a").
    assert_eq!(
        paths(&everything(&cache, 100)),
        [
            "@n:A.md",
            "@n:a.md",
            "@n:b.md",
            "@n:Z.md",
            "@n:z.md",
            "@n:a/x.md",
            "@n:deep/er/zeta.md"
        ]
    );
}

#[test]
fn a_higher_score_beats_a_shorter_path() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    add(&cache, s, "a.md", "prose", None);
    add(&cache, s, "some/long/path/best.md", "prose", None);
    let hits = cache
        .search_names(10, |c| Some(if c.name == "best.md" { 0.9 } else { 0.2 }))
        .unwrap();
    assert_eq!(paths(&hits), ["@n:some/long/path/best.md", "@n:a.md"]);
    assert_eq!(hits[0].score, 0.9);
}

#[test]
fn limit_returns_exactly_that_many_as_the_top_of_the_unlimited_list() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    for i in 0..60 {
        // Scores repeat, so the cut falls inside a run of ties.
        add(
            &cache,
            s,
            &format!("f{i:02}-{}.md", "x".repeat(i % 7)),
            "prose",
            None,
        );
    }
    let score = |c: &tephra_cache::NameCandidate<'_>| Some(((c.name.len() % 3) as f64 + 1.0) / 4.0);
    let all = cache.search_names(1000, score).unwrap();
    assert_eq!(all.len(), 60);
    for limit in [1, 2, 7, 50, 59, 60, 61] {
        let some = cache.search_names(limit, score).unwrap();
        assert_eq!(some.len(), limit.min(60), "limit {limit}");
        assert_eq!(some, all[..limit.min(60)], "limit {limit}");
    }
    assert!(cache.search_names(0, score).unwrap().is_empty());
}

#[test]
fn units_the_closure_rejects_are_left_out_and_the_closure_sees_every_unit_once() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    for p in ["a.md", "b.rs", "c/d.md"] {
        add(&cache, s, p, "prose", None);
    }
    let mut seen = Vec::new();
    let hits = cache
        .search_names(10, |c| {
            seen.push(c.rel_path.to_string());
            (c.name == "d.md").then_some(1.0)
        })
        .unwrap();
    seen.sort();
    assert_eq!(seen, ["a.md", "b.rs", "c/d.md"]);
    assert_eq!(paths(&hits), ["@n:c/d.md"]);
}

#[test]
fn nothing_is_matched_by_wildcards_in_the_stored_text() {
    // The cache never builds SQL from the query; this pins that a path
    // holding `%`, `_` and quotes is handed over as plain text.
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    for p in ["100%_done.md", "it's [[x]] .*(.md", "plain.md"] {
        add(&cache, s, p, "prose", None);
    }
    assert_eq!(paths(&by_substring(&cache, "%_", 10)), ["@n:100%_done.md"]);
    assert_eq!(
        paths(&by_substring(&cache, "'s [[x]] .*(", 10)),
        ["@n:it's [[x]] .*(.md"]
    );
    assert!(by_substring(&cache, "%", 10).len() == 1);
}

#[test]
fn the_read_only_connection_the_daemon_uses_gives_the_same_answer_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    let u = add(&cache, s, "a.md", "prose", None);
    heading(&cache, u, "Alpha", 1);
    let expected = everything(&cache, 10);

    let reader = Cache::open_read_only(dir.path().join("cache.db")).unwrap();
    let before: i64 = reader
        .connection()
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    for _ in 0..100 {
        assert_eq!(reader.search_names(10, |_| Some(0.5)).unwrap(), expected);
    }
    let after: i64 = reader
        .connection()
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    // And the connection really is read-only.
    assert!(reader
        .connection()
        .execute("DELETE FROM units", [])
        .is_err());
}

#[test]
fn a_search_does_not_block_or_get_blocked_by_an_open_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let s = cache
        .register_source("n", "/n", SourceStatus::Active)
        .unwrap();
    add(&cache, s, "old.md", "prose", None);

    // A writer holds the lock mid-transaction (a scan, say).
    let writer = Connection::open(dir.path().join("cache.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    writer
        .execute(
            "INSERT INTO units (source_id, rel_path, kind) VALUES (?1, 'new.md', 'prose')",
            [s],
        )
        .unwrap();

    let reader = Cache::open_read_only(dir.path().join("cache.db")).unwrap();
    let started = std::time::Instant::now();
    let hits = reader.search_names(10, |_| Some(0.5)).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "must not wait on the writer"
    );
    // It sees the last committed state, not the half-written one.
    assert_eq!(paths(&hits), ["@n:old.md"]);

    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(reader.search_names(10, |_| Some(0.5)).unwrap().len(), 2);
}

/// The plan for the search statement: one pass over `units` (a scan is
/// expected and is the whole cost), `sources` by its primary key, and the
/// first-heading lookup through `idx_headings_unit_id`, never a scan of
/// `headings` per unit.
#[test]
fn the_search_statement_makes_one_pass_over_units_and_looks_headings_up_by_index() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let conn = cache.connection();
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {NAME_SEARCH_SQL}"))
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let joined = plan.join("; ");

    let scans: Vec<&String> = plan.iter().filter(|p| p.starts_with("SCAN")).collect();
    assert_eq!(scans.len(), 1, "exactly one scan, of units: {joined}");
    assert!(
        scans[0].starts_with("SCAN u"),
        "the scan is of units (aliased u): {joined}"
    );
    assert!(
        plan.iter()
            .any(|p| p.contains("SEARCH h ") && p.contains("INDEX") && p.contains("unit_id")),
        "headings must be reached through its unit_id index: {joined}"
    );
    assert!(
        plan.iter()
            .any(|p| p.contains("SEARCH s ") || p.contains("SEARCH sources")),
        "sources must be looked up by key, not scanned: {joined}"
    );
}

/// The `index_scaling.rs` pattern at 100k units: the search returns only
/// `limit` hits however many match, and the scoring closure is the only
/// per-row cost. Wall-clock is printed for the PR body, never asserted.
#[test]
fn a_hundred_thousand_units_are_searched_in_one_pass_keeping_only_limit_hits() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let a = cache
        .register_source("big-a", "/big/a", SourceStatus::Active)
        .unwrap();
    let b = cache
        .register_source("big-b", "/big/b", SourceStatus::Offline)
        .unwrap();
    {
        let conn = cache.connection();
        conn.execute_batch("BEGIN").unwrap();
        let mut unit = conn
            .prepare(
                "INSERT INTO units (source_id, rel_path, kind, size, mtime, frontmatter_json) \
                 VALUES (?1, ?2, ?3, 10, 1, ?4)",
            )
            .unwrap();
        let mut head = conn
            .prepare("INSERT INTO headings (unit_id, level, text, line) VALUES (?1, 1, ?2, 1)")
            .unwrap();
        for i in 0..100_000i64 {
            let (source, kind) = match i % 5 {
                0 => (a, "prose"),
                1 => (b, "prose"),
                2 => (a, "code"),
                3 => (b, "document"),
                _ => (a, "binary"),
            };
            let fm = (i % 10 == 0).then(|| format!(r#"{{"title":"Title {i}"}}"#));
            unit.execute(rusqlite::params![
                source,
                format!("dir{}/sub{}/file{i}.ext", i % 100, i % 7),
                kind,
                fm
            ])
            .unwrap();
            if kind == "prose" && fm.is_none() {
                head.execute(rusqlite::params![
                    conn.last_insert_rowid(),
                    format!("Heading {i}")
                ])
                .unwrap();
            }
        }
        drop((unit, head));
        conn.execute_batch("COMMIT").unwrap();
    }

    let mut offered = 0usize;
    let started = std::time::Instant::now();
    let hits = cache
        .search_names(50, |c| {
            offered += 1;
            // Every unit "matches", so a result set that grew with the
            // matches would hold 100k hits.
            Some(if c.name.ends_with("7.ext") { 0.9 } else { 0.4 })
        })
        .unwrap();
    println!(
        "search over {offered} units, limit 50: {:?}",
        started.elapsed()
    );
    assert_eq!(offered, 100_000, "each unit is offered exactly once");
    assert_eq!(hits.len(), 50);
    assert!(hits.iter().all(|h| h.score == 0.9));
    // Deterministic: the same call gives the same top.
    let again = cache
        .search_names(50, |c| {
            Some(if c.name.ends_with("7.ext") { 0.9 } else { 0.4 })
        })
        .unwrap();
    assert_eq!(hits, again);
}
