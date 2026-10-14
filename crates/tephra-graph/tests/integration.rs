//! Aggregates `tests/integration/*.rs` into one test binary.

#[path = "fixtures/mod.rs"]
mod fixtures;

#[path = "integration/federation.rs"]
mod federation;
#[path = "integration/offline_source.rs"]
mod offline_source;
#[path = "integration/watcher_update.rs"]
mod watcher_update;
