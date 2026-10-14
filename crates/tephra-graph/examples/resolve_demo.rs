//! Checkpoints 2 & 3 (PR-5 spec): a small CLI harness that builds a
//! `UnitGraph` from the two committed fixtures and resolves
//! `[[@atlas-repo:README]]` against it directly — exercising
//! `unit.resolve_link` exactly as GP5 describes it, printing the
//! resolved metadata and outgoing edges, and separately printing the
//! E4 tie-break error for a deliberately ambiguous target.
//!
//! Usage:
//!   cargo run -p tephra-graph --example resolve_demo
//!   cargo run -p tephra-graph --example resolve_demo -- ambiguous

use std::path::Path;

use tephra_cache::{Cache, SourceStatus};
use tephra_graph::UnitGraph;

const FEDERATED_VAULT_DIR: &str = "tests/fixtures/federated-vault";
const FEDERATED_REPO_DIR: &str = "tests/fixtures/federated-repo";

fn list_markdown_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(current).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let is_markdown = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
                .unwrap_or(false);
            if !is_markdown {
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

fn load_source(cache: &Cache, source_name: &str, fixture_dir: &Path) {
    let source_id = cache
        .register_source(
            source_name,
            &fixture_dir.to_string_lossy(),
            SourceStatus::Active,
        )
        .expect("register_source");
    for rel_path in list_markdown_files(fixture_dir) {
        let abs_path = fixture_dir.join(&rel_path);
        let bytes = std::fs::read(&abs_path).expect("read fixture file");
        let parsed =
            tephra_parser::parse_unit_bytes(&abs_path, &bytes).expect("fixture files are UTF-8");
        let unit_id = cache
            .insert_unit(&parsed.to_new_unit(source_id, &rel_path, None))
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
}

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tmp_db = std::env::temp_dir().join(format!(
        "tephra-graph-resolve-demo-{}.db",
        std::process::id()
    ));
    let cache = Cache::open(&tmp_db).expect("open temp cache");

    load_source(&cache, "notes", &manifest_dir.join(FEDERATED_VAULT_DIR));
    load_source(&cache, "atlas-repo", &manifest_dir.join(FEDERATED_REPO_DIR));

    let graph = UnitGraph::build_from_cache(&cache).expect("build_from_cache");

    let ambiguous_mode = std::env::args().nth(1).as_deref() == Some("ambiguous");

    if ambiguous_mode {
        println!("Resolving ambiguous target [[Guide]] within source \"atlas-repo\":");
        match graph.resolve_link("[[Guide]]", Some("atlas-repo")) {
            Ok(resolved) => println!("unexpectedly resolved: {resolved:#?}"),
            Err(e) => println!("resolve_link error (expected, E4 tie-break): {e}"),
        }
    } else {
        println!("Resolving [[@atlas-repo:README]]:");
        match graph.resolve_link("[[@atlas-repo:README]]", None) {
            Ok(resolved) => {
                println!("unit:");
                println!("  id             = {}", resolved.unit.id);
                println!("  source         = {}", resolved.unit.source);
                println!("  rel_path       = {}", resolved.unit.rel_path);
                println!("  tags           = {:?}", resolved.unit.tags);
                println!("  blocks         = {:?}", resolved.unit.blocks);
                println!("  offline        = {}", resolved.unit.offline);
                println!("outgoing edges ({}):", resolved.edges.len());
                for edge in &resolved.edges {
                    println!("  {} --[{}]--> {:?}", edge.from, edge.kind, edge.to);
                }
            }
            Err(e) => println!("resolve_link error: {e}"),
        }
    }

    let _ = std::fs::remove_file(&tmp_db);
}
