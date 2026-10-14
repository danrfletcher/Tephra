//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! includes, the same convention `tephra-graph`'s `tests/unit.rs`
//! uses.

#[path = "unit/rpc_auth.rs"]
mod rpc_auth;
#[path = "unit/rpc_envelope.rs"]
mod rpc_envelope;
