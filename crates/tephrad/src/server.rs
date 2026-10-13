//! Listener-bind stub.
//!
//! This PR implements only the TCP bind proving deterministic startup
//! order. JSON-RPC method dispatch and HTTP/WebSocket framing are PR-6's
//! responsibility and are explicitly out of scope here.

use std::io;

use tokio::net::TcpListener;

/// Binds a `tokio::net::TcpListener` on `addr`. Deliberately `std::net` /
/// `tokio::net::Tcp*` only — tephra has no Unix-socket transport.
pub async fn bind(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}
