//! Listener bind.
//!
//! Earlier PRs implemented only the TCP bind proving deterministic
//! startup order. JSON-RPC method dispatch and HTTP/WebSocket framing
//! now live in the library crate's `rpc` module (`tephrad::rpc`),
//! served over the listener this module binds — see `main.rs`'s
//! `run_server` for how the two are wired together.

use std::io;

use tokio::net::TcpListener;

/// Binds a `tokio::net::TcpListener` on `addr`. Deliberately `std::net` /
/// `tokio::net::Tcp*` only — tephra has no Unix-socket transport.
pub async fn bind(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}
