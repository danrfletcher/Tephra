//! Aggregator test binary for `tephractl`'s CLI test suite (G8),
//! mirroring `tephrad`'s own `tests/integration.rs` `#[path]`
//! aggregation pattern so each concern lives in its own file under
//! `tests/cli/` while still compiling as one test binary.

#[path = "cli/support/mock_server.rs"]
mod support;

#[path = "cli/support/auth_support.rs"]
mod auth_support;

#[path = "cli/args.rs"]
mod args;

#[path = "cli/auth.rs"]
mod auth;

#[path = "cli/source_commands.rs"]
mod source_commands;

#[path = "cli/ls_commands.rs"]
mod ls_commands;

#[path = "cli/search_commands.rs"]
mod search_commands;

#[path = "cli/block_id_commands.rs"]
mod block_id_commands;

#[path = "cli/link_commands.rs"]
mod link_commands;

#[path = "cli/backlinks.rs"]
mod backlinks;

#[path = "cli/fixes_commands.rs"]
mod fixes_commands;

#[path = "cli/transport.rs"]
mod transport;

#[path = "cli/search_in_file.rs"]
mod search_in_file;

#[path = "cli/remote_auth.rs"]
mod remote_auth;

#[path = "cli/google_loopback.rs"]
mod google_loopback;

#[path = "cli/auth_secrets.rs"]
mod auth_secrets;
