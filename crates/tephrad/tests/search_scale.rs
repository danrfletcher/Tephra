//! `unit.search` at 100k units through the real daemon search path
//! (`tephrad::search::search_names`: read-only connection, the real
//! fuzzy ranking, the `offline` probe), on the `index_scaling.rs` fixture
//! pattern. The cache-level test in `tephra-cache/tests/search_names.rs`
//! checks the one-pass SQL with a stub scorer; this one adds the real
//! scorer. The wall-clock is printed for the PR body and never asserted.

use tephra_cache::{Cache, SourceStatus};
use tephrad::search::search_names;

const UNITS: i64 = 100_000;

fn build_cache(path: &std::path::Path) {
    let cache = Cache::open(path).unwrap();
    let a = cache
        .register_source("big-a", "/big/a", SourceStatus::Active)
        .unwrap();
    let b = cache
        .register_source("big-b", "/big/b", SourceStatus::Offline)
        .unwrap();
    let conn = cache.connection();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut unit = conn
            .prepare(
                "INSERT INTO units (source_id, rel_path, kind, size, mtime, frontmatter_json) \
                 VALUES (?1, ?2, ?3, 10, 1, ?4)",
            )
            .unwrap();
        for i in 0..UNITS {
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
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn a_hundred_thousand_units_are_searched_through_the_real_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    build_cache(&path);

    // A rare name: one unit matches it best among 100k.
    let started = std::time::Instant::now();
    let hits = search_names(&path, "file77777", 50).unwrap();
    println!(
        "unit.search \"file77777\" over {UNITS} units, limit 50: {:?}",
        started.elapsed()
    );
    assert!(!hits.is_empty() && hits.len() <= 50);
    assert_eq!(
        hits[0].id, "@big-a:dir77/sub0/file77777.ext",
        "the rare name leads"
    );
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));

    // A query that matches everything keeps only `limit` hits.
    let started = std::time::Instant::now();
    let many = search_names(&path, "file", 50).unwrap();
    println!(
        "unit.search \"file\" (every unit matches) over {UNITS} units, limit 50: {:?}",
        started.elapsed()
    );
    assert_eq!(many.len(), 50, "limit is honoured exactly");

    // The same call gives the same answer.
    assert_eq!(many, search_names(&path, "file", 50).unwrap());
}
