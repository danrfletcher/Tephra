//! `tephractl`: the command-line client for the `tephrad` daemon
//! (PR-7). Talks to `tephrad`'s JSON-RPC server (PR-6) over plain
//! HTTP with bearer-token auth (P2) — it implements no RPC methods,
//! no graph/registry/parsing logic, and mutates nothing on disk
//! itself; every verb below is a thin client over an RPC call PR-4/
//! PR-5/PR-6 already built.
//!
//! Module map:
//! - [`paths`]: resolves the default token file location.
//! - [`token`]: loads the bearer token from disk.
//! - [`cli`]: pure argv parsing into a [`cli::Command`] (no network).
//! - [`transport`]: the JSON-RPC 2.0 client ([`transport::RpcClient`]).
//! - [`render`]: formats RPC results/errors as human-readable text.
//! - [`commands`]: wires a parsed command to the right RPC call and
//!   renderer.

pub mod cli;
pub mod commands;
pub mod paths;
pub mod render;
pub mod token;
pub mod transport;
