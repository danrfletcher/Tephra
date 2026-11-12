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

/// One file of every kind plus ignored files (G2/G15). `notes/one.md`
/// is the only file with links/tags/blocks to extract; every other
/// file carries text that *looks* like a wikilink, tag and block ID,
/// so any accidental parse shows up as a stray row.
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
