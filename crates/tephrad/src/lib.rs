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
pub mod access;
pub mod block_id;
pub mod cache_limit;
pub mod catch_up;
pub mod dup_ids;
pub mod file_kind;
pub mod file_lock;
pub mod fix;
pub mod fixes;
pub mod forward;
pub mod ignore;
pub mod refactor;
pub mod registry;
pub mod remote;
pub mod remote_drive;
pub mod remote_github;
pub mod rename_detect;
pub mod rpc;
pub mod scan;
pub mod search;
pub mod unit_list;
pub mod watcher;
mod writer_gate;

// The credential-store trait and the API-URL rule live in `tephra-remote`
// (the adapters need them and that crate must not depend on this one, which
// pulls in `tephra-cache`). Re-exported so the paths stay
// `tephrad::credentials` and `tephrad::api_url`.
pub use tephra_remote::{api_url, credentials};
