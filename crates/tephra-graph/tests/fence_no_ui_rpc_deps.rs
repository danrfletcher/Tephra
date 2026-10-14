//! F3 fence regression, enforced programmatically (not just
//! eyeballed once): `tephra-graph`'s full dependency tree must never
//! pull in a UI/windowing/webview crate, and must never depend on an
//! RPC-transport crate — this crate stays a pure library sitting
//! between `tephra-cache`/`tephra-parser` and later PRs' RPC/CLI
//! surfaces, never the reverse. Shells out to `cargo tree` against
//! the already-resolved `Cargo.lock` (no network needed) and checks
//! every dependency name against a blocklist, the same convention
//! `tephra-parser`'s own `tests/fence_no_ui_deps.rs` uses.

use std::process::Command;

const BANNED_SUBSTRINGS: &[&str] = &[
    // UI / windowing / webview.
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
    // RPC transport.
    "tonic",
    "grpc",
    "jsonrpc",
    "jsonrpsee",
    "tower-http",
    "axum",
    "hyper",
    "warp",
    "actix-web",
    "tungstenite",
    "tokio-tungstenite",
];

/// This crate must never depend on the daemon crate it feeds, either
/// — `tephrad` sits one layer below in this workspace's dependency
/// order (it already depends on `tephra-cache`/`tephra-parser`;
/// `tephra-graph` must not depend back on it), so a dependency edge
/// in that direction would be a real layering violation `cargo tree`
/// alone wouldn't catch via the substring blocklist above.
const BANNED_EXACT_PACKAGE_NAMES: &[&str] = &["tephrad"];

#[test]
fn dependency_tree_contains_no_ui_webview_or_rpc_transport_crates() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-p", "tephra-graph", "--prefix", "none"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    let banned_substring_offenders: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            BANNED_SUBSTRINGS
                .iter()
                .any(|needle| lower.contains(needle))
        })
        .collect();
    assert!(
        banned_substring_offenders.is_empty(),
        "tephra-graph's dependency tree must stay headless and transport-free (F3) but pulled in: {banned_substring_offenders:?}\nfull tree:\n{stdout}"
    );

    let exact_name_offenders: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            let package_name = line.split_whitespace().next().unwrap_or("");
            BANNED_EXACT_PACKAGE_NAMES.contains(&package_name)
        })
        .collect();
    assert!(
        exact_name_offenders.is_empty(),
        "tephra-graph must not depend on the daemon crate (F3 layering) but pulled in: {exact_name_offenders:?}\nfull tree:\n{stdout}"
    );
}
