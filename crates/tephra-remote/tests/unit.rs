//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! includes, the convention `tephrad`'s `tests/unit.rs` uses.

#[path = "common/mod.rs"]
mod common;

#[path = "unit/auth_errors.rs"]
mod auth_errors;
#[path = "unit/base_url.rs"]
mod base_url;
#[path = "unit/rate_limit.rs"]
mod rate_limit;
