//! Aggregates `tests/integration/*.rs` the same way `tests/unit.rs`
//! does.

#[path = "integration/parse_unit_fixtures.rs"]
mod parse_unit_fixtures;

#[path = "integration/code_fixtures.rs"]
mod code_fixtures;

#[path = "integration/documents_fixtures.rs"]
mod documents_fixtures;
