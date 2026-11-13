//! The candidates of an in-file search (`unit.search` with `scope:
//! "headings"` or `"blocks"`, G9): the headings, paragraphs and symbols
//! one unit's cached rows describe, in file order.
//!
//! The cache decides *what* a unit offers; the matching rule and the
//! ranking live in `tephrad::search`. A paragraph's text is not cached
//! (the scan stores only its line range), so a prose or document
//! paragraph comes back here as a position, and the caller fills in the
//! text. Headings and symbol names are cached in full.
//!
//! The rule for each kind of unit:
//!
//! | kind        | `Headings`                  | `Blocks`                            |
//! |-------------|-----------------------------|-------------------------------------|
//! | `prose`     | its headings                | its `blocks` rows (paragraphs)      |
//! | `code`      | its symbols (settled detail)| its symbols                         |
//! | `document`  | its headings                | its paragraphs, by page/slide/index |
//! | `text-other`, `binary` | nothing          | nothing                             |
//!
//! A `too_large` unit has no cached rows and offers nothing whatever
//! its kind. Everything here is a read: it is safe on a
//! [`crate::Cache::open_read_only`] connection and never writes.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::units::{self, UnitRow};
use crate::CacheError;

/// Which in-file search is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFileScope {
    /// `scope: "headings"`, the `#` search.
    Headings,
    /// `scope: "blocks"`, the `^` search.
    Blocks,
}

impl InFileScope {
    /// The wire name: `headings` or `blocks`.
    pub fn as_str(self) -> &'static str {
        match self {
            InFileScope::Headings => "headings",
            InFileScope::Blocks => "blocks",
        }
    }
}

/// One thing a unit offers to an in-file search. Lines are 1-based; the
/// order of a returned list is file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InFileCandidate {
    /// A heading of a prose unit, on `line`.
    Heading { level: i64, text: String, line: i64 },
    /// A heading of a document: `number` is its page, slide or
    /// paragraph number (the unit's extension says which).
    DocumentHeading {
        level: i64,
        text: String,
        number: i64,
    },
    /// A prose block: a paragraph or a list item (or a bare `^id` on
    /// one line) spanning `line..=end_line`. `block_id` is set when the
    /// block already carries a `^id`. The text is not cached.
    Paragraph {
        line: i64,
        end_line: i64,
        block_id: Option<String>,
    },
    /// A symbol of a code unit, on its definition line. `block_id` is
    /// set when an ID comment already belongs to it (G13).
    Symbol {
        symbol_kind: String,
        name: String,
        parent: Option<String>,
        line: i64,
        block_id: Option<String>,
    },
    /// A paragraph of a document: `number` is its page, slide or
    /// paragraph number and `index` its 1-based position within it
    /// (the same number as `number` for a docx). The text is not
    /// cached, and a document paragraph never has a block ID.
    DocumentParagraph { number: i64, index: i64 },
}

pub(crate) fn in_file_candidates(
    conn: &Connection,
    unit: &UnitRow,
    scope: InFileScope,
) -> Result<Vec<InFileCandidate>, CacheError> {
    if unit.too_large {
        return Ok(Vec::new());
    }
    match (unit.kind.as_str(), scope) {
        ("prose", InFileScope::Headings) => Ok(units::headings_for_unit(conn, unit.id)?
            .into_iter()
            .map(|h| InFileCandidate::Heading {
                level: h.level,
                text: h.text,
                line: h.line,
            })
            .collect()),
        ("prose", InFileScope::Blocks) => {
            let mut rows = units::blocks_for_unit(conn, unit.id)?;
            // Stable: rows on one line keep the order they were stored in.
            rows.sort_by_key(|b| b.line);
            Ok(rows
                .into_iter()
                .map(|b| InFileCandidate::Paragraph {
                    line: b.line,
                    end_line: b.end_line.unwrap_or(b.line),
                    block_id: b.block_id,
                })
                .collect())
        }
        ("code", _) => symbols(conn, unit),
        ("document", InFileScope::Headings) => Ok(units::headings_for_unit(conn, unit.id)?
            .into_iter()
            .map(|h| InFileCandidate::DocumentHeading {
                level: h.level,
                text: h.text,
                number: h.line,
            })
            .collect()),
        ("document", InFileScope::Blocks) => {
            let mut rows = units::blocks_for_unit(conn, unit.id)?;
            rows.sort_by_key(|b| (b.line, b.end_line));
            Ok(rows
                .into_iter()
                .map(|b| InFileCandidate::DocumentParagraph {
                    number: b.line,
                    index: b.end_line.unwrap_or(1),
                })
                .collect())
        }
        // `text-other`, `binary`: a unit by name only (G6).
        _ => Ok(Vec::new()),
    }
}

/// A code unit's symbols in file order, each with the ID that already
/// belongs to it, if any. A symbol with several IDs reports the first
/// one stored.
fn symbols(conn: &Connection, unit: &UnitRow) -> Result<Vec<InFileCandidate>, CacheError> {
    let mut ids: HashMap<i64, String> = HashMap::new();
    for block in units::blocks_for_unit(conn, unit.id)? {
        if let (Some(symbol_id), Some(block_id)) = (block.symbol_id, block.block_id) {
            ids.entry(symbol_id).or_insert(block_id);
        }
    }
    Ok(units::symbols_for_unit(conn, unit.id)?
        .into_iter()
        .map(|s| InFileCandidate::Symbol {
            block_id: ids.get(&s.id).cloned(),
            symbol_kind: s.kind,
            name: s.name,
            parent: s.parent,
            line: s.line,
        })
        .collect())
}
