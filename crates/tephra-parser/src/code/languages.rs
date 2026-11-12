//! The one extension / shebang -> grammar table (G4).
//!
//! `tephrad`'s `code` file-kind check calls [`Language::from_path`] and
//! [`Language::from_extension`] rather than keeping a second list, so
//! "this file has a bundled grammar" and "this file is `code`" can
//! never disagree.
//!
//! Extensions match case-insensitively (`.RS`, `.Py`). A file with no
//! extension gets its grammar from its shebang line; a file with an
//! extension that is not in the table never does (a `notes.txt` that
//! starts with `#!/bin/bash` is prose, not shell).

use std::path::Path;

/// A language with a bundled tree-sitter grammar.
///
/// TypeScript and TSX are separate grammars, as are JavaScript and
/// JSX at the table level: the JavaScript grammar reads JSX, so
/// [`Language::Jsx`] and [`Language::JavaScript`] load the same
/// parser, but the extension table keeps them apart so a caller can
/// tell which one a file was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Rust,
    Go,
    /// bash / sh / zsh (one grammar, tree-sitter-bash).
    Shell,
    Yaml,
    Toml,
    C,
    Cpp,
    Java,
    Swift,
    Lua,
}

/// The extension table (lower-case, no dot). `.h` is C, `.hpp` and
/// `.cc`/`.cpp`/`.cxx` are C++.
pub const EXTENSIONS: [(&str, Language); 24] = [
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

impl Language {
    /// Every language, in table order of first appearance.
    pub const ALL: [Language; 15] = [
        Language::Python,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Jsx,
        Language::Rust,
        Language::Go,
        Language::Shell,
        Language::Yaml,
        Language::Toml,
        Language::C,
        Language::Cpp,
        Language::Java,
        Language::Swift,
        Language::Lua,
    ];

    /// The language an extension (no dot, any case) maps to.
    pub fn from_extension(ext: &str) -> Option<Language> {
        EXTENSIONS
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(ext))
            .map(|(_, l)| *l)
    }

    /// The language of the file at `path`, given the file's first
    /// line (`None` when the caller has not read it).
    ///
    /// A path with an extension is decided by the extension alone: a
    /// known one gives its language, an unknown one gives `None`
    /// whatever the first line says. Only a path with *no* extension
    /// (`script`, `.bashrc`) consults `first_line` for a `#!` shebang.
    pub fn from_path(path: &Path, first_line: Option<&str>) -> Option<Language> {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => Language::from_extension(ext),
            None => Language::from_shebang(first_line?),
        }
    }

    /// The language a `#!` line names, if its interpreter has a
    /// bundled grammar: `sh`/`bash`/`zsh`/`dash`/`ksh` -> Shell,
    /// `python*` -> Python, `node`/`nodejs` -> JavaScript, `lua*` ->
    /// Lua. Handles `#!/usr/bin/env python3` and
    /// `env -S VAR=1 python3 -u`. An interpreter with no grammar
    /// (`perl`, `ruby`) is `None`. Only the first line of `line` is
    /// read.
    pub fn from_shebang(line: &str) -> Option<Language> {
        let line = line.trim_start_matches('\u{FEFF}');
        let rest = line.strip_prefix("#!")?;
        let rest = rest.lines().next().unwrap_or("");
        let mut tokens = rest.split_whitespace();
        let program = tokens.next()?;
        let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
        let mut interpreter = base(program);
        if interpreter == "env" {
            // Skip env's own flags and `NAME=value` assignments.
            interpreter = tokens
                .find(|t| !t.starts_with('-') && !t.contains('='))
                .map(base)?;
        }
        match interpreter.as_str() {
            "sh" | "bash" | "zsh" | "dash" | "ksh" => Some(Language::Shell),
            "node" | "nodejs" => Some(Language::JavaScript),
            s if versioned(s, "python") => Some(Language::Python),
            s if versioned(s, "lua") => Some(Language::Lua),
            _ => None,
        }
    }

    /// A stable lower-case name, for logs and snapshots.
    pub fn name(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Jsx => "jsx",
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Shell => "shell",
            Language::Yaml => "yaml",
            Language::Toml => "toml",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Java => "java",
            Language::Swift => "swift",
            Language::Lua => "lua",
        }
    }

    /// The compiled-in tree-sitter grammar.
    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            // The JavaScript grammar includes JSX.
            Language::JavaScript | Language::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Shell => tree_sitter_bash::LANGUAGE.into(),
            Language::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Language::Java => tree_sitter_java::LANGUAGE.into(),
            Language::Swift => tree_sitter_swift::LANGUAGE.into(),
            Language::Lua => tree_sitter_lua::LANGUAGE.into(),
        }
    }

    /// Whether a tree-sitter node of `kind` is a comment in this
    /// grammar. (Rust and Java name theirs `line_comment` /
    /// `block_comment`, Swift has `comment` and `multiline_comment`,
    /// every other grammar uses `comment`.)
    pub(crate) fn is_comment_kind(self, kind: &str) -> bool {
        match self {
            Language::Rust | Language::Java => kind == "line_comment" || kind == "block_comment",
            Language::Swift => kind == "comment" || kind == "multiline_comment",
            _ => kind == "comment",
        }
    }
}

/// `s` is `base` or `base` followed only by version characters
/// (`python`, `python3`, `python3.11`).
fn versioned(s: &str, base: &str) -> bool {
    s.strip_prefix(base)
        .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}
