//! Aggregates `tests/integration/*.rs` into one test binary, the
//! same convention `tephra-graph`'s `tests/integration.rs` uses.

#[path = "rpc_fixtures/mod.rs"]
mod rpc_fixtures;

#[path = "integration/rpc_concurrency.rs"]
mod rpc_concurrency;
#[path = "integration/rpc_server_http.rs"]
mod rpc_server_http;
#[path = "integration/rpc_server_ws.rs"]
mod rpc_server_ws;
#[path = "integration/rpc_transport_parity.rs"]
mod rpc_transport_parity;
