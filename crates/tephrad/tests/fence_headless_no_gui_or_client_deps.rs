//! F3/F4 fence regressions, enforced programmatically rather than
//! only eyeballed once:
//!
//! - F3: no GUI, webview, tray-icon, or browser-launching dependency
//!   is present in `tephrad`'s resolved dependency tree. (`axum`,
//!   `tower`, `tokio-tungstenite`, `hyper`, etc. are *not* banned
//!   here — unlike `tephra-graph`'s own fence test, `tephrad` is
//!   exactly the crate meant to carry the RPC/WebSocket transport;
//!   what's banned is anything that would open a window, webview, or
//!   browser.)
//! - F4: no VSCode extension manifest, Chrome extension manifest,
//!   `chrome.*`/`browser.*` API usage, or `vscode` module import
//!   exists anywhere in this crate's source tree.
//!
//! Shells out to `cargo tree` against the already-resolved
//! `Cargo.lock` (no network needed), the same convention
//! `tephra-graph`'s `tests/fence_no_ui_rpc_deps.rs` and
//! `tephra-parser`'s `tests/fence_no_ui_deps.rs` use.

use std::process::Command;

const BANNED_DEPENDENCY_SUBSTRINGS: &[&str] = &[
    // UI / windowing / webview / tray icon.
    "gtk",
    "qt-",
    "qt_",
    "winit",
    "wgpu",
    "vulkan",
    "metal-rs",
    "tauri",
    "egui",
    "iced",
    "slint",
    "webview",
    "electron",
    "cocoa",
    "x11",
    "wayland",
    "skia",
    "glutin",
    "sdl2",
    "druid",
    "tray-item",
    "tray-icon",
    // Browser-launching.
    "webbrowser",
    "open-rs",
];

#[test]
fn dependency_tree_contains_no_gui_webview_or_browser_launching_crates() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-p", "tephrad", "--prefix", "none"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let offenders: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            BANNED_DEPENDENCY_SUBSTRINGS.iter().any(|needle| lower.contains(needle))
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "tephrad must stay headless (F3): no GUI/webview/browser-launching dependency is allowed, but found: {offenders:?}\nfull tree:\n{stdout}"
    );
}

/// F3's `Cargo.toml`-only half, independent of `cargo tree`'s
/// transitive view: the crate's own declared dependencies must not
/// *directly* name any of the banned categories either.
#[test]
fn cargo_toml_declares_no_gui_webview_or_browser_launching_dependency() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    let lower = manifest.to_ascii_lowercase();
    let offenders: Vec<&str> = BANNED_DEPENDENCY_SUBSTRINGS
        .iter()
        .filter(|needle| lower.contains(*needle))
        .copied()
        .collect();
    assert!(offenders.is_empty(), "Cargo.toml names a banned dependency: {offenders:?}");
}

const BANNED_SOURCE_PATTERNS: &[&str] = &[
    "vscode",
    "chrome.runtime",
    "chrome.tabs",
    "chrome.extension",
    "browser.runtime",
    "browser.tabs",
    "browser.extension",
    "chrome-extension://",
];

/// F4: no VSCode/Chrome extension client code anywhere in this
/// crate's source tree — this PR's scope ends at the wire protocol
/// and dispatch table; client consumption is explicitly deferred.
#[test]
fn source_tree_contains_no_vscode_or_chrome_extension_code() {
    let src_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut offenders = Vec::new();
    walk_rs_files(src_dir, &mut |path, contents| {
        let lower = contents.to_ascii_lowercase();
        for pattern in BANNED_SOURCE_PATTERNS {
            if lower.contains(pattern) {
                offenders.push(format!("{}: {pattern}", path.display()));
            }
        }
    });
    assert!(offenders.is_empty(), "source tree contains banned VSCode/Chrome client references: {offenders:?}");
}

/// F4: no VSCode/Chrome extension manifest file (`manifest.json` with
/// extension-shaped fields, or a `.vsixmanifest`) exists anywhere
/// under this crate.
#[test]
fn no_extension_manifest_files_exist_in_this_crate() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    walk_all_files(crate_dir, &mut |path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "manifest.json" || name.ends_with(".vsixmanifest") {
            offenders.push(path.to_path_buf());
        }
    });
    assert!(offenders.is_empty(), "found an extension-manifest-shaped file: {offenders:?}");
}

fn walk_rs_files(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path, &str)) {
    walk_all_files(dir, &mut |path| {
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(contents) = std::fs::read_to_string(path) {
                visit(path, &contents);
            }
        }
    });
}

fn walk_all_files(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_all_files(&path, visit);
        } else {
            visit(&path);
        }
    }
}
