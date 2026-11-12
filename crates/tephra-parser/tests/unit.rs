//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! `mod` includes, so the files can live at the exact paths this
//! crate's spec names (`tests/unit/frontmatter.rs`, etc.) while still
//! compiling as ordinary Rust test modules.

#[path = "unit/blocks.rs"]
mod blocks;
#[path = "unit/code_comments.rs"]
mod code_comments;
#[path = "unit/code_languages.rs"]
mod code_languages;
#[path = "unit/code_robustness.rs"]
mod code_robustness;
#[path = "unit/determinism.rs"]
mod determinism;
#[path = "unit/frontmatter.rs"]
mod frontmatter;
#[path = "unit/headings.rs"]
mod headings;
#[path = "unit/large_file.rs"]
mod large_file;
#[path = "unit/markdown_links.rs"]
mod markdown_links;
#[path = "unit/prose_formats.rs"]
mod prose_formats;
#[path = "unit/prose_headings.rs"]
mod prose_headings;
#[path = "unit/robustness.rs"]
mod robustness;
#[path = "unit/tags.rs"]
mod tags;
#[path = "unit/wikilinks.rs"]
mod wikilinks;
