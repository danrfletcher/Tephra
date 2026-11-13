//! Shared test fixtures for every file in `tests/` — loading a
//! directory of real markdown files into a real `tephra_cache::Cache`
//! through its typed insert API (`register_source`/`insert_unit`/
//! `insert_link`/`insert_tag`/`insert_symbol`/`insert_block`, "PR-2's cache loader"),
//! fed by the real `tephra_parser::parse_unit_bytes` ("PR-3's
//! parser"), rather than hand-built SQL or synthetic graph data
//! (spec: "seeded through PR-2's cache loader rather than hand-built
//! SQL").
//!
//! Each test binary in `tests/` compiles this module separately, so
//! `dead_code` is expected and allowed here rather than per-item —
//! the same convention `tephrad`'s and `tephra-cache`'s own
//! `tests/fixtures`/`tests/common` modules use.
#![allow(dead_code)]

use std::path::Path;

use tephra_cache::Cache;

/// The two fixture source directories committed under
/// `tests/fixtures/`, named after the sources the integration tests
/// register them as.
pub const FEDERATED_VAULT_DIR: &str = "tests/fixtures/federated-vault";
pub const FEDERATED_REPO_DIR: &str = "tests/fixtures/federated-repo";

/// Opens a `Cache` at `<tempdir>/cache.db`, never a real on-disk
/// `~/.local/share/tephra/cache.db`.
pub fn open_temp_cache(dir: &tempfile::TempDir) -> Cache {
    Cache::open(dir.path().join("cache.db")).expect("open cache")
}

/// Recursively lists every `.md`, `.rs` and `.py` file under `dir`, relative to `dir`,
/// with forward-slash separators regardless of platform — the exact
/// `rel_path` shape `tephrad::scan`'s own walk produces, so a fixture
/// loaded this way is indistinguishable, from `tephra-graph`'s
/// perspective, from one loaded by a real scan.
fn list_markdown_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(current).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let loadable = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    ["md", "markdown", "rs", "py"]
                        .iter()
                        .any(|ext| e.eq_ignore_ascii_case(ext))
                })
                .unwrap_or(false);
            if !loadable {
                continue;
            }
            let rel = path.strip_prefix(root).expect("strip_prefix");
            let rel_str: Vec<&str> = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();
            out.push(rel_str.join("/"));
        }
    }

    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Parses `content` as the file `rel_path` (a code grammar by its
/// extension, else prose) and writes the unit with its links, tags,
/// symbols and blocks through the cache's typed insert API — the same
/// order `tephrad::scan::write_unit` uses, so a code file's block IDs
/// name their owning symbol (G13). Returns the new `units.id`.
pub fn write_file_into_cache(cache: &Cache, source_id: i64, rel_path: &str, content: &str) -> i64 {
    let path = Path::new(rel_path);
    let language = tephra_parser::Language::from_path(path, None);
    let parsed = match language {
        Some(language) => tephra_parser::parse_code_unit(path, language, content),
        None => tephra_parser::parse_unit(path, content),
    };

    let mut new_unit = parsed.to_new_unit(source_id, rel_path, None);
    if language.is_some() {
        new_unit.kind = "code";
    }
    let unit_id = cache.insert_unit(&new_unit).expect("insert_unit");
    for link in parsed.to_new_links(unit_id) {
        cache.insert_link(&link).expect("insert_link");
    }
    for tag in parsed.to_new_tags(unit_id) {
        cache.insert_tag(&tag).expect("insert_tag");
    }
    let symbol_ids: Vec<i64> = parsed
        .to_new_symbols(unit_id)
        .iter()
        .map(|symbol| cache.insert_symbol(symbol).expect("insert_symbol"))
        .collect();
    for block in parsed.to_new_blocks_with_symbols(unit_id, &symbol_ids) {
        cache.insert_block(&block).expect("insert_block");
    }
    unit_id
}

/// Registers `source_name` (status `Active`) with these in-memory
/// files (`(rel_path, content)`) and writes each through
/// [`write_file_into_cache`]. Returns the new `sources.id`. For tests
/// that need a small, exact source (a moved block, a duplicate ID)
/// without a directory on disk.
pub fn load_files_into_cache(cache: &Cache, source_name: &str, files: &[(&str, &str)]) -> i64 {
    let source_id = cache
        .register_source(source_name, "/virtual", tephra_cache::SourceStatus::Active)
        .expect("register_source");
    for (rel_path, content) in files {
        write_file_into_cache(cache, source_id, rel_path, content);
    }
    source_id
}

/// Registers `source_name` (status `Active`) rooted at `fixture_dir`,
/// then parses and writes every `.md`, `.rs` and `.py` file under it
/// through the real `tephra-parser` + `tephra-cache` typed API — the
/// same pipeline `tephrad::scan::write_unit` uses, just without the
/// filesystem walk machinery (ignore rules, debounce, chunked
/// commits) this crate has no reason to depend on. Returns the new
/// `sources.id`.
pub fn load_source_into_cache(cache: &Cache, source_name: &str, fixture_dir: &Path) -> i64 {
    let source_id = cache
        .register_source(
            source_name,
            &fixture_dir.to_string_lossy(),
            tephra_cache::SourceStatus::Active,
        )
        .expect("register_source");

    for rel_path in list_markdown_files(fixture_dir) {
        let abs_path = fixture_dir.join(&rel_path);
        let bytes = std::fs::read(&abs_path).expect("read fixture file");
        let content = std::str::from_utf8(&bytes).expect("fixture files are valid UTF-8");
        write_file_into_cache(cache, source_id, &rel_path, content);
    }

    source_id
}

/// Loads both committed fixture directories into one fresh temp
/// cache, as `"notes"` (vault-shaped) and `"atlas-repo"`
/// (repo-shaped) — the two-source federation fixture
/// `tests/integration/federation.rs` and the demo example both use.
pub fn load_federated_fixtures(cache: &Cache) {
    load_source_into_cache(
        cache,
        "notes",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FEDERATED_VAULT_DIR)
            .as_path(),
    );
    load_source_into_cache(
        cache,
        "atlas-repo",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FEDERATED_REPO_DIR)
            .as_path(),
    );
}
