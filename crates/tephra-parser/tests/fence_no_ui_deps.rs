//! F3 fence regression, enforced programmatically (not just eyeballed
//! once): `tephra-parser`'s full dependency tree must never pull in a
//! UI/windowing/webview crate. Shells out to `cargo tree` against the
//! already-resolved `Cargo.lock` (no network needed) and checks every
//! dependency name against a blocklist of well-known UI/windowing/
//! webview crate name fragments.

use std::process::Command;

const BANNED_SUBSTRINGS: &[&str] = &[
    "gtk", "qt-", "qt_", "winit", "wgpu", "vulkan", "metal-rs", "tauri", "egui", "iced", "slint",
    "webview", "electron", "cocoa", "x11", "wayland", "skia", "glutin", "sdl2", "druid",
];

#[test]
fn dependency_tree_contains_no_ui_windowing_or_webview_crates() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-p", "tephra-parser", "--prefix", "none"])
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
            BANNED_SUBSTRINGS
                .iter()
                .any(|needle| lower.contains(needle))
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "tephra-parser's dependency tree must stay headless (F3) but pulled in: {offenders:?}\nfull tree:\n{stdout}"
    );
}
