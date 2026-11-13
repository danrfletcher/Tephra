//! Integration test: two sources registered concurrently (GP4),
//! exercising independent scan/watch lifecycles — including a
//! crash/removal of one leaving the other's watcher and cache rows
//! untouched (spec: `tests/multi_source.rs`).

mod fixtures;

use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_sources_register_and_scan_fully_independently() {
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");

    let notes_stats = registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    let repo_stats = registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // Each source's own scan sees only its own files — no shared
    // mutable state leaking file counts across sources (GP4).
    assert_eq!(notes_stats.files_seen, 2);
    // README.md, Cargo.toml, and src/main.rs are all walked and each
    // becomes a unit (G2): README.md prose, the other two code.
    assert_eq!(repo_stats.files_seen, 3);
    assert_eq!(
        repo_stats.units_written, 3,
        "README.md, Cargo.toml and src/main.rs are all units"
    );
    assert_eq!(
        repo_stats.links_written, 1,
        "only README.md's [[one]] is a link; Cargo.toml's [[bin]] is not"
    );
    assert_eq!(
        repo_stats.tags_written, 0,
        "main.rs's comment tag is not a tag"
    );

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 2);
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"notes"));
    assert!(names.contains(&"atlas-repo"));
    // Distinct ids (Checkpoint 1 in the spec: "two distinct
    // source_registered log lines with different source ids").
    assert_ne!(sources[0].id, sources[1].id);

    // G2/R3: Cargo.toml and main.rs are `code` units with no extracted rows.
    let repo_row = sources
        .iter()
        .find(|s| s.name == "atlas-repo")
        .expect("atlas-repo row");
    let verify_cache = Cache::open(cache_dir.path().join("cache.db")).expect("reopen cache");
    for (path, kind) in [
        ("README.md", "prose"),
        ("Cargo.toml", "code"),
        ("src/main.rs", "code"),
    ] {
        let unit = verify_cache
            .get_unit_by_rel_path(repo_row.id, path)
            .expect("get_unit_by_rel_path")
            .unwrap_or_else(|| panic!("{path} must be a unit"));
        assert_eq!(unit.kind, kind, "{path}");
        assert!(unit.size.is_some() && !unit.too_large && !unit.needs_rescan);
    }
    for path in ["Cargo.toml", "src/main.rs"] {
        let unit = verify_cache
            .get_unit_by_rel_path(repo_row.id, path)
            .unwrap()
            .unwrap();
        for table in ["links", "tags", "blocks"] {
            let col = if table == "links" {
                "from_unit_id"
            } else {
                "unit_id"
            };
            let n: i64 = verify_cache
                .connection()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {col} = ?1"),
                    [unit.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{path} must have no {table} rows");
        }
    }

    registry.unregister("notes").expect("unregister notes");
    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn two_sources_register_concurrently_from_separate_threads() {
    // R7(d): a real concurrent registration, not two sequential
    // calls from the same thread — proves `register` is actually
    // safe to call from multiple threads at once, as PR-6's eventual
    // JSON-RPC dispatch will.
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = std::sync::Arc::new(SourceRegistry::new(cache).expect("new registry"));

    let notes_root = notes_fixture.root().to_path_buf();
    let repo_root = repo_fixture.root().to_path_buf();

    let registry_for_notes = std::sync::Arc::clone(&registry);
    let notes_handle = std::thread::spawn(move || registry_for_notes.register("notes", notes_root));
    let registry_for_repo = std::sync::Arc::clone(&registry);
    let repo_handle =
        std::thread::spawn(move || registry_for_repo.register("atlas-repo", repo_root));

    let notes_result = notes_handle.join().expect("notes thread panicked");
    let repo_result = repo_handle.join().expect("repo thread panicked");

    assert!(notes_result.is_ok(), "{:?}", notes_result);
    assert!(repo_result.is_ok(), "{:?}", repo_result);

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 2, "both concurrent registrations must land");
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"notes"));
    assert!(names.contains(&"atlas-repo"));

    registry.unregister("notes").expect("unregister notes");
    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn a_sources_root_disappearing_leaves_the_others_watcher_and_rows_untouched() {
    // R7(d): the "crash" is simulated by the root actually
    // disappearing out from under a running watcher (what GP4/E2
    // describe), not by a clean `unregister` call — `unregister` is
    // already covered by
    // `removing_one_source_leaves_the_others_watcher_and_rows_untouched`.
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // Simulate notes's host crashing/unmounting: its root vanishes
    // without ever calling `unregister`.
    std::fs::remove_dir_all(notes_fixture.root()).expect("remove_dir_all");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let notes_offline = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_source("notes")
            .expect("get_source")
            .map(|row| row.status == tephra_cache::SourceStatus::Offline)
            .unwrap_or(false)
    });
    assert!(
        notes_offline,
        "notes must transition to offline rather than vanish or crash the daemon"
    );

    // atlas-repo's row, and its watcher, are untouched by notes's
    // disappearance.
    let repo_row = verify_cache
        .get_source("atlas-repo")
        .expect("get_source")
        .expect("atlas-repo must still be registered");
    let readme_unit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("atlas-repo's README.md row must survive notes's crash");

    repo_fixture.write("README.md", "# atlas-repo\n\nNo links anymore.\n");
    let updated = wait_until(Duration::from_secs(5), || {
        verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
                [readme_unit.id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            == 0
    });
    assert!(
        updated,
        "atlas-repo's watcher must keep running after notes's crash"
    );

    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn removing_one_source_leaves_the_others_watcher_and_rows_untouched() {
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // "Crash/removal" of `notes`.
    registry.unregister("notes").expect("unregister notes");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    assert!(
        verify_cache
            .get_source("notes")
            .expect("get_source")
            .is_none(),
        "notes's source row must be gone"
    );
    let repo_row = verify_cache
        .get_source("atlas-repo")
        .expect("get_source")
        .expect("atlas-repo must still be registered");
    let readme_unit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("atlas-repo's README.md row must survive notes's removal");
    assert_eq!(readme_unit.rel_path, "README.md");

    // atlas-repo's watcher is still live and independently debounces
    // its own edits after notes was torn down.
    repo_fixture.write("README.md", "# atlas-repo\n\nNo links anymore.\n");
    let updated = wait_until(Duration::from_secs(5), || {
        verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
                [readme_unit.id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            == 0
    });
    assert!(
        updated,
        "atlas-repo's watcher must keep running after notes's removal"
    );

    // The re-parsed row for README.md must still exist (a fresh row,
    // from the delete-then-reinsert apply_change_batch does per
    // change) and carry zero links, matching its edited content.
    let readme_after_edit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("README.md's row must still exist after the edit");
    let link_count_after: i64 = verify_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
            [readme_after_edit.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        link_count_after, 0,
        "the edited README.md has no links left"
    );

    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

/// `(kind, links, tags, blocks)` for every unit of `source_id`, keyed by
/// `rel_path`.
fn unit_summary(
    cache: &Cache,
    source_id: i64,
) -> std::collections::BTreeMap<String, (String, i64, i64, i64)> {
    let conn = cache.connection();
    let mut stmt = conn
        .prepare(
            "SELECT u.rel_path, u.kind, \
                (SELECT COUNT(*) FROM links l WHERE l.from_unit_id = u.id), \
                (SELECT COUNT(*) FROM tags t WHERE t.unit_id = u.id), \
                (SELECT COUNT(*) FROM blocks b WHERE b.unit_id = u.id) \
             FROM units u WHERE u.source_id = ?1",
        )
        .unwrap();
    let rows = stmt
        .query_map([source_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn every_non_ignored_file_is_a_unit_with_its_kind_and_only_prose_and_code_have_rows() {
    // G2/G6/G15 through the real register -> scan path, a real temp
    // dir and a real cache.
    let fixture = fixtures::kinds_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");

    let stats = registry
        .register("kinds", fixture.root())
        .expect("register");
    assert_eq!(stats.units_written, fixtures::KINDS_EXPECTED.len());
    // Files under an ignored *directory* (`.venv`, `__pycache__`) are
    // pruned before the walk lists them; the two ignored `.DS_Store`
    // *files* are listed, counted as seen, and skipped.
    assert_eq!(stats.files_ignored, 2);
    assert_eq!(stats.files_seen, fixtures::KINDS_EXPECTED.len() + 2);

    let cache = Cache::open(&cache_path).expect("reopen cache");
    let source_id = cache.get_source("kinds").unwrap().unwrap().id;
    let summary = unit_summary(&cache, source_id);

    let expected_paths: std::collections::BTreeSet<&str> =
        fixtures::KINDS_EXPECTED.iter().map(|(p, _)| *p).collect();
    let actual_paths: std::collections::BTreeSet<&str> =
        summary.keys().map(String::as_str).collect();
    assert_eq!(
        actual_paths, expected_paths,
        "exact set of units, exact rel_paths"
    );

    for (path, kind) in fixtures::KINDS_EXPECTED {
        let (got_kind, links, tags, blocks) = &summary[path];
        assert_eq!(got_kind, kind, "{path}");
        if got_kind != "prose" {
            // G4/G13: a code unit has its comment links and its `^id`
            // comment blocks; no other non-prose unit has links or
            // blocks, and none has tags.
            let code_links = fixtures::KINDS_CODE_LINKS
                .iter()
                .find(|(p, _)| *p == path)
                .map_or(0, |(_, n)| *n);
            let code_blocks = fixtures::KINDS_CODE_BLOCKS
                .iter()
                .find(|(p, _)| *p == path)
                .map_or(0, |(_, n)| *n);
            assert_eq!(
                (*links, *tags, *blocks),
                (code_links, 0, code_blocks),
                "{path}: a non-prose unit has no tags, links only from code comments and blocks only from code ID comments"
            );
        }
    }
    // Every prose file goes through the parser (G3), whatever its
    // extension. `blocks` counts every row: paragraphs and anchors.
    // one.md: two paragraphs (the second ends in `^block1`).
    assert_eq!(summary["notes/one.md"], ("prose".to_string(), 1, 1, 2));
    assert_eq!(summary["Notes.MD"], ("prose".to_string(), 1, 0, 1));
    // A `.txt` gives its link and its paragraph (with the `^blk123`),
    // but never a tag: `#tag` is read from Markdown only.
    assert_eq!(summary["notes/two.txt"], ("prose".to_string(), 1, 0, 1));
    assert_eq!(
        summary["notes/café notes.txt"],
        ("prose".to_string(), 1, 0, 1)
    );
    assert_eq!(summary["empty.md"], ("prose".to_string(), 0, 0, 0));

    for ignored in fixtures::KINDS_IGNORED {
        assert!(
            !summary.contains_key(ignored),
            "{ignored} is under an ignored name and must not be a unit"
        );
    }
    for path in ["docs/a b.PDF", "img/logo.png"] {
        let row = cache
            .get_unit_by_rel_path(source_id, path)
            .unwrap()
            .unwrap();
        let on_disk = std::fs::metadata(fixture.root().join(path)).unwrap().len();
        assert_eq!(
            row.size,
            Some(on_disk as i64),
            "{path}: size is the file length"
        );
        assert!(row.mtime.is_some());
    }

    registry.unregister("kinds").expect("unregister");
}

#[test]
fn a_scan_never_writes_into_a_source_file() {
    // F2: byte-compare the whole fixture tree (names, contents,
    // mtimes) before and after a register-scan and a forced rescan.
    fn snapshot(root: &std::path::Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push((
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        std::fs::read(&path).unwrap(),
                        std::fs::metadata(&path).unwrap().modified().unwrap(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    let fixture = fixtures::kinds_fixture();
    let before = snapshot(fixture.root());
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let registry =
        SourceRegistry::new(Cache::open(cache_dir.path().join("cache.db")).expect("open cache"))
            .expect("registry");
    registry
        .register("kinds", fixture.root())
        .expect("register");
    registry.rescan("kinds").expect("rescan");
    let after = snapshot(fixture.root());
    assert_eq!(
        before, after,
        "the scan must not create, change or delete any source file"
    );
    registry.unregister("kinds").expect("unregister");
}

#[test]
fn a_real_scan_of_a_mixed_format_tree_gives_the_expected_rows_and_writes_nothing() {
    // G3 + F4/F5/F6/E2/E7 through the real register -> scan path: a
    // real temp dir, a real cache, the real parser.
    let fixture = fixtures::prose_fixture();
    let before = fixtures::tree_snapshot(fixture.root());

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    let stats = registry
        .register("prose", fixture.root())
        .expect("register");
    assert_eq!(
        stats.units_written,
        fixtures::PROSE_EXPECTED.len()
            + fixtures::CODE_EXPECTED.len()
            + fixtures::PROSE_NO_ROWS.len()
    );

    // G4/G13: a code file gives its one comment link, and a block for
    // each `^id` comment (only `main.py` has one: `# ... ^x` owned by
    // the assignment below it); no heading or tag.
    for path in fixtures::CODE_EXPECTED {
        let got = fixtures::unit_rows(&cache_path, "prose", path)
            .unwrap_or_else(|| panic!("{path} is not a unit"));
        assert_eq!(got.kind, "code", "{path}");
        assert_eq!(
            got.links,
            vec![("wikilink".to_string(), "Real".to_string())],
            "{path}"
        );
        let want_blocks: Vec<(Option<String>, i64, i64)> = if path == "main.py" {
            vec![(Some("x".to_string()), 2, 2)]
        } else {
            Vec::new()
        };
        assert!(
            got.headings.is_empty() && got.tags.is_empty() && got.blocks == want_blocks,
            "{path}: {got:?}"
        );
    }

    // Every prose file: its headings, links, tags and blocks, exactly.
    for expected in &fixtures::PROSE_EXPECTED {
        let got = fixtures::unit_rows(&cache_path, "prose", expected.0)
            .unwrap_or_else(|| panic!("{} is not a unit", expected.0));
        assert_eq!(got, fixtures::expected_rows(expected), "{}", expected.0);
    }

    // E2/F5/F6: units, but no rows of any kind.
    for path in fixtures::PROSE_NO_ROWS {
        let got = fixtures::unit_rows(&cache_path, "prose", path)
            .unwrap_or_else(|| panic!("{path} is not a unit"));
        assert_ne!(got.kind, "prose", "{path}");
        assert_eq!(
            (
                got.headings.len(),
                got.links.len(),
                got.tags.len(),
                got.blocks.len()
            ),
            (0, 0, 0, 0),
            "{path}"
        );
    }

    // E7: both `^dup` occurrences are stored, one row each, with their
    // own lines (asserted in PROSE_EXPECTED for NOTES.TXT above), and
    // the scan wrote no byte and moved no mtime.
    let after = fixtures::tree_snapshot(fixture.root());
    assert_eq!(before, after, "indexing must never write to the source");

    registry.unregister("prose").expect("unregister");
}

#[test]
fn utf16_bom_prose_parses_the_same_as_its_utf8_copy() {
    let fixture = fixtures::empty_source();
    let text = "Plan\n\nsee [[Café]] ^id1\n";
    fixture.write("utf8.txt", text);
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    fixture.write_bytes("utf16.txt", &utf16);
    // UTF-16BE with a BOM too.
    let mut be = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        be.extend_from_slice(&unit.to_be_bytes());
    }
    fixture.write_bytes("utf16be.txt", &be);

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("u", fixture.root()).expect("register");

    let utf8 = fixtures::unit_rows(&cache_path, "u", "utf8.txt").unwrap();
    assert_eq!(utf8.kind, "prose");
    assert_eq!(utf8.links, [("wikilink".to_string(), "Café".to_string())]);
    assert_eq!(utf8.blocks.len(), 2);
    for name in ["utf16.txt", "utf16be.txt"] {
        assert_eq!(
            fixtures::unit_rows(&cache_path, "u", name).unwrap(),
            utf8,
            "{name}"
        );
    }
    registry.unregister("u").expect("unregister");
}

#[test]
fn prose_that_is_not_utf8_or_bom_utf16_is_not_parsed() {
    // Latin-1 bytes with no BOM: valid text to a sniffer, invalid
    // UTF-8 to the parser. Whatever kind it ends up with, it gives no
    // rows and never fails the scan.
    let fixture = fixtures::empty_source();
    fixture.write("ok.txt", "[[Fine]]\n");
    fixture.write_bytes("latin1.txt", b"caf\xe9 [[Nope]] ^x\n");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("l", fixture.root()).expect("register");

    let ok = fixtures::unit_rows(&cache_path, "l", "ok.txt").unwrap();
    assert_eq!(ok.links.len(), 1);
    let latin = fixtures::unit_rows(&cache_path, "l", "latin1.txt").expect("still a unit");
    assert_eq!(
        (latin.links.len(), latin.blocks.len(), latin.headings.len()),
        (0, 0, 0)
    );
    registry.unregister("l").expect("unregister");
}

#[test]
fn an_empty_or_whitespace_only_prose_file_is_a_unit_with_no_rows_in_every_format() {
    let fixture = fixtures::empty_source();
    for name in ["e.md", "e.txt", "e.org", "e.rst", "e.adoc"] {
        fixture.write(name, "");
        fixture.write(&format!("w-{name}"), "  \n\t\n\r\n   ");
    }
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("e", fixture.root()).expect("register");

    for name in ["e.md", "e.txt", "e.org", "e.rst", "e.adoc"] {
        for path in [name.to_string(), format!("w-{name}")] {
            let got = fixtures::unit_rows(&cache_path, "e", &path)
                .unwrap_or_else(|| panic!("{path} must be a unit"));
            assert_eq!(
                (
                    got.headings.len(),
                    got.links.len(),
                    got.tags.len(),
                    got.blocks.len()
                ),
                (0, 0, 0, 0),
                "{path}"
            );
        }
    }
    registry.unregister("e").expect("unregister");
}

#[test]
fn malformed_prose_never_fails_the_scan() {
    let fixture = fixtures::empty_source();
    // Valid UTF-8 noise built from the constructs each format keys on.
    let noise = "* [[ ][ ]] ^ #+BEGIN_SRC\n::\n.. code-block::\n----\n=====\n\n\
                 ** ****** = == =\n[[a|[[b]]]] ^^ ^id ^id\n\n\u{0}\u{1}\u{7f}";
    for name in ["m.md", "m.txt", "m.org", "m.rst", "m.adoc"] {
        fixture.write(name, noise);
    }
    fixture.write(
        "stars.org",
        &format!("{} deep\n{}\n", "*".repeat(5_000), "*".repeat(10_000)),
    );
    let bar = "=".repeat(10_000);
    fixture.write("bar.rst", &format!("{bar}\nT\n{bar}\n\n{bar}\n"));
    fixture.write("ok.txt", "[[Fine]]\n");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    let stats = registry.register("m", fixture.root()).expect("register");
    assert_eq!(stats.units_written, 8);
    assert!(fixtures::unit_rows(&cache_path, "m", "ok.txt").is_some());
    registry.unregister("m").expect("unregister");
}

// ------------------------------------------------------------ E8 / G4

fn blocks_with_id_rows(cache_path: &std::path::Path, source: &str, id: &str) -> Vec<(String, i64)> {
    let cache = Cache::open(cache_path).expect("open cache");
    let source_id = cache.get_source(source).unwrap().unwrap().id;
    cache
        .blocks_with_id(source_id, id)
        .unwrap()
        .into_iter()
        .map(|h| (h.rel_path, h.line))
        .collect()
}

#[test]
fn the_same_id_in_two_files_of_one_source_gives_two_rows_and_a_third_source_is_not_mixed_in() {
    // E8 through a real scan: `conflict.rs` and `registry_copy.rs` both
    // carry `// ^k3f9a2` on a symbol; a different source has the same
    // ID too. `blocks_with_id` for the first source lists exactly its
    // two files, sorted by path.
    let body =
        |name: &str| format!("use std::fmt;\n\n/// Docs.\n// ^k3f9a2\npub enum {name} {{ A }}\n");
    let repo = fixtures::empty_source();
    repo.write(
        "crates/tephrad/src/registry_copy.rs",
        &body("ConflictReason"),
    );
    repo.write("crates/tephrad/src/conflict.rs", &body("ConflictReason"));
    repo.write(
        "crates/tephrad/src/unrelated.rs",
        "// ^other-id\nfn f() {}\n",
    );
    let other = fixtures::empty_source();
    other.write("conflict.rs", &body("ConflictReason"));

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("repo", repo.root()).expect("register");
    registry.register("third", other.root()).expect("register");

    assert_eq!(
        blocks_with_id_rows(&cache_path, "repo", "k3f9a2"),
        [
            ("crates/tephrad/src/conflict.rs".to_string(), 5),
            ("crates/tephrad/src/registry_copy.rs".to_string(), 5),
        ]
    );
    assert_eq!(
        blocks_with_id_rows(&cache_path, "third", "k3f9a2"),
        [("conflict.rs".to_string(), 5)]
    );
    assert_eq!(
        blocks_with_id_rows(&cache_path, "repo", "other-id"),
        [("crates/tephrad/src/unrelated.rs".to_string(), 2)]
    );

    // Each block row names its enum symbol.
    let cache = Cache::open(&cache_path).unwrap();
    let linked: i64 = cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM blocks b JOIN symbols s ON s.id = b.symbol_id \
             WHERE b.block_id = 'k3f9a2' AND s.name = 'ConflictReason' AND s.kind = 'enum' AND s.line = b.line",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked, 3);
    registry.unregister("repo").unwrap();
    registry.unregister("third").unwrap();
}

#[test]
fn two_ids_the_same_in_one_file_are_two_rows_and_a_rescan_does_not_double_them() {
    let repo = fixtures::empty_source();
    repo.write("a.rs", "// ^k3f9a2\nfn one() {}\n// ^k3f9a2\nfn two() {}\n");
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("repo", repo.root()).expect("register");
    let want = [("a.rs".to_string(), 2), ("a.rs".to_string(), 4)];
    assert_eq!(blocks_with_id_rows(&cache_path, "repo", "k3f9a2"), want);
    registry.rescan("repo").expect("rescan");
    assert_eq!(blocks_with_id_rows(&cache_path, "repo", "k3f9a2"), want);
    registry.unregister("repo").unwrap();
}

#[test]
fn a_language_without_a_grammar_and_a_non_code_kind_get_no_symbols() {
    // F5: `.rb` and `.kt` are `text-other`, never parsed for symbols.
    let repo = fixtures::empty_source();
    repo.write("a.rb", "def hello\n  1\nend\n");
    repo.write("b.kt", "fun main() {}\n");
    repo.write("notes.md", "# Heading\n\nfn main() {}\n");
    repo.write("c.rs", "fn real() {}\n");
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry.register("repo", repo.root()).expect("register");
    let cache = Cache::open(&cache_path).unwrap();
    let source_id = cache.get_source("repo").unwrap().unwrap().id;
    for (rel, kind, symbols) in [
        ("a.rb", "text-other", 0),
        ("b.kt", "text-other", 0),
        ("notes.md", "prose", 0),
        ("c.rs", "code", 1),
    ] {
        let unit = cache.get_unit_by_rel_path(source_id, rel).unwrap().unwrap();
        assert_eq!(unit.kind, kind, "{rel}");
        assert_eq!(
            cache.symbols_for_unit(unit.id).unwrap().len(),
            symbols,
            "{rel}"
        );
    }
    registry.unregister("repo").unwrap();
}

#[test]
fn a_half_written_code_file_never_fails_the_scan_and_keeps_the_symbols_before_the_error() {
    // E12: an unclosed `{` after `fn walk_and_write` and a Python def
    // with a broken parameter list. The scan finishes, the other
    // files are unaffected, and the symbols before the error are
    // stored.
    let repo = fixtures::empty_source();
    repo.write(
        "broken.rs",
        "fn before() {}\n\nstruct Mid;\n\nfn walk_and_write() {\n    let x = 1;\n",
    );
    repo.write("broken.py", "def ok():\n    pass\n\ndef bad(:\n    pass\n");
    repo.write("fine.rs", "fn fine() {}\n");
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    let stats = registry.register("repo", repo.root()).expect("register");
    assert_eq!(stats.units_written, 3);
    let cache = Cache::open(&cache_path).unwrap();
    let source_id = cache.get_source("repo").unwrap().unwrap().id;
    let names = |rel: &str| -> Vec<String> {
        let unit = cache.get_unit_by_rel_path(source_id, rel).unwrap().unwrap();
        cache
            .symbols_for_unit(unit.id)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    };
    let rs = names("broken.rs");
    assert!(
        rs.contains(&"before".to_string()) && rs.contains(&"Mid".to_string()),
        "{rs:?}"
    );
    assert!(names("broken.py").contains(&"ok".to_string()));
    assert_eq!(names("fine.rs"), ["fine"]);
    registry.unregister("repo").unwrap();
}
