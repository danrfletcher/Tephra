//! `tephrad`'s library surface: the pieces of the daemon this PR
//! implements as a callable Rust API rather than only reachable
//! through the binary — the source registry/watcher (PR-4), the
//! scan pipeline it depends on, and the JSON-RPC transport ([`rpc`],
//! PR-6: a single HTTP/WebSocket listener dispatching to PR-5's
//! resolver/graph verbs and PR-4's registry verbs behind a Bearer
//! auth gate). Config, paths, token, and the listener-bind stub stay
//! binary-only (`src/main.rs`'s own module tree), since they're
//! startup-sequence concerns with no API surface a later crate or
//! test needs to call directly.
pub mod ignore;
pub mod registry;
pub mod rpc;
pub mod scan;
pub mod watcher;
mod writer_gate;
