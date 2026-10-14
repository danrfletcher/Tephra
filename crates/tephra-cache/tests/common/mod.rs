//! Shared test fixtures and a seed-data builder used across every file
//! in `tests/` (spec: "a small seed-data builder function shared
//! across the above files").
//!
//! Each test binary in `tests/` compiles this module separately and
//! typically uses only a subset of it, so `dead_code` is expected and
//! allowed here rather than per-item.
#![allow(dead_code)]

use tephra_cache::{Cache, NewBlock, NewLink, NewTag, NewUnit, SourceStatus};

/// Opens a `Cache` at `<tempdir>/cache.db`, never the real
/// `~/.local/share/tephra/cache.db`.
pub fn open_temp_cache(dir: &tempfile::TempDir) -> Cache {
    Cache::open(dir.path().join("cache.db")).expect("open cache")
}

/// IDs of the rows seeded by [`seed_source_with_units`], so callers can
/// assert on them without re-deriving row ids from scratch.
pub struct SeededSource {
    pub source_id: i64,
    pub unit_ids: Vec<i64>,
}

/// Registers a source named `name` at `path` with `status`, then seeds
/// it with two units, each carrying one link, one tag, and one block —
/// enough to exercise every one of `units`/`links`/`tags`/`blocks` in a
/// single call. Returns the new rows' ids.
pub fn seed_source_with_units(
    cache: &Cache,
    name: &str,
    path: &str,
    status: SourceStatus,
) -> SeededSource {
    let source_id = cache
        .register_source(name, path, status)
        .expect("register_source");

    let mut unit_ids = Vec::new();
    for (rel_path, frontmatter) in [("one.md", Some("{\"title\":\"One\"}")), ("two.md", None)] {
        let unit_id = cache
            .insert_unit(&NewUnit {
                source_id,
                rel_path,
                mtime: Some(1_700_000_000),
                frontmatter_json: frontmatter,
            })
            .expect("insert_unit");

        cache
            .insert_link(&NewLink {
                from_unit_id: unit_id,
                to_raw: "[[Other]]",
                to_unit_id: None,
                kind: "wikilink",
            })
            .expect("insert_link");

        cache
            .insert_tag(&NewTag {
                unit_id,
                tag: "fixture",
            })
            .expect("insert_tag");

        cache
            .insert_block(&NewBlock {
                unit_id,
                block_id: "abcd1234",
                line: 1,
            })
            .expect("insert_block");

        unit_ids.push(unit_id);
    }

    SeededSource {
        source_id,
        unit_ids,
    }
}
