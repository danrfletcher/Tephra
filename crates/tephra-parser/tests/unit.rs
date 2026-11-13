//! Aggregates `tests/unit/*.rs` into one test binary via `#[path]`
//! `mod` includes, so the files can live at the exact paths this
//! crate's spec names (`tests/unit/frontmatter.rs`, etc.) while still
//! compiling as ordinary Rust test modules.

#[path = "unit/blocks.rs"]
mod blocks;
#[path = "unit/code_block_ids.rs"]
mod code_block_ids;
#[path = "unit/code_comments.rs"]
mod code_comments;
#[path = "unit/code_languages.rs"]
mod code_languages;
#[path = "unit/code_robustness.rs"]
mod code_robustness;
#[path = "unit/determinism.rs"]
mod determinism;
#[path = "unit/documents_docx.rs"]
mod documents_docx;
#[path = "unit/documents_links.rs"]
mod documents_links;
#[path = "unit/documents_pdf.rs"]
mod documents_pdf;
#[path = "unit/documents_pptx.rs"]
mod documents_pptx;
#[path = "unit/documents_support.rs"]
mod documents_support;
#[path = "unit/frontmatter.rs"]
mod frontmatter;
#[path = "unit/headings.rs"]
mod headings;
#[path = "unit/index_rows.rs"]
mod index_rows;
#[path = "unit/large_file.rs"]
mod large_file;
#[path = "unit/line_end.rs"]
mod line_end;
#[path = "unit/line_regions.rs"]
mod line_regions;
#[path = "unit/markdown_links.rs"]
mod markdown_links;
#[path = "unit/prose_formats.rs"]
mod prose_formats;
#[path = "unit/prose_headings.rs"]
mod prose_headings;
#[path = "unit/robustness.rs"]
mod robustness;
#[path = "unit/symbol_ranges.rs"]
mod symbol_ranges;
#[path = "unit/symbols.rs"]
mod symbols;
#[path = "unit/tags.rs"]
mod tags;
#[path = "unit/wikilinks.rs"]
mod wikilinks;
