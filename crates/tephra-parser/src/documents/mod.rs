//! Documents (G5): text, headings and links from `.docx`, `.pptx` and
//! `.pdf` files. Read-only, pure, no I/O.
//!
//! [`parse_document_bytes`] takes a path (only its extension is looked
//! at) and the file's bytes and returns a [`ParsedDocument`]. Like
//! [`crate::parse_unit_bytes`] it opens nothing itself: the caller read
//! the bytes (and already refused a file over the 20 MB cap, G7, so an
//! oversized document never reaches an extractor).
//!
//! - **docx** ([`docx`]): the paragraphs (`w:p`) of `word/document.xml`,
//!   each paragraph's runs (`w:r`, however many Word split it into)
//!   joined before anything is scanned. Paragraphs styled `Title` or
//!   `Heading1`..`Heading6` are headings.
//! - **pptx** ([`pptx`]): the text of every slide, in the order of
//!   `ppt/presentation.xml`'s `sldIdLst`. A slide's title placeholder is
//!   a level-1 heading at that slide. Links in body text boxes, tables
//!   and the title are read; **speaker notes are not** (the notes parts
//!   are never opened).
//! - **pdf** ([`pdf`]): the text layer, page by page, and the outline
//!   (bookmarks) as headings at their page.
//!
//! Links are read from the extracted paragraph text, one paragraph at a
//! time, by the very scanners Markdown and code comments use
//! ([`crate::wikilinks`], [`crate::markdown_pass`]). There is no second
//! link grammar: a link form that works in a note works, with the same
//! result, in a document. Each link records where it was found, as a
//! [`DocLocation`] (page, slide or paragraph number) plus the paragraph's
//! index there.
//!
//! What is deliberately not read (Dan: A, F6):
//!
//! - docx hyperlinks (`w:hyperlink` targets, `HYPERLINK` field codes)
//!   and PDF link annotations (`/Annots`) are not links. Only typed
//!   `[[...]]` and `[text](target)` in the visible text count. (The
//!   visible text inside a docx `w:hyperlink` is still text.)
//! - No OCR. A PDF with no text layer gives no text, no links and no
//!   paragraphs, with a [`DiagnosticKind::NoTextLayer`] diagnostic.
//! - Nothing is read from images, audio or video: a docx/pptx's `media/`
//!   parts are never opened.
//!
//! A document that cannot be read is an `Err(DocumentError)`: the caller
//! keeps a name-only unit and logs a warning. The extractors never panic
//! on bad input (the PDF library is run under `catch_unwind`) and are
//! bounded: no single zip part is inflated past [`MAX_PART_BYTES`].
//!
//! Spans: a document link's `line` is its paragraph's index and its
//! `span` is a byte range inside that paragraph's text. Neither points
//! into the file (a document is never edited), so nothing may use them
//! to rewrite one.

mod docx;
mod pdf;
mod pptx;
mod xml;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::wikilinks;
use crate::{MarkdownLink, WikiLink};

use tephra_cache::{NewBlock, NewHeading, NewLink, NewUnit};

/// The most one part of a docx/pptx zip may inflate to (a zip bomb
/// guard). Real `document.xml` and slide parts are well under a few
/// MiB; this is far above that and keeps one document's peak memory
/// bounded by a small multiple of the file's own size cap.
pub const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// The most a PDF page's content stream may inflate to.
pub const MAX_PDF_PAGE_BYTES: usize = 64 * 1024 * 1024;

/// Which kind of document a file is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentFormat {
    Docx,
    Pptx,
    Pdf,
}

impl DocumentFormat {
    /// The format named by `path`'s extension (`docx`, `pptx`, `pdf`,
    /// any case), or `None`.
    pub fn from_path(path: &Path) -> Option<DocumentFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "docx" => Some(DocumentFormat::Docx),
            "pptx" => Some(DocumentFormat::Pptx),
            "pdf" => Some(DocumentFormat::Pdf),
            _ => None,
        }
    }
}

/// Where in a document a paragraph or link is. The number is 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocLocation {
    /// A PDF page.
    Page(usize),
    /// A pptx slide, numbered in `sldIdLst` order.
    Slide(usize),
    /// A docx paragraph: its 1-based position among every `w:p` of
    /// `word/document.xml`, in document order (empty ones count, so a
    /// number never moves because of a blank line's neighbours).
    Paragraph(usize),
}

impl DocLocation {
    /// The page, slide or paragraph number.
    pub fn number(self) -> usize {
        match self {
            DocLocation::Page(n) | DocLocation::Slide(n) | DocLocation::Paragraph(n) => n,
        }
    }
}

/// One paragraph of extracted text, with where it is. Empty paragraphs
/// are not listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocParagraph {
    pub location: DocLocation,
    /// 1-based position within its page or slide. For a docx, whose
    /// location already is the paragraph, the same number as
    /// `location`.
    pub index: usize,
    pub text: String,
}

/// A heading: a docx heading style, a pptx slide title or a PDF
/// outline entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocHeading {
    /// 1 through 6.
    pub level: u8,
    pub text: String,
    pub location: DocLocation,
}

/// A `[[wikilink]]` found in a document, as [`crate::parse_unit`]
/// would give it for the same text, plus where it was found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocWikiLink {
    pub location: DocLocation,
    /// The paragraph's [`DocParagraph::index`].
    pub index: usize,
    /// `link.line` is `index`; `link.span` is relative to the paragraph.
    pub link: WikiLink,
}

/// A `[text](target)` link found in a document, as for [`DocWikiLink`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocMarkdownLink {
    pub location: DocLocation,
    pub index: usize,
    pub link: MarkdownLink,
}

/// Why a document could not be read. In every case the unit is kept by
/// name only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentError {
    /// The path's extension is not `docx`, `pptx` or `pdf`.
    Unsupported,
    /// A PDF that needs a password to open.
    Encrypted,
    /// A zip that will not open, a missing part, malformed XML, a PDF
    /// that will not parse (or made the PDF library panic), or a part
    /// that inflates past [`MAX_PART_BYTES`].
    Corrupt(String),
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentError::Unsupported => write!(f, "not a docx, pptx or pdf file"),
            DocumentError::Encrypted => write!(f, "the PDF is encrypted and needs a password"),
            DocumentError::Corrupt(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for DocumentError {}

/// A document's structured parse result. Every field is a pure
/// function of `path`'s extension and the bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedDocument {
    pub path: PathBuf,
    /// Hex-encoded SHA-256 of the raw bytes.
    pub content_hash: String,
    pub format: DocumentFormat,
    /// Headings in document order.
    pub headings: Vec<DocHeading>,
    /// Every non-empty paragraph, in document order, with its location:
    /// what search (PR-9/PR-10) reads. Never carries a block ID.
    pub paragraphs: Vec<DocParagraph>,
    pub wikilinks: Vec<DocWikiLink>,
    pub markdown_links: Vec<DocMarkdownLink>,
    /// Non-fatal findings. A [`DiagnosticKind::NoTextLayer`] one is
    /// what a caller logs as the reason a PDF is name-only.
    pub diagnostics: Vec<Diagnostic>,
}

/// What an extractor hands back: text and structure, before any link
/// is looked for.
pub(crate) struct Extracted {
    pub headings: Vec<DocHeading>,
    pub paragraphs: Vec<DocParagraph>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parses the document `bytes` at `path` (its extension picks docx,
/// pptx or pdf). Pure: no file I/O, never writes anything, never
/// panics on malformed input.
pub fn parse_document_bytes(path: &Path, bytes: &[u8]) -> Result<ParsedDocument, DocumentError> {
    let format = DocumentFormat::from_path(path).ok_or(DocumentError::Unsupported)?;
    // The zip, XML and PDF readers parse untrusted bytes: a panic in
    // any of them is a corrupt document, never a crash.
    let extracted = catch_unwind(AssertUnwindSafe(|| match format {
        DocumentFormat::Docx => docx::extract(bytes),
        DocumentFormat::Pptx => pptx::extract(bytes),
        DocumentFormat::Pdf => pdf::extract(bytes),
    }))
    .unwrap_or_else(|_| {
        Err(DocumentError::Corrupt(
            "the document reader panicked on this file".to_string(),
        ))
    })?;
    Ok(finish(path, bytes, format, extracted))
}

/// Scans every paragraph for links, with the shared scanners.
fn finish(path: &Path, bytes: &[u8], format: DocumentFormat, ex: Extracted) -> ParsedDocument {
    let Extracted {
        headings,
        paragraphs,
        mut diagnostics,
    } = ex;
    let mut wikilinks_out = Vec::new();
    let mut markdown_out = Vec::new();
    for p in &paragraphs {
        if p.text.contains("[[") {
            let (links, _spans) = wikilinks::scan(&p.text, 1, 0, &[], &mut diagnostics);
            for mut link in links {
                link.line = p.index;
                wikilinks_out.push(DocWikiLink {
                    location: p.location,
                    index: p.index,
                    link,
                });
            }
        }
        let mut found = Vec::new();
        crate::code::scan_markdown_links(&p.text, 1, 0, &mut found, &mut diagnostics);
        for mut link in found {
            link.line = p.index;
            markdown_out.push(DocMarkdownLink {
                location: p.location,
                index: p.index,
                link,
            });
        }
    }
    // Findings about links carry a paragraph index, not a file line.
    ParsedDocument {
        path: path.to_path_buf(),
        content_hash: hash_bytes(bytes),
        format,
        headings,
        paragraphs,
        wikilinks: wikilinks_out,
        markdown_links: markdown_out,
        diagnostics,
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

impl Diagnostic {
    pub(crate) fn no_text_layer(pages: usize) -> Diagnostic {
        Diagnostic::new(
            DiagnosticKind::NoTextLayer,
            1,
            format!("PDF has {pages} page(s) but no text layer (scanned or image-only); indexed by name only, no OCR"),
        )
    }
}

impl ParsedDocument {
    /// This document's row for `tephra-cache`'s `units` table, given the
    /// `source_id`, relative path and mtime the caller knows. `kind` is
    /// `document`; the caller sets `size`.
    pub fn to_new_unit<'a>(
        &'a self,
        source_id: i64,
        rel_path: &'a str,
        mtime: Option<i64>,
    ) -> NewUnit<'a> {
        NewUnit {
            source_id,
            rel_path,
            mtime,
            frontmatter_json: None,
            kind: "document",
            size: None,
            too_large: false,
        }
    }

    /// One `NewLink` per wikilink and Markdown link, wikilinks first,
    /// each in document order. Same shape as
    /// [`crate::ParsedUnit::to_new_links`]; `links` has no column for a
    /// location, so a link's page/slide/paragraph stays on this struct.
    pub fn to_new_links(&self, from_unit_id: i64) -> Vec<NewLink<'_>> {
        let mut out: Vec<NewLink<'_>> = self
            .wikilinks
            .iter()
            .map(|w| NewLink {
                from_unit_id,
                to_raw: &w.link.raw,
                to_unit_id: None,
                kind: crate::LINK_KIND_WIKILINK,
            })
            .collect();
        out.extend(self.markdown_links.iter().map(|m| NewLink {
            from_unit_id,
            to_raw: &m.link.target,
            to_unit_id: None,
            kind: crate::LINK_KIND_MARKDOWN,
        }));
        out
    }

    /// One `headings` row per heading. The table's `line` column holds
    /// the heading's page, slide or paragraph number
    /// ([`DocLocation::number`]).
    pub fn to_new_headings(&self, unit_id: i64) -> Vec<NewHeading<'_>> {
        self.headings
            .iter()
            .map(|h| NewHeading {
                unit_id,
                level: i64::from(h.level),
                text: &h.text,
                line: h.location.number() as i64,
            })
            .collect()
    }

    /// One block-like `blocks` row per paragraph, with no `block_id`
    /// and no symbol: `line` is the paragraph's page, slide or
    /// paragraph number ([`DocLocation::number`]) and `end_line` its
    /// [`DocParagraph::index`] within it. The unit's extension says
    /// which of page, slide or paragraph the number is. The text is not
    /// stored (search populates its own index from a parse).
    pub fn to_new_blocks(&self, unit_id: i64) -> Vec<NewBlock<'_>> {
        self.paragraphs
            .iter()
            .map(|p| NewBlock {
                unit_id,
                block_id: None,
                line: p.location.number() as i64,
                end_line: Some(p.index as i64),
                symbol_id: None,
            })
            .collect()
    }
}

// Compile-time assertion that `ParsedDocument` is `Send + Sync`, for
// the parallel scan.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ParsedDocument>();
};
