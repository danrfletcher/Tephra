//! Tiny CLI harness for container verification: reads a code file,
//! `parse_code_unit`s it, and prints its symbols and block IDs as
//! pretty JSON (G4 symbols, G13 ownership).
//!
//! Usage:
//!   cargo run -p tephra-parser --example dump_symbols -- <path-to-code-file>
//!
//! The language comes from the file name (or its shebang), the same
//! table the scanner uses. Output is deterministic: re-running against
//! the same file produces byte-identical JSON. Lines are 1-based; a
//! symbol's range is `start_line..=end_line` and `line` is its
//! definition line.

use std::path::PathBuf;

use serde_json::json;
use tephra_parser::{parse_code_unit, Language};

fn main() {
    let mut args = std::env::args().skip(1);
    let path: PathBuf = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: dump_symbols <path-to-code-file>");
            std::process::exit(2);
        }
    };

    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    let Some(lang) = Language::from_path(&path, content.lines().next()) else {
        eprintln!("{}: no bundled grammar for this file", path.display());
        std::process::exit(1);
    };

    let parsed = parse_code_unit(&path, lang, &content);
    let out = json!({
        "path": path.display().to_string(),
        "language": lang.name(),
        "symbols": parsed.symbols,
        "blocks": parsed.blocks,
        "diagnostics": parsed.diagnostics,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&out).expect("symbols always serialize")
    );
}
