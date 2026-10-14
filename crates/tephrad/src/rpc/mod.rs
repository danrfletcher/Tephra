//! PR-6: the JSON-RPC RPC/WebSocket server. See each submodule's own
//! doc comment for its slice of the design:
//!
//! - [`protocol`]: envelope validation, error codes, response shapes.
//! - [`auth`]: Bearer-token authentication (constant-time compare).
//! - [`backend`]: the [`backend::GraphBackend`]/[`backend::SourceBackend`]
//!   seams dispatch calls through, so tests never need a real cache.
//! - [`dispatch`]: the closed method table and its handlers.
//! - [`server`]: the axum `Router`, HTTP handler, and WebSocket
//!   handler (request/response plus `graph:changed` pushes) built on
//!   top of all of the above.

pub mod auth;
pub mod backend;
pub mod dispatch;
pub mod protocol;
pub mod server;
