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
                kind: "prose",
                size: Some(100),
                too_large: false,
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
                end_line: None,
                unit_id,
                block_id: Some("abcd1234"),
                line: 1,
                symbol_id: None,
            })
            .expect("insert_block");

        unit_ids.push(unit_id);
    }

    SeededSource {
        source_id,
        unit_ids,
    }
}

/// Writes a real schema-v1 `cache.db` at `db_path` — the exact DDL the
/// v1 build created, before `kind`/`size`/`headings`/`symbols`/
/// `block_fts`/`scan_state` existed — so a test can open it with the
/// v2 build. Two sources: `offline-notes` (status `offline`, two
/// units, one link/tag/block on the first) and `online-repo` (status
/// `active`, one unit). Returns nothing; the ids are fixed (sources
/// 1 and 2, units 1, 2 and 3).
pub fn write_v1_cache(db_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(db_path).expect("open raw v1 db");
    conn.execute_batch(
        "CREATE TABLE sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            path TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('active', 'stale', 'offline')),
            last_scanned_at INTEGER
        );
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version (version) VALUES (1);
        CREATE TABLE units (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id INTEGER NOT NULL REFERENCES sources(id),
            rel_path TEXT NOT NULL,
            mtime INTEGER,
            frontmatter_json TEXT,
            UNIQUE(source_id, rel_path)
        );
        CREATE TABLE links (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            from_unit_id INTEGER NOT NULL REFERENCES units(id),
            to_raw TEXT NOT NULL,
            to_unit_id INTEGER REFERENCES units(id),
            kind TEXT NOT NULL
        );
        CREATE TABLE tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            unit_id INTEGER NOT NULL REFERENCES units(id),
            tag TEXT NOT NULL
        );
        CREATE TABLE blocks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            unit_id INTEGER NOT NULL REFERENCES units(id),
            block_id TEXT NOT NULL,
            line INTEGER NOT NULL
        );
        INSERT INTO sources (id, name, path, status, last_scanned_at)
            VALUES (1, 'offline-notes', '/gone/notes', 'offline', 1700000000),
                   (2, 'online-repo', '/here/repo', 'active', 1700000000);
        INSERT INTO units (id, source_id, rel_path, mtime, frontmatter_json)
            VALUES (1, 1, 'Café notes.md', 1700000001, '{\"title\":\"One\"}'),
                   (2, 1, 'src/main.RS', 1700000002, NULL),
                   (3, 2, 'README.md', 1700000003, NULL);
        INSERT INTO links (id, from_unit_id, to_raw, to_unit_id, kind)
            VALUES (1, 1, '[[Other]]', 2, 'wikilink');
        INSERT INTO tags (id, unit_id, tag) VALUES (1, 1, 'kept');
        INSERT INTO blocks (id, unit_id, block_id, line) VALUES (1, 1, 'abcd1234', 7);",
    )
    .expect("seed v1 db");
}
