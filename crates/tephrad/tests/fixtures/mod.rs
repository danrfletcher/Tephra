//! Shared test fixtures for every file in `tests/` (spec: "a shared
//! `fixtures::temp_source()` helper producing vault-shaped and
//! repo-shaped temp directories per test, cleaned up on drop").
//!
//! Each test binary in `tests/` compiles this module separately, so
//! `dead_code` is expected and allowed here rather than per-item —
//! same convention `tephra-cache/tests/common/mod.rs` uses.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

use tempfile::TempDir;

/// A temp directory fixture, auto-deleted on drop (via the underlying
/// `TempDir`) — no real daemon process or network access needed to
/// build or use one.
pub struct TempSource {
    dir: TempDir,
}

impl TempSource {
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `contents` to `rel_path` under this fixture's root,
    /// creating any missing parent directories first.
    pub fn write(&self, rel_path: &str, contents: &str) {
        let full = self.root().join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create_dir_all");
        }
        fs::write(full, contents).expect("write fixture file");
    }

    /// Like [`TempSource::write`], for bytes that aren't UTF-8 text.
    pub fn write_bytes(&self, rel_path: &str, contents: &[u8]) {
        let full = self.root().join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create_dir_all");
        }
        fs::write(full, contents).expect("write fixture file");
    }

    pub fn remove(&self, rel_path: &str) {
        fs::remove_file(self.root().join(rel_path)).expect("remove fixture file");
    }

    pub fn mkdir(&self, rel_path: &str) {
        fs::create_dir_all(self.root().join(rel_path)).expect("mkdir");
    }
}

/// A vault-shaped fixture: `one.md` carries one wikilink
/// (`[[Two]]`), one tag (`#project`), and one block anchor
/// (`^block1`); `two.md` is a plain file with none of those. A
/// `.git` and a `node_modules` subtree are also populated, each
/// containing a `.md` file that must never reach the parser (G2) —
/// every test using this fixture can assert on these exact counts.
pub fn vault_fixture() -> TempSource {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = TempSource { dir };

    src.write(
        "one.md",
        "# One\n\nLinks to [[Two]] and has a #project tag.\n\nSome text. ^block1\n",
    );
    src.write("two.md", "# Two\n\nNo links here.\n");
    src.mkdir(".git");
    src.write(".git/HEAD", "ref: refs/heads/main\n");
    src.mkdir("node_modules/pkg");
    src.write("node_modules/pkg/index.md", "# should never be parsed\n");

    src
}

/// A repo-shaped fixture: `README.md` carries one wikilink
/// (`[[one]]`); a `.git` directory and a `target/` build directory
/// are also populated, each with a `.md` file that must never reach
/// the parser (G2). `Cargo.toml` and `src/main.rs` are real,
/// non-ignored source files a code repository would contain: each
/// becomes a `code` unit (G2), but only `README.md` is parsed, so a
/// code-repo source never ends up with a false wikilink parsed out of
/// `Cargo.toml`'s `[[bin]]` table or a false tag out of a `main.rs`
/// comment.
pub fn repo_fixture() -> TempSource {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = TempSource { dir };

    src.write("README.md", "# atlas-repo\n\nSee [[one]] for details.\n");
    src.write(
        "Cargo.toml",
        "[package]\nname = \"atlas-repo\"\n\n[[bin]]\nname = \"atlas-repo\"\n",
    );
    src.write("src/main.rs", "// #not_a_real_tag\nfn main() {}\n");
    src.mkdir(".git");
    src.write(".git/HEAD", "ref: refs/heads/main\n");
    src.mkdir("target/debug");
    src.write("target/debug/build.md", "# should never be parsed\n");

    src
}

/// The spec's named entry point, `fixtures::temp_source()`. Defaults
/// to the vault shape — `repo_fixture()` is this module's
/// repo-shaped counterpart, used explicitly by tests that need it.
pub fn temp_source() -> TempSource {
    vault_fixture()
}

/// An empty fixture with no pre-seeded content, for tests (like
/// `large_source.rs`) that build their own synthetic tree from
/// scratch rather than using the fixed vault/repo shapes above.
pub fn empty_source() -> TempSource {
    let dir = tempfile::tempdir().expect("tempdir");
    TempSource { dir }
}

/// Alias used by `large_source.rs`; `TempSource` already is the
/// handle type returned by every `*_fixture`/`*_source` function in
/// this module.
pub type TempSourceHandle = TempSource;

/// Every file in [`kinds_fixture`] with the kind its unit must have
/// (G2). The ignored files are listed in [`KINDS_IGNORED`].
pub const KINDS_EXPECTED: [(&str, &str); 17] = [
    ("notes/one.md", "prose"),
    ("notes/two.txt", "prose"),
    ("notes/café notes.txt", "prose"),
    ("src/main.rs", "code"),
    ("Cargo.toml", "code"),
    ("Cargo.lock", "text-other"),
    ("docs/a b.PDF", "document"),
    ("img/logo.png", "binary"),
    ("scripts/bad.py", "binary"),
    ("bin/run", "code"),
    ("bin/plain", "text-other"),
    ("data/blob.xyz", "binary"),
    ("empty", "text-other"),
    ("empty.png", "text-other"),
    ("empty.md", "prose"),
    ("empty.pdf", "document"),
    ("Notes.MD", "prose"),
];

/// Paths [`kinds_fixture`] writes that must never become units.
pub const KINDS_IGNORED: [&str; 6] = [
    ".venv/lib/site.py",
    ".venv/pyvenv.cfg",
    "src/__pycache__/main.cpython-312.pyc",
    "pkg/deep/er/__pycache__/m.py",
    ".DS_Store",
    "notes/.DS_Store",
];

/// One file of every kind plus ignored files (G2/G15). Only prose
/// files (`notes/one.md`, `Notes.MD` and the two `.txt` files) are
/// parsed (G3); every other file carries text that *looks* like a
/// wikilink, tag and block ID, so any accidental parse shows up as a
/// stray row.
pub fn kinds_fixture() -> TempSource {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = TempSource { dir };
    let looks_like_markdown = "[[Target]] and #tag and a block ^blk123\n";

    src.write(
        "notes/one.md",
        "# One\n\nLinks to [[Two]] and has a #project tag.\n\nSome text. ^block1\n",
    );
    src.write("notes/two.txt", looks_like_markdown);
    src.write("notes/café notes.txt", looks_like_markdown);
    src.write(
        "src/main.rs",
        &format!("// {looks_like_markdown}fn main() {{}}\n"),
    );
    src.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\n\n[[bin]]\nname = \"x\"\n",
    );
    src.write("Cargo.lock", "# generated\nversion = 3\n");
    src.write_bytes("docs/a b.PDF", b"%PDF-1.4\n\0\x01\x02 binary container\0");
    src.write_bytes(
        "img/logo.png",
        &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D,
        ],
    );
    src.write_bytes("scripts/bad.py", b"print('hi')\n\0\0 [[Target]]\n");
    src.write(
        "bin/run",
        "#!/usr/bin/env python3\nprint('[[Target]] #tag')\n",
    );
    src.write("bin/plain", looks_like_markdown);
    src.write_bytes("data/blob.xyz", &[0xFF, 0xFE, 0x00, 0x01, 0x02]);
    src.write_bytes("empty", b"");
    src.write_bytes("empty.png", b"");
    src.write_bytes("empty.md", b"");
    src.write_bytes("empty.pdf", b"");
    src.write("Notes.MD", "# Upper-case extension\n\n[[Two]]\n");

    for ignored in KINDS_IGNORED {
        src.write(ignored, looks_like_markdown);
    }
    src
}

/// G3: one prose file of every format (extensions in mixed case, a
/// non-ASCII path, a UTF-16 file with a BOM), a duplicated `^id`, a
/// native literal region and native link syntax, plus files that must
/// give no rows (`.py`, `.rs`, `.pdf`, `.png`, `.json`, and a `.txt`
/// and `.org` with a NUL byte). See [`PROSE_EXPECTED`].
pub fn prose_fixture() -> TempSource {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = TempSource { dir };

    src.write(
        "notes/Plan.md",
        "# Plan\n\nSee [[Target]] #plan-tag\n\nPara with id ^plan-1\n",
    );
    src.write("notes/a.markdown", "## Sub\n\n[x](t.md)\n");
    src.write(
        "NOTES.TXT",
        "[[Target]] in plain #nope\n\nsecond ^dup\n\nthird ^dup\n",
    );
    src.write(
        "Plan.Org",
        "* Top\n** Next\ntext [[Target|alias]] [[file:x.org][X]] #nope\n#+BEGIN_SRC\n[[hidden]] ^hid\n#+END_SRC\n",
    );
    src.write("café notes.org", "* Café ☕\nlink [[Café]]\n");
    src.write(
        "doc.rst",
        "Title\n=====\n\nSub\n---\n\nsee [[Target]] and `S <https://x.org>`_\n",
    );
    src.write(
        "spec.adoc",
        "= Spec\n\n== Part\n\nsee [[@other:Target#^b1]] and link:x.adoc[X]\n",
    );
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "see [[U16]]\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    src.write_bytes("utf16-bom.txt", &utf16);

    // E2: a NUL in the first 8 KB makes these `binary`: no rows.
    src.write_bytes("nul.txt", b"[[Nope]] ^x\0\n");
    src.write_bytes("nul.org", b"* Head\0 [[Nope]]\n");
    // F5/F6: code, documents and binaries give no rows.
    src.write("main.py", "# [[Nope]] #nope ^x\nx = 1\n");
    src.write("lib.rs", "// [[Nope]]\n// = Not a heading\nfn f() {}\n");
    src.write_bytes("doc.pdf", b"%PDF-1.4\n\0[[Nope]]\0");
    src.write_bytes(
        "img.png",
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
    );
    src.write("data.json", "{\"a\": \"[[Nope]]\", \"b\": [1, 2]}\n");
    src
}

/// `(rel_path, headings, links, tags, blocks)` expected for each file
/// of [`prose_fixture`] that gives any rows. Headings are
/// `(level, text, line)`, links `(kind, raw)`, blocks
/// `(block_id, line, end_line)`.
pub type ProseExpect = (
    &'static str,
    &'static [(i64, &'static str, i64)],
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
    &'static [(Option<&'static str>, i64, i64)],
);

pub const PROSE_EXPECTED: [ProseExpect; 8] = [
    (
        "notes/Plan.md",
        &[(1, "Plan", 1)],
        &[("wikilink", "Target")],
        &["plan-tag"],
        &[(None, 3, 3), (Some("plan-1"), 5, 5)],
    ),
    (
        "notes/a.markdown",
        &[(2, "Sub", 1)],
        &[("markdown", "t.md")],
        &[],
        &[(None, 3, 3)],
    ),
    (
        "NOTES.TXT",
        &[],
        &[("wikilink", "Target")],
        &[],
        &[(None, 1, 1), (Some("dup"), 3, 3), (Some("dup"), 5, 5)],
    ),
    (
        "Plan.Org",
        &[(1, "Top", 1), (2, "Next", 2)],
        &[("wikilink", "Target|alias")],
        &[],
        &[(None, 3, 3)],
    ),
    (
        "café notes.org",
        &[(1, "Café ☕", 1)],
        &[("wikilink", "Café")],
        &[],
        &[(None, 2, 2)],
    ),
    (
        "doc.rst",
        &[(1, "Title", 1), (2, "Sub", 4)],
        &[("wikilink", "Target")],
        &[],
        &[(None, 7, 7)],
    ),
    (
        "spec.adoc",
        &[(1, "Spec", 1), (2, "Part", 3)],
        &[("wikilink", "@other:Target#^b1")],
        &[],
        &[(None, 5, 5)],
    ),
    (
        "utf16-bom.txt",
        &[],
        &[("wikilink", "U16")],
        &[],
        &[(None, 1, 1)],
    ),
];

/// Files of [`prose_fixture`] that are units but give no rows at all.
pub const PROSE_NO_ROWS: [&str; 7] = [
    "nul.txt",
    "nul.org",
    "main.py",
    "lib.rs",
    "doc.pdf",
    "img.png",
    "data.json",
];

/// Every file under `root` as `rel_path -> (bytes, mtime)`: the F4
/// snapshot ("indexing prose never writes").
pub fn tree_snapshot(
    root: &Path,
) -> std::collections::BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let mtime = fs::metadata(&path).unwrap().modified().unwrap();
                out.insert(rel, (fs::read(&path).unwrap(), mtime));
            }
        }
    }
    out
}

/// The rows the cache holds for one unit, in the shapes
/// [`ProseExpect`] uses. `None` if there is no such unit.
#[derive(Debug, PartialEq)]
pub struct UnitRows {
    pub kind: String,
    pub headings: Vec<(i64, String, i64)>,
    pub links: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub blocks: Vec<(Option<String>, i64, i64)>,
}

pub fn unit_rows(cache_path: &Path, source: &str, rel_path: &str) -> Option<UnitRows> {
    let cache = tephra_cache::Cache::open(cache_path).expect("reopen cache");
    let source_id = cache.get_source(source).unwrap()?.id;
    let unit = cache.get_unit_by_rel_path(source_id, rel_path).unwrap()?;
    let conn = cache.connection();
    let links = conn
        .prepare("SELECT kind, to_raw FROM links WHERE from_unit_id = ?1 ORDER BY id")
        .unwrap()
        .query_map([unit.id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let tags = conn
        .prepare("SELECT tag FROM tags WHERE unit_id = ?1 ORDER BY id")
        .unwrap()
        .query_map([unit.id], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    Some(UnitRows {
        kind: unit.kind.clone(),
        headings: cache
            .headings_for_unit(unit.id)
            .unwrap()
            .into_iter()
            .map(|h| (h.level, h.text, h.line))
            .collect(),
        links,
        tags,
        blocks: cache
            .blocks_for_unit(unit.id)
            .unwrap()
            .into_iter()
            .map(|b| (b.block_id, b.line, b.end_line.unwrap()))
            .collect(),
    })
}

pub fn expected_rows(e: &ProseExpect) -> UnitRows {
    UnitRows {
        kind: "prose".to_string(),
        headings: e
            .1
            .iter()
            .map(|(l, t, n)| (*l, t.to_string(), *n))
            .collect(),
        links: e
            .2
            .iter()
            .map(|(k, r)| (k.to_string(), r.to_string()))
            .collect(),
        tags: e.3.iter().map(|t| t.to_string()).collect(),
        blocks: e
            .4
            .iter()
            .map(|(id, l, end)| (id.map(str::to_string), *l, *end))
            .collect(),
    }
}
