//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! `mod` includes, so the files can live at the exact paths this
//! crate's spec names (`tests/unit/frontmatter.rs`, etc.) while still
//! compiling as ordinary Rust test modules.

#[path = "unit/blocks.rs"]
mod blocks;
#[path = "unit/determinism.rs"]
mod determinism;
#[path = "unit/frontmatter.rs"]
mod frontmatter;
#[path = "unit/headings.rs"]
mod headings;
#[path = "unit/markdown_links.rs"]
mod markdown_links;
#[path = "unit/robustness.rs"]
mod robustness;
#[path = "unit/tags.rs"]
mod tags;
#[path = "unit/wikilinks.rs"]
mod wikilinks;
