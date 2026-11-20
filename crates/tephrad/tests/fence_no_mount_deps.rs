//! F13 fence regression, enforced programmatically: Tephra does not use
//! rclone, FUSE or any mount unless spike S2 is a go and Dan says yes.
//!
//! Two checks, the same shape as `crates/tephra-graph/tests/
//! fence_no_ui_rpc_deps.rs`:
//!
//! 1. `cargo tree --workspace` (against the already-resolved `Cargo.lock`)
//!    lists no crate whose name contains `fuse`, `fuser`, `rclone` or `nfs`.
//! 2. No file under `crates/*/src` contains a `rclone` or `mount` command
//!    string (a quoted `"mount"`, `"umount"`, `"fusermount"`, `"mount_nfs"`,
//!    `"nfsmount"`, or any mention of `rclone`).
//!
//! The scanner is a plain function with its own table of positive and
//! negative cases, so a regression in the scan itself fails a test too.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Substrings of a crate name that are never allowed in the workspace tree.
const BANNED_CRATE_NAME_SUBSTRINGS: &[&str] = &["fuse", "fuser", "rclone", "nfs"];

/// Source-text patterns that mean "runs rclone or a mount command".
/// `rclone` is matched case-insensitively anywhere; the others are
/// quoted command names, as they would appear in `Command::new("...")`.
const BANNED_SOURCE_PATTERNS: &[&str] = &[
    "rclone",
    "\"mount\"",
    "\"umount\"",
    "\"fusermount\"",
    "\"fusermount3\"",
    "\"mount_nfs\"",
    "\"mount_macfuse\"",
    "\"nfsmount\"",
    "\"cmount\"",
    "mount -t",
];

fn offending_patterns(text: &str) -> Vec<&'static str> {
    let lower = text.to_ascii_lowercase();
    BANNED_SOURCE_PATTERNS
        .iter()
        .copied()
        .filter(|p| lower.contains(*p))
        .collect()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn dependency_tree_has_no_fuse_rclone_or_nfs_crates() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--prefix", "none"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().count() > 10,
        "cargo tree printed almost nothing, so the check proves nothing:\n{stdout}"
    );

    let offenders: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            let name = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            BANNED_CRATE_NAME_SUBSTRINGS
                .iter()
                .any(|needle| name.contains(needle))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "F13: no rclone, FUSE or mount crates, but the workspace tree has: {offenders:?}"
    );
}

#[test]
fn no_crate_source_has_an_rclone_or_mount_command_string() {
    let root = workspace_root();
    let mut files = Vec::new();
    for entry in fs::read_dir(root.join("crates")).expect("read crates/") {
        let src = entry.expect("crate dir").path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(
        files.len() > 20,
        "expected to scan the daemon's sources, found only {} files",
        files.len()
    );

    let mut offenders = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read source file");
        for pattern in offending_patterns(&text) {
            offenders.push(format!("{}: contains {pattern:?}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "F13: no rclone or mount command string under crates/*/src:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_source_scan_flags_what_it_should_and_nothing_else() {
    // (text, patterns that must be reported)
    let cases: &[(&str, &[&str])] = &[
        (r#"Command::new("mount").arg("/mnt")"#, &["\"mount\""]),
        (r#"Command::new("umount")"#, &["\"umount\""]),
        (r#"Command::new("fusermount3")"#, &["\"fusermount3\""]),
        (r#"std::process::Command::new("rclone")"#, &["rclone"]),
        ("// uses RClone for caching", &["rclone"]),
        (r#"run("mount -t nfs host:/x /y")"#, &["mount -t"]),
        // ordinary words that merely contain "mount" are fine
        ("let amount = 3; // mounted state", &[]),
        ("fn account_mountain() {}", &[]),
        ("", &[]),
    ];
    for (text, expected) in cases {
        let got = offending_patterns(text);
        for want in *expected {
            assert!(
                got.contains(want),
                "{text:?} should report {want:?}, got {got:?}"
            );
        }
        if expected.is_empty() {
            assert!(got.is_empty(), "{text:?} should be clean, got {got:?}");
        }
    }
}
