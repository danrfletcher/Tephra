//! PDF: the text layer, page by page, and the outline as headings.
//!
//! Only text shown by the page content streams is read. Link
//! annotations (`/Annots`) are never opened, and there is no OCR: a PDF
//! with pages but no text gets no paragraphs and a
//! [`crate::DiagnosticKind::NoTextLayer`] diagnostic.
//!
//! The text of a page is split into lines by where text is placed, not
//! by how the generator split its operators: a text object boundary
//! (`BT`/`ET`) or a `Tm` on the same baseline does not start a new
//! line; a change of the line's page-space y (more than half an em),
//! `T*`, `'` or `"` does. Each non-empty line is one [`DocParagraph`],
//! numbered within its page.
//!
//! Runs on one baseline are joined without a space when the next run
//! starts where the last one ends, so a link a generator wrote
//! glyph-run by glyph-run (`[[`, `Note`, `]]`) still reads as one link.
//! Where the last run ends is worked out from the font's own widths
//! (`/FirstChar` + `/Widths`, or a CID font's `/W` and `/DW`) with the
//! `Tc`, `Tw`, `Tz` and `TJ` adjustments, so a long run of wide capitals
//! does not drift into a false word gap. Only a font with no usable
//! widths (the standard 14 fonts, Type 3, non-identity CID encodings)
//! falls back to a half-em-per-glyph guess, and then only a much wider
//! gap counts. A sideways gap past the end of a run is a word space, so
//! `(See) Tj 30 0 Td ([[Target]]) Tj` reads `See [[Target]]` and words
//! are not glued in the text that is searched.
//!
//! The PDF library parses untrusted bytes; [`crate::parse_document_bytes`]
//! runs this under `catch_unwind`, so a panic is a
//! [`DocumentError::Corrupt`].

use std::collections::{BTreeMap, HashMap};

use lopdf::content::Content;
use lopdf::{Document, Encoding, Error as PdfError, LoadOptions, Object};

use super::{DocHeading, DocLocation, DocParagraph, DocumentError, Extracted, MAX_PDF_PAGE_BYTES};
use crate::diagnostics::Diagnostic;

pub(crate) fn extract(bytes: &[u8]) -> Result<Extracted, DocumentError> {
    extract_inner(bytes)
}

fn extract_inner(bytes: &[u8]) -> Result<Extracted, DocumentError> {
    let doc = match Document::load_mem_with_options(
        bytes,
        LoadOptions::with_max_decompressed_size(MAX_PDF_PAGE_BYTES),
    ) {
        Ok(doc) => doc,
        Err(PdfError::InvalidPassword | PdfError::Decryption(_)) => {
            return Err(DocumentError::Encrypted)
        }
        Err(e) => return Err(DocumentError::Corrupt(format!("cannot parse the PDF: {e}"))),
    };
    // Still carrying an `/Encrypt` entry after load means the empty
    // password did not open it.
    if doc.is_encrypted() {
        return Err(DocumentError::Encrypted);
    }

    let pages = doc.get_pages();
    let mut paragraphs = Vec::new();
    for (&number, &page_id) in &pages {
        let page = number as usize;
        // A page whose content cannot be read has no text; the rest of
        // the document is still read.
        let Ok(text) = page_text(&doc, page_id) else {
            continue;
        };
        let mut index = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            index += 1;
            paragraphs.push(DocParagraph {
                location: DocLocation::Page(page),
                index,
                text: line.to_string(),
            });
        }
    }

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    if !pages.is_empty() && paragraphs.is_empty() {
        diagnostics.push(Diagnostic::no_text_layer(pages.len()));
    }

    let headings = match doc.get_toc() {
        Ok(toc) => toc
            .toc
            .into_iter()
            .filter_map(|entry| {
                let text = entry.title.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                Some(DocHeading {
                    level: entry.level.clamp(1, 6) as u8,
                    text,
                    location: DocLocation::Page(entry.page.max(1)),
                })
            })
            .collect(),
        // No outline, or one that cannot be followed: no headings.
        Err(_) => Vec::new(),
    };

    Ok(Extracted {
        headings,
        paragraphs,
        diagnostics,
    })
}

fn page_text(doc: &Document, page_id: lopdf::ObjectId) -> Result<String, PdfError> {
    let fonts = doc.get_page_fonts(page_id).unwrap_or_default();
    let fonts_for_widths = fonts.clone();
    let encodings: BTreeMap<Vec<u8>, Encoding<'_>> = fonts
        .into_iter()
        .filter_map(|(name, font)| {
            font.get_font_encoding_with_limit(doc, MAX_PDF_PAGE_BYTES)
                .ok()
                .map(|enc| (name, enc))
        })
        .collect();
    let widths: BTreeMap<Vec<u8>, FontWidths> = fonts_for_widths
        .into_iter()
        .filter_map(|(name, font)| FontWidths::of(doc, font).map(|w| (name, w)))
        .collect();
    let content = Content::decode(&doc.get_page_content_with_limit(page_id, MAX_PDF_PAGE_BYTES)?)?;

    let mut out = String::new();
    let mut encoding: Option<&Encoding<'_>> = None;
    let mut font_widths: Option<&FontWidths> = None;
    let mut pen = Pen::default();
    for op in &content.operations {
        let nums: Vec<f32> = op.operands.iter().filter_map(number).collect();
        match op.operator.as_str() {
            "Tf" => {
                encoding = op
                    .operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|name| encodings.get(name));
                font_widths = op
                    .operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|name| widths.get(name));
                pen.font_size = nums.first().copied().unwrap_or(pen.font_size);
            }
            "TL" => pen.leading = nums.first().copied().unwrap_or(0.0),
            "Tc" => pen.char_space = nums.first().copied().unwrap_or(0.0),
            "Tw" => pen.word_space = nums.first().copied().unwrap_or(0.0),
            "Tz" => pen.h_scale = nums.first().map_or(1.0, |z| z / 100.0),
            "q" => pen.save(),
            "Q" => pen.restore(),
            "cm" => {
                if let Some(m) = Matrix::from_operands(&nums) {
                    pen.ctm = m.then(&pen.ctm);
                }
            }
            // A text object starts with an identity text matrix. Where
            // its text lands is decided when text is shown, so a bare
            // BT or ET never splits a line.
            "BT" => pen.set_text_matrix(Matrix::IDENTITY),
            "Tm" => {
                if let Some(m) = Matrix::from_operands(&nums) {
                    pen.set_text_matrix(m);
                }
            }
            "Td" | "TD" => {
                if let [tx, ty, ..] = nums[..] {
                    if op.operator == "TD" {
                        pen.leading = -ty;
                    }
                    pen.move_line(tx, ty);
                }
            }
            "T*" => pen.next_line(),
            "Tj" | "TJ" => {
                if let Some(enc) = encoding {
                    pen.show(&mut out, enc, font_widths, &op.operands);
                }
            }
            "'" => {
                pen.next_line();
                if let Some(enc) = encoding {
                    pen.show(&mut out, enc, font_widths, &op.operands);
                }
            }
            "\"" => {
                if let [aw, ac, ..] = nums[..] {
                    pen.word_space = aw;
                    pen.char_space = ac;
                }
                pen.next_line();
                if let (Some(enc), Some(s)) = (encoding, op.operands.get(2)) {
                    pen.show(&mut out, enc, font_widths, std::slice::from_ref(s));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn number(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

/// A PDF matrix `[a b c d e f]` (row-vector convention).
#[derive(Clone, Copy)]
struct Matrix([f32; 6]);

impl Matrix {
    const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn from_operands(n: &[f32]) -> Option<Matrix> {
        let m: [f32; 6] = n.get(..6)?.try_into().ok()?;
        m.iter().all(|v| v.is_finite()).then_some(Matrix(m))
    }

    /// `self` applied first, then `next`.
    fn then(&self, next: &Matrix) -> Matrix {
        let [a, b, c, d, e, f] = self.0;
        let [na, nb, nc, nd, ne, nf] = next.0;
        Matrix([
            a * na + b * nc,
            a * nb + b * nd,
            c * na + d * nc,
            c * nb + d * nd,
            e * na + f * nc + ne,
            e * nb + f * nd + nf,
        ])
    }
}

/// Without a width table an average glyph is about half an em wide.
const GUESS_GLYPH_EM: f32 = 0.5;
/// A sideways gap wider than this many em between where the last run
/// ends (worked out from the font's widths) and where the next starts is
/// a word space. A real space is about a quarter em; kerning and
/// rounding stay well below this.
const WORD_GAP_EM: f32 = 0.2;
/// The same, when the last run's end is only guessed from the glyph
/// count: the guess drifts with the run length, so only a clearly wider
/// gap counts.
const GUESSED_WORD_GAP_EM: f32 = 0.6;
/// A vertical move of more than this many em starts a new line;
/// anything smaller is a superscript, subscript or rounding.
const LINE_STEP_EM: f32 = 0.5;
/// The most glyph widths read from one font, and the most `/W` ranges;
/// a font claiming more is untrusted and falls back to the guess.
const MAX_WIDTH_ENTRIES: usize = 70_000;
const MAX_WIDTH_RANGES: usize = 4_096;

/// The glyph widths of one font, in 1/1000 em, keyed by character code.
struct FontWidths {
    /// Bytes per character code: 1 for a simple font, 2 for a CID font
    /// with an identity encoding.
    code_bytes: usize,
    table: HashMap<u32, f32>,
    /// `/W` ranges `first last width` of a CID font.
    ranges: Vec<(u32, u32, f32)>,
    /// The width of a code the table does not list (`/MissingWidth` or
    /// `/DW`).
    default: f32,
}

impl FontWidths {
    /// The widths of `font`, or `None` when it has no table that can be
    /// trusted to describe how far each code advances.
    fn of(doc: &Document, font: &lopdf::Dictionary) -> Option<FontWidths> {
        match font.get(b"Subtype").and_then(Object::as_name).ok()? {
            b"Type0" => Self::of_cid(doc, font),
            // Type 3 widths are in glyph space, scaled by a font matrix.
            b"Type3" => None,
            _ => Self::of_simple(doc, font),
        }
    }

    fn of_simple(doc: &Document, font: &lopdf::Dictionary) -> Option<FontWidths> {
        let list = font.get_deref(b"Widths", doc).ok()?.as_array().ok()?;
        if list.len() > MAX_WIDTH_ENTRIES {
            return None;
        }
        let first = font
            .get_deref(b"FirstChar", doc)
            .ok()
            .and_then(|o| o.as_float().ok())
            .filter(|f| *f >= 0.0 && *f < 65_536.0)
            .unwrap_or(0.0) as u32;
        let missing = font
            .get_deref(b"FontDescriptor", doc)
            .ok()
            .and_then(|d| d.as_dict().ok())
            .and_then(|d| d.get_deref(b"MissingWidth", doc).ok())
            .and_then(|o| o.as_float().ok())
            .unwrap_or(0.0);
        let mut table = HashMap::new();
        for (i, w) in list.iter().enumerate() {
            if let Some(w) = deref_number(doc, w) {
                table.insert(first.saturating_add(i as u32), w);
            }
        }
        Some(FontWidths {
            code_bytes: 1,
            table,
            ranges: Vec::new(),
            default: missing,
        })
    }

    fn of_cid(doc: &Document, font: &lopdf::Dictionary) -> Option<FontWidths> {
        // Only the identity encodings have a fixed two-byte code; any
        // other CMap may mix code lengths.
        let encoding = font.get_deref(b"Encoding", doc).ok()?.as_name().ok()?;
        if encoding != b"Identity-H" && encoding != b"Identity-V" {
            return None;
        }
        let descendants = font
            .get_deref(b"DescendantFonts", doc)
            .ok()?
            .as_array()
            .ok()?;
        let (_, descendant) = doc.dereference(descendants.first()?).ok()?;
        let descendant = descendant.as_dict().ok()?;
        let default = descendant
            .get_deref(b"DW", doc)
            .ok()
            .and_then(|o| o.as_float().ok())
            .unwrap_or(1000.0);
        let mut widths = FontWidths {
            code_bytes: 2,
            table: HashMap::new(),
            ranges: Vec::new(),
            default,
        };
        let list = descendant
            .get_deref(b"W", doc)
            .ok()
            .and_then(|o| o.as_array().ok())
            .map(Vec::as_slice)
            .unwrap_or_default();
        // `c [w1 w2 ...]` gives consecutive codes from c; `c1 c2 w`
        // gives one width to the whole range.
        let mut i = 0;
        while i < list.len() {
            let first = deref_number(doc, &list[i])?;
            let next = doc.dereference(list.get(i + 1)?).ok()?.1;
            match next {
                Object::Array(items) => {
                    for (k, w) in items.iter().enumerate() {
                        if let Some(w) = deref_number(doc, w) {
                            widths.table.insert(first as u32 + k as u32, w);
                        }
                    }
                    i += 2;
                }
                _ => {
                    let last = deref_number(doc, next)?;
                    let w = deref_number(doc, list.get(i + 2)?)?;
                    widths.ranges.push((first as u32, last as u32, w));
                    i += 3;
                }
            }
            if widths.table.len() > MAX_WIDTH_ENTRIES || widths.ranges.len() > MAX_WIDTH_RANGES {
                return None;
            }
        }
        Some(widths)
    }

    fn width(&self, code: u32) -> f32 {
        if let Some(w) = self.table.get(&code) {
            return *w;
        }
        self.ranges
            .iter()
            .find(|(lo, hi, _)| (*lo..=*hi).contains(&code))
            .map_or(self.default, |(_, _, w)| *w)
    }
}

fn deref_number(doc: &Document, o: &Object) -> Option<f32> {
    let n = doc.dereference(o).ok()?.1.as_float().ok()?;
    n.is_finite().then_some(n)
}

/// How far a shown run moves the pen, before the font size and the
/// text state's spacing are applied.
#[derive(Default)]
struct Advance {
    /// Glyph widths and `TJ` adjustments, in em.
    em: f32,
    /// Character codes shown (each gets `Tc`).
    codes: usize,
    /// Single-byte code 32 shown (each also gets `Tw`).
    spaces: usize,
}

impl Advance {
    fn add_operands(&mut self, widths: &FontWidths, operands: &[Object]) {
        for operand in operands {
            match operand {
                Object::String(bytes, _) => {
                    for chunk in bytes.chunks(widths.code_bytes) {
                        let code = chunk.iter().fold(0u32, |c, b| c << 8 | u32::from(*b));
                        self.em += widths.width(code) / 1000.0;
                        self.codes += 1;
                        if widths.code_bytes == 1 && code == 32 {
                            self.spaces += 1;
                        }
                    }
                }
                Object::Array(items) => self.add_operands(widths, items),
                _ => self.add_adjustment(operand),
            }
        }
    }

    /// A `TJ` number moves the pen back by that many thousandths of an em.
    fn add_adjustment(&mut self, operand: &Object) {
        if let Some(n) = number(operand) {
            self.em -= n / 1000.0;
        }
    }

    /// With no widths: half an em per decoded char.
    fn guess(operands: &[Object], shown: &str) -> Advance {
        let mut adv = Advance::default();
        fn adjustments(adv: &mut Advance, operands: &[Object]) {
            for operand in operands {
                match operand {
                    Object::Array(items) => adjustments(adv, items),
                    Object::String(..) => {}
                    other => adv.add_adjustment(other),
                }
            }
        }
        adjustments(&mut adv, operands);
        let chars = shown.chars().count();
        adv.em += chars as f32 * GUESS_GLYPH_EM;
        adv.codes = chars;
        adv.spaces = shown.chars().filter(|c| *c == ' ').count();
        adv
    }
}

/// The text position as a page-content walk sees it, so lines are
/// decided from where text is placed and not from how the generator
/// split its operators.
struct Pen {
    ctm: Matrix,
    /// Saved by `q`: the matrix and the text state `Tc`, `Tw`, `Tz`.
    stack: Vec<(Matrix, f32, f32, f32)>,
    /// Text line matrix (the start of the current line, set by `Tm`,
    /// `Td`, `TD`, `T*`).
    line: Matrix,
    font_size: f32,
    leading: f32,
    /// `Tc`, in unscaled text space units.
    char_space: f32,
    /// `Tw`, in unscaled text space units.
    word_space: f32,
    /// `Tz` as a factor (1.0 is 100%).
    h_scale: f32,
    /// Placement operators ran since text was last shown.
    moved: bool,
    /// `T*`, `'` or `\"`: the next text starts a line whatever its y.
    force_break: bool,
    /// Page-space start y of the line being filled.
    line_y: Option<f32>,
    /// Page-space x where the last shown run ends.
    end_x: f32,
    /// `end_x` was worked out from the font's widths, not guessed.
    end_exact: bool,
}

impl Default for Pen {
    fn default() -> Pen {
        Pen {
            ctm: Matrix::IDENTITY,
            stack: Vec::new(),
            line: Matrix::IDENTITY,
            font_size: 1.0,
            leading: 0.0,
            char_space: 0.0,
            word_space: 0.0,
            h_scale: 1.0,
            moved: true,
            force_break: false,
            line_y: None,
            end_x: 0.0,
            end_exact: false,
        }
    }
}

impl Pen {
    fn save(&mut self) {
        self.stack
            .push((self.ctm, self.char_space, self.word_space, self.h_scale));
    }

    fn restore(&mut self) {
        if let Some((ctm, tc, tw, th)) = self.stack.pop() {
            self.ctm = ctm;
            self.char_space = tc;
            self.word_space = tw;
            self.h_scale = th;
        }
    }

    fn set_text_matrix(&mut self, m: Matrix) {
        self.line = m;
        self.moved = true;
    }

    fn move_line(&mut self, tx: f32, ty: f32) {
        self.line = Matrix([1.0, 0.0, 0.0, 1.0, tx, ty]).then(&self.line);
        self.moved = true;
    }

    fn next_line(&mut self) {
        self.move_line(0.0, -self.leading);
        self.force_break = true;
    }

    /// Shows `operands` at the current position: a new line when the
    /// y moved (or a `T*`-style operator ran), a space when the run
    /// starts clearly past where the last one ended, nothing otherwise.
    fn show(
        &mut self,
        out: &mut String,
        enc: &Encoding<'_>,
        widths: Option<&FontWidths>,
        operands: &[Object],
    ) {
        let page = self.line.then(&self.ctm);
        let [a, b, c, d, x, y] = page.0;
        let em = self.font_size.abs();
        let height = em * c.hypot(d);
        let width = em * a.hypot(b);
        let height = if height.is_finite() && height > 0.0 {
            height
        } else {
            1.0
        };
        let width = if width.is_finite() && width > 0.0 {
            width
        } else {
            1.0
        };

        let mut shown = String::new();
        show(&mut shown, enc, operands);
        if shown.is_empty() {
            return;
        }

        if self.moved || self.force_break {
            let new_line = self.force_break
                || self
                    .line_y
                    .is_some_and(|ly| (y - ly).abs() > LINE_STEP_EM * height);
            if new_line {
                newline(out);
                self.line_y = Some(y);
            } else {
                if self.line_y.is_none() {
                    self.line_y = Some(y);
                }
                let gap = x - self.end_x;
                let min_gap = if self.end_exact {
                    WORD_GAP_EM
                } else {
                    GUESSED_WORD_GAP_EM
                };
                let starts_blank = shown.starts_with(char::is_whitespace);
                let ends_blank = out.is_empty() || out.ends_with(char::is_whitespace);
                if gap > min_gap * width && !starts_blank && !ends_blank {
                    out.push(' ');
                }
            }
            self.end_x = x;
        }
        self.moved = false;
        self.force_break = false;
        out.push_str(&shown);

        let advance = match widths {
            Some(w) => {
                let mut adv = Advance::default();
                adv.add_operands(w, operands);
                adv
            }
            None => Advance::guess(operands, &shown),
        };
        self.end_exact = widths.is_some();
        let h_scale = if self.h_scale.is_finite() && self.h_scale > 0.0 {
            self.h_scale
        } else {
            1.0
        };
        let text_advance = (advance.em * self.font_size
            + advance.codes as f32 * self.char_space
            + advance.spaces as f32 * self.word_space)
            * h_scale;
        let moved_by = text_advance * a.hypot(b);
        if moved_by.is_finite() {
            self.end_x += moved_by;
        }
    }
}

fn newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Appends the text of `Tj`/`TJ` operands. Inside a `TJ` array a
/// kerning step wider than 100 units is a word space, but only when
/// the text before it does not already end in whitespace and the text
/// after it does not start with whitespace (CoreText writes a real
/// space glyph and then a large stretch kern on a justified line). A
/// kern at the very start or end of the operands adds nothing: the
/// pen's own gap check decides the space between runs. A string that
/// cannot be decoded is skipped.
fn show(out: &mut String, enc: &Encoding<'_>, operands: &[Object]) {
    let mut pending_space = false;
    show_into(out, enc, operands, &mut pending_space);
}

fn show_into(out: &mut String, enc: &Encoding<'_>, operands: &[Object], pending_space: &mut bool) {
    for operand in operands {
        match operand {
            Object::String(bytes, _) => {
                if let Ok(s) = Document::decode_text(enc, bytes) {
                    if s.is_empty() {
                        continue;
                    }
                    if *pending_space
                        && !out.is_empty()
                        && !out.ends_with(char::is_whitespace)
                        && !s.starts_with(char::is_whitespace)
                    {
                        out.push(' ');
                    }
                    *pending_space = false;
                    out.push_str(&s);
                }
            }
            Object::Array(items) => show_into(out, enc, items, pending_space),
            Object::Integer(i) if *i < -100 => *pending_space = true,
            Object::Real(r) if *r < -100.0 => *pending_space = true,
            _ => {}
        }
    }
}
