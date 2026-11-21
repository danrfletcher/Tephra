//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! `mod` includes, the same convention `tephra-parser`'s
//! `tests/unit.rs` uses.

#[path = "fixtures/mod.rs"]
mod fixtures;

#[path = "unit/block_links.rs"]
mod block_links;
#[path = "../../tephra-parser/tests/unit/cpu_timer.rs"]
mod cpu_timer;
#[path = "unit/large_fan_in.rs"]
mod large_fan_in;
#[path = "unit/markdown_links.rs"]
mod markdown_links;
#[path = "unit/model.rs"]
mod model;
#[path = "unit/multibyte_names.rs"]
mod multibyte_names;
#[path = "unit/query.rs"]
mod query;
#[path = "unit/resolver.rs"]
mod resolver;
