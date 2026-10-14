//! Tiny CLI harness for container verification: reads a file,
//! `parse_unit`s it, and prints the resulting `ParsedUnit` as pretty
//! JSON — the GP3 extraction checkpoint rendered as evidence.
//!
//! Usage:
//!   cargo run -p tephra-parser --example dump_ast -- <path-to-md-file>
//!
//! Output is deterministic: re-running against the same file produces
//! byte-identical JSON (no timestamps, no non-deterministic ordering).

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let path: PathBuf = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: dump_ast <path-to-md-file>");
            std::process::exit(2);
        }
    };

    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("failed to read {}: {e}", path.display());
        std::process::exit(1);
    });

    let parsed = tephra_parser::parse_unit(&path, &content);
    println!(
        "{}",
        serde_json::to_string_pretty(&parsed).expect("ParsedUnit always serializes")
    );
}
