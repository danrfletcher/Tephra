//! Aggregates `tests/integration/*.rs` into one test binary via `#[path]`
//! includes. Every test talks to a fake on 127.0.0.1.

#[path = "common/mod.rs"]
mod common;

#[path = "integration/drive_fake.rs"]
mod drive_fake;
#[path = "integration/drive_upload.rs"]
mod drive_upload;
#[path = "integration/fake_servers.rs"]
mod fake_servers;
#[path = "integration/github_fake.rs"]
mod github_fake;
