//! `tests/unit/code_languages.rs`: the extension / shebang -> grammar
//! table (G4). Plain inputs, nothing on disk.

use std::path::Path;

use tephra_parser::{Language, EXTENSIONS};

/// The settled mapping, written out independently of the table.
const SETTLED: &[(&str, Language)] = &[
    ("py", Language::Python),
    ("ts", Language::TypeScript),
    ("tsx", Language::Tsx),
    ("js", Language::JavaScript),
    ("mjs", Language::JavaScript),
    ("cjs", Language::JavaScript),
    ("jsx", Language::Jsx),
    ("rs", Language::Rust),
    ("go", Language::Go),
    ("sh", Language::Shell),
    ("bash", Language::Shell),
    ("zsh", Language::Shell),
    ("yml", Language::Yaml),
    ("yaml", Language::Yaml),
    ("toml", Language::Toml),
    ("c", Language::C),
    ("h", Language::C),
    ("cpp", Language::Cpp),
    ("cc", Language::Cpp),
    ("cxx", Language::Cpp),
    ("hpp", Language::Cpp),
    ("java", Language::Java),
    ("swift", Language::Swift),
    ("lua", Language::Lua),
];

#[test]
fn every_settled_extension_maps_to_its_language() {
    for (ext, lang) in SETTLED {
        let path = format!("dir/file.{ext}");
        assert_eq!(
            Language::from_path(Path::new(&path), None),
            Some(*lang),
            "{path}"
        );
        assert_eq!(Language::from_extension(ext), Some(*lang), "{ext}");
    }
}

#[test]
fn the_public_table_is_exactly_the_settled_mapping() {
    let table: Vec<(&str, Language)> = EXTENSIONS.to_vec();
    assert_eq!(table, SETTLED.to_vec());
}

#[test]
fn every_language_is_reachable_from_some_extension() {
    for lang in Language::ALL {
        assert!(
            SETTLED.iter().any(|(_, l)| *l == lang),
            "{lang:?} has no extension"
        );
    }
}

#[test]
fn extensions_match_case_insensitively() {
    for (path, lang) in [
        ("x.RS", Language::Rust),
        ("x.Py", Language::Python),
        ("x.TSX", Language::Tsx),
        ("x.Yml", Language::Yaml),
        ("src/Main.JAVA", Language::Java),
        ("a.H", Language::C),
        ("a.HPP", Language::Cpp),
    ] {
        assert_eq!(
            Language::from_path(Path::new(path), None),
            Some(lang),
            "{path}"
        );
    }
}

#[test]
fn an_unknown_extension_is_not_a_language() {
    for path in [
        "main.kt",
        "script.rb",
        "data.json",
        "notes.ini",
        "a.txt",
        "a.md",
        "a.pdf",
        "a.",
        "Makefile.bak",
    ] {
        assert_eq!(Language::from_path(Path::new(path), None), None, "{path}");
    }
}

#[test]
fn shebang_only_files_map_to_shell_or_python() {
    for (line, lang) in [
        ("#!/bin/bash", Language::Shell),
        ("#!/bin/sh", Language::Shell),
        ("#!/bin/zsh", Language::Shell),
        ("#!/usr/bin/env bash", Language::Shell),
        ("#!/usr/bin/env python3", Language::Python),
        ("#!/usr/bin/python", Language::Python),
        ("#!/usr/bin/env python3.11", Language::Python),
        ("#!/usr/bin/env -S FOO=1 python3 -u", Language::Python),
        ("#!/usr/bin/env node", Language::JavaScript),
        ("#!/usr/bin/lua5.4", Language::Lua),
        ("#!/bin/bash\r", Language::Shell),
        ("#!/bin/bash\nsecond line", Language::Shell),
    ] {
        assert_eq!(
            Language::from_path(Path::new("script"), Some(line)),
            Some(lang),
            "{line:?}"
        );
        // A dot-file has no extension either.
        assert_eq!(
            Language::from_path(Path::new(".bashrc"), Some(line)),
            Some(lang),
            ".bashrc {line:?}"
        );
    }
}

#[test]
fn a_shebang_for_a_language_without_a_grammar_is_not_a_language() {
    for line in [
        "#!/usr/bin/perl",
        "#!/usr/bin/env ruby",
        "#!/usr/bin/pythonista",
        "no shebang",
        "",
        "# !/bin/bash",
    ] {
        assert_eq!(
            Language::from_path(Path::new("script"), Some(line)),
            None,
            "{line:?}"
        );
    }
}

#[test]
fn a_file_without_an_extension_and_without_a_first_line_has_no_language() {
    assert_eq!(Language::from_path(Path::new("Makefile"), None), None);
}

#[test]
fn an_extension_wins_over_a_shebang_and_an_unknown_one_ignores_it() {
    // `.py` is Python even when the first line names bash.
    assert_eq!(
        Language::from_path(Path::new("a.py"), Some("#!/bin/bash")),
        Some(Language::Python)
    );
    // A `.txt` that starts with a shebang is not code.
    assert_eq!(
        Language::from_path(Path::new("a.txt"), Some("#!/bin/bash")),
        None
    );
}
