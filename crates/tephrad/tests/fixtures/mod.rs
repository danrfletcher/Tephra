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
/// non-ignored source files a code repository would contain — they
/// are walked and counted in `files_seen` like any other non-ignored
/// file, but (R3) must never be fed through the parser: only
/// `README.md`'s `.md` extension qualifies it to become a unit, so a
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
