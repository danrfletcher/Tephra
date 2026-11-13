//! F7 fence, structural: link resolution never depends on name search.
//! `tephra-graph/src/resolver.rs` (and every other source file of this
//! crate) must not mention the search module or `unit.search`, so a
//! search ranking can never feed into how `[[Name]]` resolves. A tie
//! stays the ambiguity error. (The behavioural half, a search changing no
//! `unit.resolve_link` output, is `tephrad`'s `tests/search_end_to_end.rs`.)
//!
//! A plain source grep in the style of `fence_no_ui_rpc_deps.rs`'s
//! dependency check: comments are skipped, so the resolver may still
//! explain in prose why it does not do this.

use std::fs;
use std::path::Path;

const FORBIDDEN: &[&str] = &[
    "tephrad",
    "tephra_cache::search",
    "tephra_cache::NameHit",
    "tephra_cache::NameCandidate",
    "search_names",
    "NameHit",
    "NameCandidate",
    "unit.search",
    "fuzzy",
    "crate::search",
    "mod search",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_resolver_does_not_depend_on_the_search_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let resolver = src.join("resolver.rs");
    assert!(resolver.exists(), "resolver.rs moved: update this fence");

    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.contains(&resolver));
    for file in files {
        let code = code_only(&fs::read_to_string(&file).expect("read source"));
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(forbidden),
                "{} depends on search (`{forbidden}`): link resolution must never use name search (F7)",
                file.display()
            );
        }
    }
}

#[test]
fn the_crate_does_not_list_a_search_or_daemon_dependency() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read Cargo.toml");
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        assert!(
            !line.starts_with("tephrad"),
            "tephra-graph must not depend on tephrad"
        );
        assert!(
            !line.contains("fuzzy"),
            "no fuzzy-matching dependency: {line}"
        );
    }
}
