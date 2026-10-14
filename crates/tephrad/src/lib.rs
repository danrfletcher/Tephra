//! `tephrad`'s library surface: the pieces of the daemon this PR
//! implements as a callable Rust API rather than JSON-RPC (PR-6) or a
//! CLI (PR-7) — the source registry/watcher (PR-4). Config, paths,
//! token, and the listener-bind stub stay binary-only (`src/main.rs`'s
//! own module tree), since they're startup-sequence concerns with no
//! API surface a later crate or test needs to call directly.

pub mod ignore;
pub mod registry;
pub mod scan;
pub mod watcher;
