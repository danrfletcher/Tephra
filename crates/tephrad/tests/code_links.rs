//! G4 / GP8 / F3 / F4 / F5 / F7 / E12 through the real path: a real
//! `SourceRegistry` over two temp sources (a vault named `notes` and a
//! repo named `tephra`), a real cache, the real watcher and the real
//! `tephra_graph::UnitGraph` built from that cache. No mocks.
//!
//! Backlinks are read from a `UnitGraph` rebuilt from the cache file,
//! which is what `graph.get_backlinks` answers from (the RPC wiring
//! itself is covered by `rpc_live_graph.rs`; this PR adds no method).

mod fixtures;

use std::path::Path;
use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephra_graph::{Edge, EdgeTarget, UnitGraph};
use tephrad::registry::SourceRegistry;

/// Polls `check` until it is true or `timeout` passes (the pattern in
/// `watcher_scan.rs`: never a fixed sleep against the real debounce).
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

/// A graph built from the cache as it is right now.
fn graph(cache_path: &Path) -> UnitGraph {
    let cache = Cache::open(cache_path).expect("open cache for graph");
    UnitGraph::build_from_cache(&cache).expect("build graph")
}

/// Every referring unit of `id`, one entry per link row (no display
/// dedup), as `(from, to_raw)`.
fn backlinks(cache_path: &Path, id: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = graph(cache_path)
        .get_backlinks_all(id)
        .into_iter()
        .map(|e: Edge| (e.from, e.to_raw))
        .collect();
    out.sort();
    out
}

struct World {
    vault: fixtures::TempSource,
    repo: fixtures::TempSource,
    cache_path: std::path::PathBuf,
    registry: SourceRegistry,
    _cache_dir: tempfile::TempDir,
}

const SCAN_RS: &str = "crates/tephrad/src/scan.rs";

/// A vault with `Tephra test.md`, `Decoy.md` and `Vaultonly.md`, and a
/// repo with `crates/tephrad/src/scan.rs` (no comment link yet, only
/// a string literal), `Repoonly.md` and the files named per test.
fn world(extra: impl FnOnce(&fixtures::TempSource)) -> World {
    let vault = fixtures::empty_source();
    vault.write("Tephra test.md", "# Tephra test\n\nA note.\n");
    vault.write("Decoy.md", "# Decoy\n");
    vault.write("Vaultonly.md", "# Vault only\n");

    let repo = fixtures::empty_source();
    repo.write(
        SCAN_RS,
        "fn walk_and_write() {}\nconst DECOY: &str = \"[[@notes:Decoy]]\";\n",
    );
    repo.write("Repoonly.md", "# Repo only\n");
    extra(&repo);

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry");
    registry
        .register("notes", vault.root())
        .expect("register notes");
    registry
        .register("tephra", repo.root())
        .expect("register tephra");
    World {
        vault,
        repo,
        cache_path,
        registry,
        _cache_dir: cache_dir,
    }
}

impl World {
    fn finish(self) {
        self.registry.unregister("notes").expect("unregister notes");
        self.registry
            .unregister("tephra")
            .expect("unregister tephra");
    }
}

/// `(kind, name, line)` of every symbol of one unit, in file order.
fn symbol_rows(cache_path: &Path, source: &str, rel_path: &str) -> Vec<(String, String, i64)> {
    let cache = Cache::open(cache_path).unwrap();
    let source_id = cache.get_source(source).unwrap().unwrap().id;
    let unit = cache
        .get_unit_by_rel_path(source_id, rel_path)
        .unwrap()
        .unwrap();
    cache
        .symbols_for_unit(unit.id)
        .unwrap()
        .into_iter()
        .map(|s| (s.kind, s.name, s.line))
        .collect()
}

fn scan_unit() -> String {
    format!("@tephra:{SCAN_RS}")
}

// ---------------------------------------------------------------- GP8

#[test]
fn gp8_a_comment_link_in_a_code_file_is_a_backlink_and_a_string_decoy_is_not() {
    let w = world(|repo| {
        repo.write(
            SCAN_RS,
            concat!(
                "// see [[@notes:Tephra test]]\n",
                "fn walk_and_write() {}\n",
                "const DECOY: &str = \"[[@notes:Tephra test]]\";\n",
                "const OTHER: &str = \"[[@notes:Decoy]]\";\n",
            ),
        );
    });

    // Exactly one link row from scan.rs: the comment. The decoy
    // string literals, naming the same note and another one, are not
    // rows.
    assert_eq!(
        backlinks(&w.cache_path, "@notes:Tephra test.md"),
        vec![(scan_unit(), "@notes:Tephra test".to_string())]
    );
    assert!(backlinks(&w.cache_path, "@notes:Decoy.md").is_empty());

    // The RPC's own query (`get_backlinks`) lists the code file once.
    let edges = graph(&w.cache_path).get_backlinks("@notes:Tephra test.md");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from, scan_unit());
    assert_eq!(edges[0].kind, tephra_parser::LINK_KIND_WIKILINK);
    assert_eq!(
        edges[0].to,
        EdgeTarget::Resolved("@notes:Tephra test.md".into())
    );

    // The unit itself is `code`, with that one link row, its three
    // symbols (G4), and no tag, block or heading row.
    let rows = fixtures::unit_rows(&w.cache_path, "tephra", SCAN_RS).expect("scan.rs is a unit");
    assert_eq!(rows.kind, "code");
    assert_eq!(
        rows.links,
        vec![("wikilink".to_string(), "@notes:Tephra test".to_string())]
    );
    assert!(rows.tags.is_empty() && rows.blocks.is_empty() && rows.headings.is_empty());
    assert_eq!(
        symbol_rows(&w.cache_path, "tephra", SCAN_RS),
        vec![
            ("function".to_string(), "walk_and_write".to_string(), 2),
            ("const".to_string(), "DECOY".to_string(), 3),
            ("const".to_string(), "OTHER".to_string(), 4),
        ],
        "the string decoys are no links but the consts are symbols"
    );
    w.finish();
}

#[test]
fn gp8_watcher_adding_a_comment_shows_up_after_the_debounce_and_removing_it_takes_it_away() {
    let w = world(|_| {});
    let id = "@notes:Tephra test.md";
    assert!(backlinks(&w.cache_path, id).is_empty(), "nothing yet");

    // Add the comment above `fn walk_and_write` (and keep the decoy).
    w.repo.write(
        SCAN_RS,
        concat!(
            "// see [[@notes:Tephra test]]\n",
            "fn walk_and_write() {}\n",
            "const DECOY: &str = \"[[@notes:Tephra test]]\";\n",
        ),
    );
    assert!(
        wait_until(Duration::from_secs(20), || {
            backlinks(&w.cache_path, id) == vec![(scan_unit(), "@notes:Tephra test".to_string())]
        }),
        "the new comment link never appeared: {:?}",
        backlinks(&w.cache_path, id)
    );

    // Remove the comment (the string literal stays): the backlink goes.
    w.repo.write(
        SCAN_RS,
        "fn walk_and_write() {}\nconst DECOY: &str = \"[[@notes:Tephra test]]\";\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || backlinks(&w.cache_path, id)
            .is_empty()),
        "the removed comment's backlink never went away: {:?}",
        backlinks(&w.cache_path, id)
    );

    // A brand-new code file arriving through the watcher is parsed by
    // the code dispatch too: its docstring counts, its string does not.
    w.repo.write(
        "crates/new.py",
        "\"\"\"Docstring [[@notes:Tephra test]]\"\"\"\nx = '[[@notes:Decoy]]'\n",
    );
    assert!(
        wait_until(Duration::from_secs(20), || {
            backlinks(&w.cache_path, id)
                == vec![(
                    "@tephra:crates/new.py".to_string(),
                    "@notes:Tephra test".to_string(),
                )]
        }),
        "a new Python docstring link never appeared: {:?}",
        backlinks(&w.cache_path, id)
    );
    assert!(backlinks(&w.cache_path, "@notes:Decoy.md").is_empty());
    w.finish();
}

// ---------------------------------------------------------------- F7

#[test]
fn f7_a_code_comment_link_without_a_prefix_resolves_only_in_its_own_source() {
    let w = world(|repo| {
        repo.write(
            SCAN_RS,
            concat!(
                "// own [[Repoonly]]\n",
                "// other source, no prefix [[Vaultonly]]\n",
                "// other source, prefixed [[@notes:Vaultonly]]\n",
                "fn f() {}\n",
            ),
        );
    });
    // Unprefixed: found in the code file's own source.
    assert_eq!(
        backlinks(&w.cache_path, "@tephra:Repoonly.md"),
        vec![(scan_unit(), "Repoonly".to_string())]
    );
    // `[[Vaultonly]]` does not reach across into `notes`; only the
    // `@notes:`-prefixed one does.
    assert_eq!(
        backlinks(&w.cache_path, "@notes:Vaultonly.md"),
        vec![(scan_unit(), "@notes:Vaultonly".to_string())]
    );
    w.finish();
}

// ---------------------------------------------------------------- F3

#[test]
fn f3_a_symbol_style_anchor_is_stored_as_an_anchor_link_with_no_symbol_resolution() {
    let w = world(|repo| {
        repo.write(
            SCAN_RS,
            "// [[@tephra:crates/tephrad/src/scan.rs#walk_and_write]]\nfn walk_and_write() {}\n",
        );
    });
    let rows = fixtures::unit_rows(&w.cache_path, "tephra", SCAN_RS).unwrap();
    assert_eq!(
        rows.links,
        vec![(
            "wikilink".to_string(),
            "@tephra:crates/tephrad/src/scan.rs#walk_and_write".to_string()
        )]
    );
    // F3: the resolver reads headings for `#name`, never symbols. The
    // symbol is recorded (G4) but the link stays what it was: a
    // `wikilink` row with the raw anchor text (asserted above), with
    // no column or table that could name a symbol.
    assert_eq!(
        symbol_rows(&w.cache_path, "tephra", SCAN_RS),
        vec![("function".to_string(), "walk_and_write".to_string(), 2)]
    );
    let cache = Cache::open(&w.cache_path).unwrap();
    let link_columns: Vec<String> = cache
        .connection()
        .prepare("SELECT name FROM pragma_table_info('links')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        link_columns.iter().all(|c| !c.contains("symbol")),
        "{link_columns:?}"
    );
    w.finish();
}

// ---------------------------------------------------------------- F5

#[test]
fn f5_a_language_without_a_grammar_is_a_text_other_unit_with_no_links() {
    let w = world(|repo| {
        repo.write("main.kt", "// [[A]]\nfun main() {}\n");
        repo.write("script.rb", "# [[A]]\nputs 1\n");
        repo.write("data.json", "{\"a\": \"[[A]]\"}\n// [[A]]\n");
        repo.write("notes.ini", "; [[A]]\n# [[A]]\n[section]\nk=v\n");
    });
    for path in ["main.kt", "script.rb", "data.json", "notes.ini"] {
        let rows = fixtures::unit_rows(&w.cache_path, "tephra", path)
            .unwrap_or_else(|| panic!("{path} is not a unit"));
        assert_eq!(rows.kind, "text-other", "{path}");
        assert!(
            rows.links.is_empty() && rows.tags.is_empty() && rows.blocks.is_empty(),
            "{path}: {rows:?}"
        );
    }
    w.finish();
}

// ---------------------------------------------------------------- F4

#[test]
fn f4_a_full_scan_and_a_rescan_never_write_to_a_code_file() {
    let vault = fixtures::empty_source();
    vault.write("Tephra test.md", "# T\n");
    let repo = fixtures::empty_source();
    repo.write(SCAN_RS, "// [[@notes:Tephra test]]\nfn f() {}\n");
    repo.write("run.sh", "#!/bin/bash\n# [[@notes:Tephra test]]\necho hi\n");
    repo.write("lib/a.py", "\"\"\"[[@notes:Tephra test]]\"\"\"\n");
    repo.write("cfg.toml", "# [[@notes:Tephra test]]\nk = 1\n");
    repo.write("crlf.c", "/* [[@notes:Tephra test]] */\r\nint x;\r\n");
    let before_vault = fixtures::tree_snapshot(vault.root());
    let before_repo = fixtures::tree_snapshot(repo.root());

    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", vault.root()).unwrap();
    let stats = registry.register("tephra", repo.root()).unwrap();
    assert_eq!(stats.links_written, 5, "one comment link per code file");
    registry.rescan("tephra").unwrap();

    assert_eq!(before_vault, fixtures::tree_snapshot(vault.root()));
    assert_eq!(
        before_repo,
        fixtures::tree_snapshot(repo.root()),
        "bytes and mtime of every code file are unchanged"
    );
    assert_eq!(backlinks(&cache_path, "@notes:Tephra test.md").len(), 5);
    registry.unregister("notes").unwrap();
    registry.unregister("tephra").unwrap();
}

// ---------------------------------------------------------------- E12

#[test]
fn e12_a_scan_with_a_broken_rs_and_a_binary_looking_py_keeps_every_good_files_links() {
    let w = world(|repo| {
        repo.write(
            "broken.rs",
            "// [[@notes:Tephra test]]\nfn broken( {\n    let x = ;\n// [[@notes:Decoy]]\n",
        );
        // A NUL in the head: `binary` (PR-1.S1), never parsed.
        let mut py = b"# [[@notes:Vaultonly]]\nx = 1\n".to_vec();
        py.extend_from_slice(&[0, 1, 2, 0]);
        repo.write_bytes("binaryish.py", &py);
        repo.write("good.py", "# [[@notes:Vaultonly]]\n");
        repo.write(
            "good.go",
            "package g\n\n// [[@notes:Vaultonly]]\nfunc F() {}\n",
        );
        // Invalid UTF-8 after the sniff window: stored as binary, not
        // a failed scan.
        let mut late = vec![b'a'; 9000];
        late.extend_from_slice(&[0xFF, 0xFE, b'\n']);
        repo.write_bytes("late-bad-utf8.rs", &late);
    });

    let rows = |p: &str| fixtures::unit_rows(&w.cache_path, "tephra", p).unwrap();
    let broken = rows("broken.rs");
    assert_eq!(broken.kind, "code");
    assert_eq!(
        broken.links,
        vec![
            ("wikilink".to_string(), "@notes:Tephra test".to_string()),
            ("wikilink".to_string(), "@notes:Decoy".to_string())
        ]
    );
    let bin = rows("binaryish.py");
    assert_eq!(bin.kind, "binary");
    assert!(bin.links.is_empty());
    assert_eq!(rows("late-bad-utf8.rs").kind, "binary");
    assert_eq!(rows("good.py").links.len(), 1);
    assert_eq!(rows("good.go").links.len(), 1);

    let mut from: Vec<String> = backlinks(&w.cache_path, "@notes:Vaultonly.md")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    from.sort();
    assert_eq!(from, ["@tephra:good.go", "@tephra:good.py"]);
    w.finish();
}

#[test]
fn e12_a_15_mb_single_line_minified_js_is_skipped_quickly_and_does_not_lose_the_rest() {
    let mut js = String::with_capacity(15 * 1024 * 1024 + 64);
    js.push_str("/* [[@notes:Tephra test]] */");
    while js.len() < 15 * 1024 * 1024 {
        js.push_str("var a=function(b){return b+1};");
    }
    let vault = fixtures::empty_source();
    vault.write("Tephra test.md", "# T\n");
    let repo = fixtures::empty_source();
    repo.write("dist/min.js", &js);
    repo.write("good.rs", "// [[@notes:Tephra test]]\n");

    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", vault.root()).unwrap();
    let started = Instant::now();
    registry.register("tephra", repo.root()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the scan took {:?}",
        started.elapsed()
    );

    let min = fixtures::unit_rows(&cache_path, "tephra", "dist/min.js").unwrap();
    assert_eq!(min.kind, "code", "a skipped file is still a code unit");
    // Over the parse size limit it is never parsed: no links, however
    // its comment reads (memory is asserted in `code_memory.rs`).
    assert!(min.links.is_empty(), "{:?}", min.links);
    // The good file next to it is unaffected.
    let good = fixtures::unit_rows(&cache_path, "tephra", "good.rs").unwrap();
    assert_eq!(good.links.len(), 1);
    registry.unregister("notes").unwrap();
    registry.unregister("tephra").unwrap();
}

// ------------------------------------------------- dispatch / shebang

#[test]
fn a_shebang_only_script_is_code_and_gives_its_comment_links() {
    let w = world(|repo| {
        repo.write(
            "bin/run",
            "#!/usr/bin/env python3\n# [[@notes:Vaultonly]]\nprint('[[@notes:Decoy]]')\n",
        );
        repo.write("bin/deploy", "#!/bin/bash\n# [[@notes:Vaultonly]]\n");
        repo.write(
            "bin/notes",
            "# [[@notes:Vaultonly]]\njust text, no shebang\n",
        );
    });
    let kind = |p: &str| {
        fixtures::unit_rows(&w.cache_path, "tephra", p)
            .unwrap()
            .kind
    };
    assert_eq!(kind("bin/run"), "code");
    assert_eq!(kind("bin/deploy"), "code");
    assert_eq!(kind("bin/notes"), "text-other");
    let from: Vec<String> = backlinks(&w.cache_path, "@notes:Vaultonly.md")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    assert_eq!(from, ["@tephra:bin/deploy", "@tephra:bin/run"]);
    assert!(backlinks(&w.cache_path, "@notes:Decoy.md").is_empty());
    w.finish();
}

#[test]
fn a_file_flipping_between_prose_and_code_is_reparsed_by_the_new_kind() {
    let w = world(|_| {});
    let id = "@notes:Vaultonly.md";
    // A `.txt` (prose): the link is read from anywhere.
    w.repo.write("x.txt", "plain [[@notes:Vaultonly]] text\n");
    assert!(wait_until(Duration::from_secs(20), || {
        backlinks(&w.cache_path, id) == vec![("@tephra:x.txt".into(), "@notes:Vaultonly".into())]
    }));
    // The same name replaced by a `.rs`: only the comment counts.
    w.repo.remove("x.txt");
    w.repo
        .write("x.rs", "const S: &str = \"[[@notes:Vaultonly]]\";\n");
    assert!(
        wait_until(Duration::from_secs(20), || {
            fixtures::unit_rows(&w.cache_path, "tephra", "x.rs").is_some()
                && fixtures::unit_rows(&w.cache_path, "tephra", "x.txt").is_none()
        }),
        "the watcher never swapped x.txt for x.rs"
    );
    assert!(backlinks(&w.cache_path, id).is_empty());
    let _ = &w.vault;
    w.finish();
}
