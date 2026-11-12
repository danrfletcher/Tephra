//! File-kind classification (G2) and the size cap (G7).
//!
//! Every non-ignored file in a source becomes a unit, labelled with one
//! of five kinds:
//!
//! - `prose`: extension `md`, `markdown`, `txt`, `org`, `rst`, `adoc`.
//! - `code`: an extension with a bundled tree-sitter grammar (the G4
//!   list, [`CODE_EXTENSIONS`]; the grammars themselves arrive in a
//!   later PR, this is only the static extension table), or, for a file
//!   with *no* extension, a recognised shebang interpreter.
//! - `document`: `pdf`, `docx`, `pptx`.
//! - `text-other`: anything else whose first [`SNIFF_BYTES`] bytes have
//!   no NUL byte and decode as UTF-8.
//! - `binary`: everything else.
//!
//! Extensions match case-insensitively. A `prose` or `code` file whose
//! first 8 KB contains a NUL byte is `binary`; the one exception is
//! prose that starts with a UTF-16 byte-order mark and decodes as
//! UTF-16, which is kept as `prose` (UTF-16 text is full of NULs).
//! Other non-UTF-8 prose gets no special treatment: it falls through to
//! the same text check as an unknown extension, which it fails, so it
//! is `binary`. `document` files are never sniffed: PDF and Office
//! formats are binary containers by nature.
//!
//! The classifier is pure: [`classify`] takes the file name and the
//! first bytes of the file and never touches the disk, so the whole
//! rule table is unit-tested from byte arrays. [`read_head`] is the one
//! helper that reads a file, and reads at most `SNIFF_BYTES + 1` bytes.
//!
//! The kind of an empty file follows the same rules, so a 0-byte file
//! with an unknown extension (`.png` included, because the text check
//! passes on no bytes) is `text-other`, while a 0-byte `.md` is `prose`
//! and a 0-byte `.pdf` is `document`.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// How many leading bytes the NUL / UTF-8 / shebang checks look at.
pub const SNIFF_BYTES: usize = 8 * 1024;

/// Files larger than this many bytes are indexed by name only (G7).
/// A file of exactly this size is parsed normally.
pub const MAX_INDEXED_BYTES: u64 = 20 * 1024 * 1024;

/// A unit's kind. The string forms are the exact values the cache's
/// `units.kind` CHECK constraint allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Prose,
    Code,
    Document,
    TextOther,
    Binary,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Prose => "prose",
            FileKind::Code => "code",
            FileKind::Document => "document",
            FileKind::TextOther => "text-other",
            FileKind::Binary => "binary",
        }
    }
}

/// A language with a bundled grammar (G4). Only the extension and
/// shebang tables exist in this PR; nothing parses with them yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Rust,
    Go,
    Shell,
    Yaml,
    Toml,
    C,
    Cpp,
    Java,
    Swift,
    Lua,
}

const PROSE_EXTENSIONS: [&str; 6] = ["md", "markdown", "txt", "org", "rst", "adoc"];
const DOCUMENT_EXTENSIONS: [&str; 3] = ["pdf", "docx", "pptx"];

/// The static extension table for `code` (lower-case, no dot).
pub const CODE_EXTENSIONS: [(&str, Language); 24] = [
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

/// The extension of `file_name` (text after the last `.`, lower-cased),
/// or `None` when there is none. A leading dot alone (`.bashrc`) and a
/// trailing dot (`notes.`) do not make an extension, matching
/// [`Path::extension`].
fn extension_of(file_name: &str) -> Option<String> {
    Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The language the extension table assigns to `file_name`, if any.
pub fn language_for_extension(file_name: &str) -> Option<Language> {
    let ext = extension_of(file_name)?;
    CODE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, l)| *l)
}

/// The language a `#!` line names, if its interpreter has a bundled
/// grammar: `sh`/`bash`/`zsh`/`dash`/`ksh` -> Shell, `python*` ->
/// Python, `node`/`nodejs` -> JavaScript, `lua*` -> Lua. Handles
/// `#!/usr/bin/env python3` and `env -S VAR=1 python3 -u`. An
/// interpreter with no grammar (`perl`, `ruby`) is `None`.
pub fn language_for_shebang(head: &[u8]) -> Option<Language> {
    let rest = head.strip_prefix(b"#!")?;
    let line_end = rest
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(rest.len());
    let line = std::str::from_utf8(&rest[..line_end]).ok()?;
    let mut tokens = line.split_whitespace();
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
        s if s == "python" || s.starts_with("python") && s[6..].chars().all(is_version_char) => {
            Some(Language::Python)
        }
        s if s == "lua" || s.starts_with("lua") && s[3..].chars().all(is_version_char) => {
            Some(Language::Lua)
        }
        _ => None,
    }
}

fn is_version_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

/// Classifies one file from its name and its leading bytes. `head` is
/// the start of the file: only the first [`SNIFF_BYTES`] bytes are
/// looked at, and a slice longer than that tells the classifier the
/// file continues past the window (so a UTF-8 character cut off by the
/// window edge is not mistaken for invalid UTF-8). Pass at most
/// `SNIFF_BYTES + 1` bytes ([`read_head`] does).
pub fn classify(file_name: &str, head: &[u8]) -> FileKind {
    let truncated = head.len() > SNIFF_BYTES;
    let window = &head[..head.len().min(SNIFF_BYTES)];
    let ext = extension_of(file_name);
    let ext = ext.as_deref();

    if let Some(ext) = ext {
        if DOCUMENT_EXTENSIONS.contains(&ext) {
            return FileKind::Document;
        }
        if PROSE_EXTENSIONS.contains(&ext) {
            if has_utf16_bom(window) {
                return if is_valid_utf16(window, truncated) {
                    FileKind::Prose
                } else {
                    FileKind::Binary
                };
            }
            return if is_text(window, truncated) {
                FileKind::Prose
            } else {
                FileKind::Binary
            };
        }
        if CODE_EXTENSIONS.iter().any(|(e, _)| *e == ext) {
            return if is_text(window, truncated) {
                FileKind::Code
            } else {
                FileKind::Binary
            };
        }
    }

    // Unknown extension, or none at all.
    if !is_text(window, truncated) {
        return FileKind::Binary;
    }
    if ext.is_none() && language_for_shebang(window).is_some() {
        return FileKind::Code;
    }
    FileKind::TextOther
}

/// The kind a cache rebuild gives a carried-over offline row, from the
/// path alone (no file read, because the source is offline): by
/// extension, `text-other` for anything the table doesn't know. The
/// `needs_rescan` flag on that row marks the guess as provisional.
pub fn infer_kind_from_path(rel_path: &str) -> &'static str {
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    match extension_of(file_name).as_deref() {
        Some(e) if DOCUMENT_EXTENSIONS.contains(&e) => FileKind::Document,
        Some(e) if PROSE_EXTENSIONS.contains(&e) => FileKind::Prose,
        Some(e) if CODE_EXTENSIONS.iter().any(|(c, _)| *c == e) => FileKind::Code,
        _ => FileKind::TextOther,
    }
    .as_str()
}

/// The text check: no NUL byte, and valid UTF-8. When `truncated`, a
/// multi-byte character cut off at the very end of the window is fine.
fn is_text(window: &[u8], truncated: bool) -> bool {
    if window.contains(&0) {
        return false;
    }
    match std::str::from_utf8(window) {
        Ok(_) => true,
        Err(e) => truncated && e.error_len().is_none(),
    }
}

fn has_utf16_bom(window: &[u8]) -> bool {
    window.starts_with(&[0xFF, 0xFE]) || window.starts_with(&[0xFE, 0xFF])
}

fn utf16_units(bytes: &[u8], little_endian: bool) -> impl Iterator<Item = u16> + '_ {
    // Any odd trailing byte is dropped; callers check for it first.
    bytes.as_chunks::<2>().0.iter().map(move |&pair| {
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    })
}

/// Whether `window` (which starts with a UTF-16 BOM) decodes as UTF-16.
/// When `truncated`, a trailing odd byte or a high surrogate cut off at
/// the window edge is tolerated.
fn is_valid_utf16(window: &[u8], truncated: bool) -> bool {
    let little_endian = window[0] == 0xFF;
    let body = &window[2..];
    let odd_tail = body.len() % 2 == 1;
    if odd_tail && !truncated {
        return false;
    }
    let mut decoded = char::decode_utf16(utf16_units(body, little_endian)).peekable();
    while let Some(item) = decoded.next() {
        if item.is_err() {
            // An unpaired high surrogate as the very last unit is just
            // a pair split by the window edge.
            let is_last = decoded.peek().is_none();
            if !(truncated && is_last) {
                return false;
            }
        }
    }
    true
}

/// Decodes a whole file as UTF-16 when it starts with a BOM (LE or BE),
/// dropping the BOM. `None` if there is no BOM or the bytes are not
/// valid UTF-16. Used to feed UTF-16 Markdown to the (UTF-8) parser.
pub fn decode_utf16_with_bom(bytes: &[u8]) -> Option<String> {
    if !has_utf16_bom(bytes) || (bytes.len() - 2) % 2 == 1 {
        return None;
    }
    let little_endian = bytes[0] == 0xFF;
    char::decode_utf16(utf16_units(&bytes[2..], little_endian))
        .collect::<Result<String, _>>()
        .ok()
}

/// Reads up to `SNIFF_BYTES + 1` leading bytes of `path` (the extra
/// byte tells [`classify`] the file continues past the window). Never
/// reads more, so it is safe to call on a file of any size.
pub fn read_head(path: &Path) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(SNIFF_BYTES + 1);
    File::open(path)?
        .take(SNIFF_BYTES as u64 + 1)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use FileKind::*;

    fn utf16(text: &str, little_endian: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            out.extend(if little_endian {
                [0xFF, 0xFE]
            } else {
                [0xFE, 0xFF]
            });
        }
        for u in text.encode_utf16() {
            out.extend(if little_endian {
                u.to_le_bytes()
            } else {
                u.to_be_bytes()
            });
        }
        out
    }

    /// `len` bytes of `a`, with `byte` written at each index in `at`.
    fn filled(len: usize, at: &[(usize, u8)]) -> Vec<u8> {
        let mut v = vec![b'a'; len];
        for (i, b) in at {
            v[*i] = *b;
        }
        v
    }

    #[test]
    fn every_g2_extension_maps_to_its_kind_in_any_case() {
        let text = b"hello\n".as_slice();
        let mut cases: Vec<(String, FileKind)> = Vec::new();
        for ext in ["md", "markdown", "txt", "org", "rst", "adoc"] {
            cases.push((format!("n.{ext}"), Prose));
            cases.push((format!("n.{}", ext.to_uppercase()), Prose));
        }
        for ext in ["pdf", "docx", "pptx"] {
            cases.push((format!("n.{ext}"), Document));
            cases.push((format!("n.{}", ext.to_uppercase()), Document));
        }
        for (ext, _) in CODE_EXTENSIONS {
            cases.push((format!("n.{ext}"), Code));
            cases.push((format!("n.{}", ext.to_uppercase()), Code));
        }
        // Mixed case, and names with dots, spaces and non-ASCII.
        cases.push(("Notes.Md".into(), Prose));
        cases.push(("a b.PDF".into(), Document));
        cases.push(("café notes.txt".into(), Prose));
        cases.push(("archive.tar.gz.md".into(), Prose));
        cases.push(("main.RS".into(), Code));
        for (name, expected) in cases {
            assert_eq!(classify(&name, text), expected, "{name}");
        }
    }

    #[test]
    fn the_alias_table_maps_to_the_documented_languages() {
        let cases = [
            ("a.h", Language::C),
            ("a.c", Language::C),
            ("a.hpp", Language::Cpp),
            ("a.cc", Language::Cpp),
            ("a.cpp", Language::Cpp),
            ("a.cxx", Language::Cpp),
            ("a.yml", Language::Yaml),
            ("a.yaml", Language::Yaml),
            ("a.mjs", Language::JavaScript),
            ("a.cjs", Language::JavaScript),
            ("a.js", Language::JavaScript),
            ("a.zsh", Language::Shell),
            ("a.bash", Language::Shell),
            ("a.sh", Language::Shell),
            ("a.tsx", Language::Tsx),
            ("a.jsx", Language::Jsx),
            ("a.ts", Language::TypeScript),
            ("a.py", Language::Python),
            ("a.rs", Language::Rust),
            ("a.go", Language::Go),
            ("a.toml", Language::Toml),
            ("a.java", Language::Java),
            ("a.swift", Language::Swift),
            ("a.lua", Language::Lua),
            ("A.CPP", Language::Cpp),
        ];
        for (name, lang) in cases {
            assert_eq!(language_for_extension(name), Some(lang), "{name}");
        }
        for name in ["a.txt", "a.md", "Makefile", "a.png", ".bashrc"] {
            assert_eq!(language_for_extension(name), None, "{name}");
        }
    }

    #[test]
    fn unknown_extensions_use_the_text_check() {
        let cases: Vec<(&str, Vec<u8>, FileKind)> = vec![
            ("data.csv", b"a,b\n1,2\n".to_vec(), TextOther),
            ("Cargo.lock", b"# lock\n".to_vec(), TextOther),
            ("LICENSE", b"MIT\n".to_vec(), TextOther),
            ("notes", b"just text".to_vec(), TextOther),
            (".gitignore", b"target\n".to_vec(), TextOther),
            (".bashrc", b"export A=1\n".to_vec(), TextOther),
            ("trailing.", b"x".to_vec(), TextOther),
            ("héllo.xyz", "é\n".as_bytes().to_vec(), TextOther),
            ("photo.png", vec![0x89, b'P', b'N', b'G', 0, 1, 2], Binary),
            ("blob", vec![0, 1, 2, 3], Binary),
            ("a.bin", vec![0xFF, 0xFE, 0xFD], Binary),
            ("latin1.xyz", vec![b'c', b'a', b'f', 0xE9], Binary),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(classify(name, &bytes), expected, "{name}");
        }
    }

    #[test]
    fn empty_files_follow_the_same_rules() {
        // Pinned (spec): the text check passes on no bytes, so an
        // unknown extension is text-other, `.png` included.
        assert_eq!(classify("empty", b""), TextOther);
        assert_eq!(classify("empty.png", b""), TextOther);
        assert_eq!(classify("empty.xyz", b""), TextOther);
        // Known extensions keep their kind.
        assert_eq!(classify("empty.md", b""), Prose);
        assert_eq!(classify("empty.txt", b""), Prose);
        assert_eq!(classify("empty.rs", b""), Code);
        assert_eq!(classify("empty.pdf", b""), Document);
    }

    #[test]
    fn a_nul_byte_in_the_first_8k_makes_prose_code_and_unknown_binary() {
        let window = SNIFF_BYTES;
        for name in ["n.txt", "n.md", "n.py", "n.rs", "n.xyz", "noext"] {
            // Inside the window: first byte, last byte of the window.
            for at in [0, 1, window - 1] {
                let bytes = filled(window + 10, &[(at, 0)]);
                assert_eq!(classify(name, &bytes), Binary, "{name} NUL at {at}");
            }
            // Byte 8192 is the first one *outside* the window.
            for at in [window, window + 1] {
                let bytes = filled(window + 10, &[(at, 0)]);
                let expected = classify(name, &filled(window + 10, &[]));
                assert_ne!(expected, Binary, "{name}");
                assert_eq!(classify(name, &bytes), expected, "{name} NUL at {at}");
            }
        }
        // Exactly 8192 bytes with a NUL in the last one: still inside.
        assert_eq!(
            classify("n.txt", &filled(window, &[(window - 1, 0)])),
            Binary
        );
    }

    #[test]
    fn documents_are_never_sniffed() {
        for name in ["a.pdf", "a.DOCX", "a.pptx"] {
            assert_eq!(
                classify(name, &[0x50, 0x4B, 0, 0, 0xFF]),
                Document,
                "{name}"
            );
        }
    }

    #[test]
    fn extensionless_files_pick_code_from_the_shebang() {
        let cases: &[(&str, &[u8], FileKind)] = &[
            ("run", b"#!/bin/bash\necho hi\n", Code),
            ("run", b"#!/bin/sh\n", Code),
            ("run", b"#!/usr/bin/env python3\nprint(1)\n", Code),
            ("run", b"#!/usr/bin/python\n", Code),
            ("run", b"#!/usr/bin/env python3.12\n", Code),
            ("run", b"#!/usr/bin/env -S python3 -u\n", Code),
            ("run", b"#!/usr/bin/env FOO=1 bash\n", Code),
            ("run", b"#!/usr/bin/env node\n", Code),
            ("run", b"#!/bin/zsh\r\nls\r\n", Code),
            ("run", b"#!/usr/bin/env lua\n", Code),
            // An interpreter with no bundled grammar: text check only.
            ("run", b"#!/usr/bin/perl\n", TextOther),
            ("run", b"#!/usr/bin/env ruby\n", TextOther),
            ("run", b"#!/usr/bin/pythonista\n", TextOther),
            // No shebang, or not at byte 0: text check only.
            ("run", b"echo hi\n", TextOther),
            ("run", b"\n#!/bin/bash\n", TextOther),
            ("run", b" #!/bin/bash\n", TextOther),
            ("run", b"#! \n", TextOther),
            ("run", b"#!", TextOther),
            // A shebang with a NUL in the window is still binary.
            ("run", b"#!/bin/bash\n\0\0", Binary),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(
                classify(name, bytes),
                *expected,
                "{name}: {:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        // The shebang only applies to files with no extension: a
        // `.txt` or unknown-extension script keeps its extension kind.
        assert_eq!(classify("run.txt", b"#!/bin/bash\n"), Prose);
        assert_eq!(classify("run.xyz", b"#!/bin/bash\n"), TextOther);
        // And the language lookup agrees.
        assert_eq!(
            language_for_shebang(b"#!/usr/bin/env python3\n"),
            Some(Language::Python)
        );
        assert_eq!(
            language_for_shebang(b"#!/bin/bash\n"),
            Some(Language::Shell)
        );
        assert_eq!(language_for_shebang(b"no shebang"), None);
    }

    #[test]
    fn invalid_utf8_is_binary_except_a_character_cut_by_the_window() {
        // Invalid in the middle: binary for prose, code and unknown.
        for name in ["n.txt", "n.md", "n.rs", "n.xyz"] {
            assert_eq!(classify(name, &[b'a', 0xFF, b'b']), Binary, "{name}");
            assert_eq!(classify(name, &[b'a', 0xC3]), Binary, "{name} (whole file)");
        }
        // A 2-byte character straddling the window edge in a longer
        // file is valid (the file continues past the window).
        let mut long = filled(SNIFF_BYTES - 1, &[]);
        long.extend("é".as_bytes()); // bytes 8191 and 8192
        long.extend(b"tail");
        assert_eq!(classify("n.txt", &long), Prose);
        assert_eq!(classify("n.xyz", &long), TextOther);
        // The same cut in a file that really ends there is invalid.
        let mut short = filled(SNIFF_BYTES - 1, &[]);
        short.push(0xC3);
        assert_eq!(classify("n.txt", &short), Binary);
        // Invalid bytes only *past* the window are not looked at.
        let mut past = filled(SNIFF_BYTES, &[]);
        past.push(0xFF);
        assert_eq!(classify("n.txt", &past), Prose);
    }

    #[test]
    fn utf16_prose_with_a_bom_is_prose_and_without_a_bom_is_binary() {
        let text = "héllo wörld 😀\nsecond line\n";
        for (name, expected) in [("n.txt", Prose), ("n.MD", Prose), ("n.rst", Prose)] {
            for little_endian in [true, false] {
                assert_eq!(
                    classify(name, &utf16(text, little_endian, true)),
                    expected,
                    "{name} le={little_endian}"
                );
                // Without the BOM the NUL bytes make it binary.
                assert_eq!(
                    classify(name, &utf16(text, little_endian, false)),
                    Binary,
                    "{name} le={little_endian} no BOM"
                );
            }
        }
        // A BOM followed by garbage (lone surrogate / odd length) is binary.
        assert_eq!(classify("n.txt", &[0xFF, 0xFE, 0x00, 0xD8]), Binary);
        assert_eq!(classify("n.txt", &[0xFF, 0xFE, b'a']), Binary);
        // A BOM on a non-prose file is not special: NULs make it binary.
        assert_eq!(classify("n.rs", &utf16("fn main() {}", true, true)), Binary);
        assert_eq!(classify("n.xyz", &utf16("abc", true, true)), Binary);
        // A UTF-16 file cut mid-pair by the window is still prose.
        let mut long = utf16(&"a".repeat(SNIFF_BYTES), true, true);
        long.truncate(SNIFF_BYTES + 1); // one byte past the window
        assert_eq!(classify("n.txt", &long), Prose);
    }

    #[test]
    fn utf16_markdown_decodes_to_the_original_text() {
        let text = "# Título\n\n[[café]] 😀\n";
        for le in [true, false] {
            assert_eq!(
                decode_utf16_with_bom(&utf16(text, le, true)).as_deref(),
                Some(text)
            );
        }
        assert_eq!(decode_utf16_with_bom(&utf16(text, true, false)), None);
        assert_eq!(decode_utf16_with_bom(b"plain"), None);
        assert_eq!(decode_utf16_with_bom(&[0xFF, 0xFE, b'a']), None);
        assert_eq!(decode_utf16_with_bom(&[0xFF, 0xFE, 0x00, 0xD8]), None);
    }

    #[test]
    fn a_utf8_bom_is_just_text() {
        assert_eq!(classify("n.md", b"\xEF\xBB\xBF# hi\n"), Prose);
        assert_eq!(classify("n.xyz", b"\xEF\xBB\xBFhi"), TextOther);
    }

    #[test]
    fn names_keep_their_exact_form_and_odd_names_classify() {
        let cases: &[(&str, FileKind)] = &[
            ("café notes.txt", Prose),
            ("a b.PDF", Document),
            ("日本語.md", Prose),
            ("weird name with.dots.and spaces.RS", Code),
            ("no extension at all", TextOther),
            (".hidden.md", Prose),
            ("a.", TextOther),
        ];
        for (name, expected) in cases {
            assert_eq!(classify(name, b"x"), *expected, "{name}");
        }
    }

    #[test]
    fn infer_kind_from_path_uses_the_extension_only() {
        let cases = [
            ("notes/a.md", "prose"),
            ("a.TXT", "prose"),
            ("src/main.RS", "code"),
            ("Cargo.toml", "code"),
            ("docs/Report.PDF", "document"),
            ("Cargo.lock", "text-other"),
            ("photo.png", "text-other"),
            ("run", "text-other"),
            ("dir.md/inner", "text-other"),
            ("café/a b.pptx", "document"),
        ];
        for (path, expected) in cases {
            assert_eq!(infer_kind_from_path(path), expected, "{path}");
        }
    }

    #[test]
    fn kind_strings_match_the_cache_check_constraint() {
        let all = [Prose, Code, Document, TextOther, Binary];
        let strings: Vec<_> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            strings,
            ["prose", "code", "document", "text-other", "binary"]
        );
    }

    #[test]
    fn size_cap_is_exactly_20_mib() {
        assert_eq!(MAX_INDEXED_BYTES, 20 * 1_048_576);
    }
}
