//! `block.ensure_id` (G11, G12): give the paragraph or list item holding
//! a line of a prose unit, or the symbol a line or a name points at in a
//! code unit, a block ID, and return the link to it. The code half is in
//! [`code`].
//!
//! Layout, like [`crate::refactor`]: pure functions first, each tested on
//! its own, then [`BlockIdEngine`], which wires them to the real cache,
//! the real filesystem and the watcher.
//!
//! - [`locate_block`]: which block a line is in, or why it is in none.
//! - [`insert_marker`]: the new file content, which differs from the old
//!   by exactly one insertion of ` ^id` at the end of one line.
//! - [`new_id`]: a fresh 6-character `[a-z0-9]` ID, unique in the file.
//! - [`link_for`]: `[[@source:path#anchor]]`, dropping `.md` only.
//!
//! The write itself is [`crate::refactor::rewrite_links_atomic_with`]
//! (temp file, fsync, rename; permissions kept; symlinks refused), the
//! staleness gate compares the file on disk with the cache's row for it,
//! and the watcher is paused around the write and told which path to
//! forget, exactly as `link.apply_refactor` does. Nothing in here is
//! copied from [`crate::refactor`]: it all calls it.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::{Rng, RngCore, SeedableRng};
use serde::Serialize;
use tephra_cache::SourceStatus;
use tephra_parser::{Heading, LineRegion, ParsedUnit, RegionKind};

use crate::file_lock;
use crate::refactor::{rewrite_links_atomic_with, AtomicFs, RealFs, WatcherPauseControl};
use crate::registry::SourceRegistry;
use crate::watcher;

pub mod code;

/// A new ID's length.
pub const ID_LEN: usize = 6;

/// The characters a new ID is drawn from.
const ID_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// How many draws [`new_id`] makes before giving up. A draw collides with
/// probability (IDs in the file) / 36^6, so reaching this means a broken
/// random source, never a real file.
const MAX_DRAWS: usize = 64;

// ---------------------------------------------------------------
// Lines
// ---------------------------------------------------------------

/// Where each line of `content` is: `(start, end)` byte offsets, `end`
/// before the line's `\n` or `\r\n`. A final line with no line ending is a
/// line; the empty text after the last line ending is not.
pub(crate) fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            spans.push((start, end));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }
    spans
}

// ---------------------------------------------------------------
// What a request points at
// ---------------------------------------------------------------

/// What `block.ensure_id` was asked to give an ID: a line of the file
/// (prose and code), or a symbol of a code file by name (`name` or
/// `Parent.name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTarget {
    /// A line, counting from 1.
    Line(u64),
    Symbol(String),
}

// ---------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------

/// Why a line cannot get a block ID, or be linked to at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// Lines count from 1.
    LineZero,
    OutOfRange {
        line: usize,
        total: usize,
    },
    Blank(usize),
    Frontmatter(usize),
    CodeBlock(usize),
    Table(usize),
    /// A non-blank line that is in no paragraph, list item or heading
    /// (a rule, a block delimiter, an HTML block).
    NotABlock(usize),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocateError::LineZero => write!(f, "line 0 is out of range: lines start at 1"),
            LocateError::OutOfRange { line, total } => write!(
                f,
                "line {line} is out of range: the file has {total} line{}",
                if *total == 1 { "" } else { "s" }
            ),
            LocateError::Blank(line) => write!(
                f,
                "line {line} is a blank line: pick a line inside a paragraph or list item"
            ),
            LocateError::Frontmatter(line) => write!(
                f,
                "line {line} is inside the frontmatter: block IDs go in the body of a note"
            ),
            LocateError::CodeBlock(line) => write!(
                f,
                "line {line} is inside a code block (code fence): a block ID there would be part of the code"
            ),
            LocateError::Table(line) => write!(
                f,
                "line {line} is inside a table: a block ID there would break the table"
            ),
            LocateError::NotABlock(line) => write!(
                f,
                "line {line} is not part of a paragraph, list item or heading"
            ),
        }
    }
}

/// Everything that makes `block.ensure_id` refuse, with nothing written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockIdError {
    /// The request itself is malformed (`INVALID_PARAMS`).
    InvalidParams(String),
    /// A valid request that cannot be done; the message says why.
    Refused(String),
}

impl fmt::Display for BlockIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockIdError::InvalidParams(m) | BlockIdError::Refused(m) => f.write_str(m),
        }
    }
}

impl From<LocateError> for BlockIdError {
    fn from(e: LocateError) -> Self {
        BlockIdError::Refused(e.to_string())
    }
}

pub(crate) fn refused(message: impl Into<String>) -> BlockIdError {
    BlockIdError::Refused(message.into())
}

// ---------------------------------------------------------------
// locate_block
// ---------------------------------------------------------------

/// What a line is in.
#[derive(Debug, Clone, PartialEq)]
pub enum Located<'a> {
    /// A heading (all of its lines, setext and rst underlines included):
    /// linked by its text, never given an ID.
    Heading(&'a Heading),
    /// A paragraph or one list item's own text, by its line range, with
    /// the `^id` that already ends its last line, if any.
    Block {
        start_line: usize,
        end_line: usize,
        id: Option<&'a str>,
    },
}

/// Which block `line` (1-indexed) of `content` is in. `parsed` and
/// `regions` are [`tephra_parser::parse_unit`]'s and
/// [`tephra_parser::line_regions`]'s answers for the same `content`.
///
/// The rules, in order: line 0 and a line past the end are out of range; a
/// line in the frontmatter, a heading, a code block or a table is that
/// (a blank line inside a fence is in the fence); a blank line is blank; a
/// line in a paragraph or list item is that block; anything else is in no
/// block.
pub fn locate_block<'a>(
    content: &str,
    parsed: &'a ParsedUnit,
    regions: &[LineRegion],
    line: usize,
) -> Result<Located<'a>, LocateError> {
    let spans = line_spans(content);
    if line == 0 {
        return Err(LocateError::LineZero);
    }
    if line > spans.len() {
        return Err(LocateError::OutOfRange {
            line,
            total: spans.len(),
        });
    }

    let inside = |r: &&LineRegion| r.start_line <= line && line <= r.end_line;
    if regions
        .iter()
        .filter(inside)
        .any(|r| r.kind == RegionKind::Frontmatter)
    {
        return Err(LocateError::Frontmatter(line));
    }
    for region in regions.iter().filter(inside) {
        if let RegionKind::Heading(index) = region.kind {
            if let Some(heading) = parsed.headings.get(index) {
                return Ok(Located::Heading(heading));
            }
        }
    }
    for region in regions.iter().filter(inside) {
        match region.kind {
            RegionKind::CodeBlock => return Err(LocateError::CodeBlock(line)),
            RegionKind::Table => return Err(LocateError::Table(line)),
            _ => {}
        }
    }

    let (start, end) = spans[line - 1];
    if content[start..end].trim().is_empty() {
        return Err(LocateError::Blank(line));
    }

    match parsed
        .paragraphs
        .iter()
        .find(|p| p.start_line <= line && line <= p.end_line)
    {
        Some(p) => Ok(Located::Block {
            start_line: p.start_line,
            end_line: p.end_line,
            id: p.block_id.as_deref(),
        }),
        None => Err(LocateError::NotABlock(line)),
    }
}

// ---------------------------------------------------------------
// insert_marker
// ---------------------------------------------------------------

/// `content` with ` ^id` inserted at the end of line `line` (1-indexed),
/// before that line's `\n` or `\r\n`, or at the end of the file when the
/// line is the last and has no line ending. Every other byte is kept as it
/// is: other lines, line endings, trailing whitespace (a hard break's two
/// spaces stay, and the marker goes after them), a BOM. The insertion
/// point is a line end, so always a character boundary. `None` when the
/// file has no such line.
pub fn insert_marker(content: &str, line: usize, id: &str) -> Option<String> {
    let spans = line_spans(content);
    let (_, end) = *spans.get(line.checked_sub(1)?)?;
    let mut out = String::with_capacity(content.len() + id.len() + 2);
    out.push_str(&content[..end]);
    out.push_str(" ^");
    out.push_str(id);
    out.push_str(&content[end..]);
    Some(out)
}

// ---------------------------------------------------------------
// new_id
// ---------------------------------------------------------------

/// The IDs this file already uses, lower-cased: Obsidian reads block IDs
/// without regard to case, so `^AB12CD` and `^ab12cd` would clash.
pub fn ids_in_file(parsed: &ParsedUnit) -> HashSet<String> {
    parsed
        .blocks
        .iter()
        .map(|b| b.block_id.to_ascii_lowercase())
        .chain(
            parsed
                .paragraphs
                .iter()
                .filter_map(|p| p.block_id.as_deref())
                .map(str::to_ascii_lowercase),
        )
        .collect()
}

/// A new block ID: [`ID_LEN`] characters of `[a-z0-9]`, not in `existing`
/// (a collision is redrawn). `rng` is injected so a test can make the
/// first draw collide. `None` only if [`MAX_DRAWS`] draws in a row all
/// collide.
pub fn new_id<R: Rng + ?Sized>(existing: &HashSet<String>, rng: &mut R) -> Option<String> {
    for _ in 0..MAX_DRAWS {
        let id: String = (0..ID_LEN)
            .map(|_| ID_ALPHABET[rng.gen_range(0..ID_ALPHABET.len())] as char)
            .collect();
        if !existing.contains(&id) {
            return Some(id);
        }
    }
    None
}

// ---------------------------------------------------------------
// link_for
// ---------------------------------------------------------------

/// `[[@source:path#anchor]]`: `.md` is dropped from `rel_path` (Obsidian
/// style) and every other extension is kept. The path is written as it is,
/// spaces and non-ASCII included. `anchor` is `^id` for a block or the
/// heading text for a heading.
pub fn link_for(source: &str, rel_path: &str, anchor: &str) -> String {
    let path = rel_path.strip_suffix(".md").unwrap_or(rel_path);
    format!("[[@{source}:{path}#{anchor}]]")
}

/// The first character of `rel_path` that cannot sit in a wikilink target
/// (`[`, `]`, `|`, `#`, `^`), if any: a link built from such a path would
/// read as something else, so a request for one is refused before any
/// write.
pub fn unlinkable_char(rel_path: &str) -> Option<char> {
    rel_path
        .chars()
        .find(|c| matches!(c, '[' | ']' | '|' | '#' | '^'))
}

/// A heading's text as a link anchor: characters wikilinks give a meaning
/// to (`# | ^ [ ] :`) become spaces, and runs of whitespace one space, as
/// Obsidian does when it writes a heading link. `None` if nothing is left.
pub fn heading_anchor(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if matches!(c, '#' | '|' | '^' | '[' | ']' | ':') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}

// ---------------------------------------------------------------
// plan_ensure: the pure core
// ---------------------------------------------------------------

/// What `block.ensure_id` decided for one file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// A heading: the anchor to link by. Nothing to write.
    Heading { anchor: String },
    /// The block already has this ID. Nothing to write.
    Existing { id: String },
    /// A new ID to write: the content with it inserted.
    Insert { id: String, new_content: String },
}

/// Decides what to do for `line` of `content` (the prose file at `path`,
/// whose extension picks the format). Pure: no IO, and the random source
/// is injected. For [`Plan::Insert`] the new content has been parsed again
/// and checked to hold the ID as the end of the very same block, and to
/// hold it once, so the marker can never land somewhere the parser would
/// not read it back (the end of a line inside an unclosed code span, say).
pub fn plan_ensure<R: Rng + ?Sized>(
    path: &Path,
    content: &str,
    line: usize,
    rng: &mut R,
) -> Result<Plan, BlockIdError> {
    let parsed = tephra_parser::parse_unit(path, content);
    let regions = tephra_parser::line_regions(path, content);
    match locate_block(content, &parsed, &regions, line)? {
        Located::Heading(heading) => heading_anchor(&heading.text)
            .map(|anchor| Plan::Heading { anchor })
            .ok_or_else(|| {
                refused(format!(
                    "line {line} is a heading with no text a link can use"
                ))
            }),
        Located::Block { id: Some(id), .. } => Ok(Plan::Existing { id: id.to_string() }),
        Located::Block {
            start_line,
            end_line,
            id: None,
        } => {
            let id = new_id(&ids_in_file(&parsed), rng)
                .ok_or_else(|| refused("could not draw an unused block ID"))?;
            let new_content = insert_marker(content, end_line, &id)
                .ok_or_else(|| refused(format!("line {end_line} is not in the file")))?;
            let after = tephra_parser::parse_unit(path, &new_content);
            let holds_it = after.paragraphs.iter().any(|p| {
                p.start_line == start_line
                    && p.end_line == end_line
                    && p.block_id.as_deref() == Some(id.as_str())
            }) && after.blocks.iter().filter(|b| b.block_id == id).count() == 1;
            if !holds_it {
                return Err(refused(format!(
                    "the end of line {end_line} cannot carry a block ID (it is inside inline code, markup or a literal block)"
                )));
            }
            Ok(Plan::Insert { id, new_content })
        }
    }
}

// ---------------------------------------------------------------
// Wire shape
// ---------------------------------------------------------------

/// `block.ensure_id`'s result. `id` is `null` for a heading, which has no
/// block ID (its link is by heading text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnsureIdWire {
    pub id: Option<String>,
    pub link: String,
    pub created: bool,
}

// ---------------------------------------------------------------
// The engine
// ---------------------------------------------------------------

/// The watcher pause, shared by every `ensure_id` in flight: the first
/// one in pauses, the last one out waits out the debounce window, then
/// resumes telling the watcher every path written meanwhile. Overlap
/// with other writers (`link.apply_refactor`) is handled below this, in
/// the [`WatcherPauseControl`] the real registry provides
/// ([`SourceRegistry::pause_for_write`]).
struct PauseState {
    active: usize,
    written: HashSet<PathBuf>,
}

/// Orchestrates `block.ensure_id` against the real [`SourceRegistry`]
/// (cache, sources), an [`AtomicFs`] and a [`WatcherPauseControl`]. The
/// two seams exist for the tests; production uses [`RealFs`] and the
/// registry itself.
pub struct BlockIdEngine {
    registry: Arc<SourceRegistry>,
    fs: Arc<dyn AtomicFs>,
    watcher: Arc<dyn WatcherPauseControl>,
    rng: Mutex<Box<dyn RngCore + Send>>,
    settle: Duration,
    pause: Mutex<PauseState>,
}

/// The mtime second the cache records for a file; a file on disk is "as
/// Tephra last read it" when this and its size match the cache's row.
fn mtime_secs(meta: &fs::Metadata) -> Option<i64> {
    let since_epoch = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs() as i64)
}

/// What identifies the exact bytes of a file between two looks at it.
fn fingerprint(meta: &fs::Metadata) -> (u64, Option<SystemTime>) {
    (meta.len(), meta.modified().ok())
}

const STALE: &str = "file changed, try again";

impl BlockIdEngine {
    /// The production wiring: real filesystem, the registry's own
    /// watchers, a random source seeded from the OS, and the same
    /// settle time `link.apply_refactor` waits before resuming.
    pub fn new(registry: Arc<SourceRegistry>) -> Self {
        let watcher = Arc::clone(&registry) as Arc<dyn WatcherPauseControl>;
        Self::with_parts(registry, Arc::new(RealFs), watcher)
    }

    /// [`BlockIdEngine::new`] with the filesystem and watcher control
    /// injected.
    pub fn with_parts(
        registry: Arc<SourceRegistry>,
        fs: Arc<dyn AtomicFs>,
        watcher: Arc<dyn WatcherPauseControl>,
    ) -> Self {
        BlockIdEngine {
            registry,
            fs,
            watcher,
            rng: Mutex::new(Box::new(rand::rngs::StdRng::from_entropy())),
            settle: watcher::DEBOUNCE_WINDOW + Duration::from_millis(150),
            pause: Mutex::new(PauseState {
                active: 0,
                written: HashSet::new(),
            }),
        }
    }

    /// Replaces the random source (tests seed it).
    pub fn with_rng(mut self, rng: Box<dyn RngCore + Send>) -> Self {
        self.rng = Mutex::new(rng);
        self
    }

    /// Replaces how long the last write in flight waits for its own
    /// filesystem event to reach the paused watcher before resuming it.
    /// Tests with a recording (not a live) watcher set it to zero.
    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// `block.ensure_id {unit, line}`: [`BlockIdEngine::ensure_target`]
    /// for a line.
    pub fn ensure_id(&self, unit: &str, line: u64) -> Result<EnsureIdWire, BlockIdError> {
        self.ensure_target(unit, &BlockTarget::Line(line))
    }

    /// `block.ensure_id {unit, line | symbol}`: see the module doc and
    /// G11/G12. Everything that can be refused is refused before anything
    /// is written; the write is one atomic replace of the file.
    pub fn ensure_target(
        &self,
        unit: &str,
        target: &BlockTarget,
    ) -> Result<EnsureIdWire, BlockIdError> {
        let (source, rel_path) = crate::search::in_file::split_unit_id(unit).ok_or_else(|| {
            BlockIdError::InvalidParams(format!(
                "\"unit\" must be a unit id like \"@source:path/to/file\", got \"{unit}\""
            ))
        })?;

        // A first look, for the early refusals and the file's path. The
        // row is read again below once this request holds the file's
        // lock: a request that waited for another writer of the same
        // file must see the cache as that writer left it, not as it
        // was when this one arrived.
        let (root, _) = self.checked_row(unit, source, rel_path, target)?;
        let abs = safe_join(&root, rel_path)?;

        // One writer per file from the first look at it to the rename.
        let file_guard = file_lock::lock(&abs);
        let (_, row) = self.checked_row(unit, source, rel_path, target)?;

        let before = fs::symlink_metadata(&abs).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                refused(format!("{STALE} ({unit} no longer exists)"))
            } else {
                refused(format!("cannot read {unit}: {e}"))
            }
        })?;
        if before.file_type().is_symlink() {
            return Err(refused(format!(
                "refusing to write {unit}: it is a symlink, and rewriting it would replace the link itself with a plain file"
            )));
        }
        if !before.is_file() {
            return Err(refused(format!("{unit} is not a regular file")));
        }
        // E6: the file must be as the cache last saw it.
        let cache_is_current = row.mtime.is_some()
            && !row.needs_rescan
            && mtime_secs(&before) == row.mtime
            && row.size == Some(before.len() as i64);
        if !cache_is_current {
            return Err(refused(STALE));
        }

        let bytes = fs::read(&abs).map_err(|e| refused(format!("cannot read {unit}: {e}")))?;
        if bytes.len() as u64 != before.len() {
            return Err(refused(STALE));
        }
        let content = String::from_utf8(bytes).map_err(|_| {
            refused(format!(
                "{unit} is not valid UTF-8, so Tephra will not write to it"
            ))
        })?;

        let plan = {
            let mut rng = self.rng.lock().unwrap_or_else(|p| p.into_inner());
            if row.kind == "code" {
                code::plan_code(Path::new(rel_path), unit, &content, target, &mut **rng)?
            } else {
                // `checked_row` has refused a symbol request for a
                // prose file, so this is a line.
                let BlockTarget::Line(line) = target else {
                    return Err(symbol_needs_code(unit));
                };
                let line = usize::try_from(*line).unwrap_or(usize::MAX);
                plan_ensure(Path::new(rel_path), &content, line, &mut **rng)?
            }
        };
        let (id, new_content) = match plan {
            Plan::Heading { anchor } => {
                return Ok(EnsureIdWire {
                    id: None,
                    link: link_for(source, rel_path, &anchor),
                    created: false,
                })
            }
            Plan::Existing { id } => {
                return Ok(EnsureIdWire {
                    link: link_for(source, rel_path, &format!("^{id}")),
                    id: Some(id),
                    created: false,
                })
            }
            Plan::Insert { id, new_content } => (id, new_content),
        };

        // Just before the write: still the file we read?
        match fs::symlink_metadata(&abs) {
            Ok(now) if fingerprint(&now) == fingerprint(&before) => {}
            _ => return Err(refused(STALE)),
        }

        self.begin_write();
        let write = rewrite_links_atomic_with(self.fs.as_ref(), &abs, new_content.as_bytes());
        let mut written = None;
        let outcome = match write {
            Err(e) => Err(refused(format!("could not write {unit}: {e}"))),
            Ok(()) => {
                // The cache records the ID before this returns. If that
                // fails the file is already changed, so the watcher must
                // still see this write and reindex it: don't tell it to
                // forget the path.
                match self.registry.reindex_file(source, &abs) {
                    Ok(()) => written = Some(abs.clone()),
                    Err(e) => tracing::warn!(
                        unit = %unit,
                        error = %e,
                        "block ID written but the cache could not be updated; leaving it to the watcher"
                    ),
                }
                Ok(EnsureIdWire {
                    link: link_for(source, rel_path, &format!("^{id}")),
                    id: Some(id),
                    created: true,
                })
            }
        };
        // Let the next writer of this file go before the settle wait.
        drop(file_guard);
        self.end_write(written);
        outcome
    }

    /// The source's root and the cached row for `rel_path`, after every
    /// refusal that depends only on the cache: no such source or unit,
    /// a source that cannot be written, a kind Tephra never writes, a
    /// file too large to be indexed, a path no wikilink can hold.
    fn checked_row(
        &self,
        unit: &str,
        source: &str,
        rel_path: &str,
        target: &BlockTarget,
    ) -> Result<(PathBuf, tephra_cache::UnitRow), BlockIdError> {
        let cached = self
            .registry
            .cached_unit(source, rel_path)
            .map_err(|e| refused(e.to_string()))?
            .ok_or_else(|| {
                refused(format!(
                    "no such unit: {unit} (no source named \"{source}\")"
                ))
            })?;
        if cached.permission_denied {
            return Err(refused(format!(
                "source \"{source}\" is permission denied: Tephra cannot read or write its files"
            )));
        }
        if cached.status == SourceStatus::Offline {
            return Err(refused(format!(
                "source \"{source}\" is offline: its files cannot be written"
            )));
        }
        let row = cached
            .unit
            .ok_or_else(|| refused(format!("no such unit: {unit}")))?;
        match row.kind.as_str() {
            "prose" => {
                if matches!(target, BlockTarget::Symbol(_)) {
                    return Err(symbol_needs_code(unit));
                }
            }
            "code" => {}
            "document" => {
                return Err(refused(format!(
                    "{unit} is a document (pdf, docx or pptx): Tephra never writes to documents"
                )))
            }
            "text-other" => {
                return Err(refused(format!(
                    "{unit} is a text file of another type: block IDs go only into prose files (md, markdown, txt, org, rst, adoc) and code files with a bundled grammar"
                )))
            }
            _ => {
                return Err(refused(format!(
                    "{unit} is a binary file: Tephra never writes to binary files"
                )))
            }
        }
        if row.too_large {
            return Err(refused(format!(
                "{unit} is over the size limit and is indexed by name only"
            )));
        }
        if let Some(c) = unlinkable_char(rel_path) {
            return Err(refused(format!(
                "{unit} has a '{c}' in its path, which a [[wikilink]] cannot hold, so no link to it can be made"
            )));
        }
        Ok((cached.root, row))
    }

    fn begin_write(&self) {
        let mut state = self.pause.lock().unwrap_or_else(|p| p.into_inner());
        if state.active == 0 {
            self.watcher.pause_all();
        }
        state.active += 1;
    }

    /// The last write out waits for the filesystem events of every write
    /// in the batch to land in the paused watcher's queue (see
    /// [`WatcherPauseControl`]), then resumes it, discarding exactly the
    /// paths written.
    fn end_write(&self, written: Option<PathBuf>) {
        let mut state = self.pause.lock().unwrap_or_else(|p| p.into_inner());
        state.written.extend(written);
        state.active -= 1;
        if state.active == 0 {
            if !self.settle.is_zero() {
                std::thread::sleep(self.settle);
            }
            let paths = std::mem::take(&mut state.written);
            self.watcher.resume_all(&paths);
        }
    }
}

/// A `symbol` request for a file that has no symbols.
fn symbol_needs_code(unit: &str) -> BlockIdError {
    refused(format!(
        "{unit} is not a code file, so it has no symbols: ask for a line instead"
    ))
}

/// `root` joined with `rel_path`, refusing anything that is not a plain
/// relative path inside `root`.
fn safe_join(root: &Path, rel_path: &str) -> Result<PathBuf, BlockIdError> {
    let rel = Path::new(rel_path);
    if rel.components().all(|c| matches!(c, Component::Normal(_))) {
        Ok(root.join(rel))
    } else {
        Err(refused(format!(
            "\"{rel_path}\" is not a path inside the source"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;

    // -- helpers ------------------------------------------------

    fn located(path: &str, content: &str, line: usize) -> Result<String, LocateError> {
        let path = Path::new(path);
        let parsed = tephra_parser::parse_unit(path, content);
        let regions = tephra_parser::line_regions(path, content);
        locate_block(content, &parsed, &regions, line).map(|l| match l {
            Located::Heading(h) => format!("heading:{}", h.text),
            Located::Block {
                start_line,
                end_line,
                id,
            } => format!("block:{start_line}-{end_line}:{}", id.unwrap_or("-")),
        })
    }

    // -- locate_block -------------------------------------------

    /// One table for every rule of G11 prose, for every prose extension:
    /// `(file name, content, line, expected)`.
    #[test]
    fn locate_block_follows_every_g11_prose_rule_for_every_extension() {
        use LocateError::*;
        let ok = |s: &str| Ok(s.to_string());
        let md_doc = "---\ntitle: T\n---\n# Head\n\nfirst para\nsecond line\n\n- item one\n- item two ^kept\n\n```rust\nlet x = 1;\n\nlet y = 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n    indented code\n\nlast ^end\n";
        let org = "* Heading\nbody line\nmore\n\n#+BEGIN_SRC rust\ncode\n#+END_SRC\n";
        let rst = "Title\n=====\n\nbody text\n\n::\n\n    literal\n";
        let adoc = "== Title\n\nparagraph\nmore\n\n----\ncode\n----\n";
        let cases: Vec<(&str, &str, usize, Result<String, LocateError>)> = vec![
            // markdown: every kind of line
            ("n.md", md_doc, 1, Err(Frontmatter(1))),
            ("n.md", md_doc, 2, Err(Frontmatter(2))),
            ("n.md", md_doc, 3, Err(Frontmatter(3))),
            ("n.md", md_doc, 4, ok("heading:Head")),
            ("n.md", md_doc, 5, Err(Blank(5))),
            ("n.md", md_doc, 6, ok("block:6-7:-")),
            ("n.md", md_doc, 7, ok("block:6-7:-")),
            ("n.md", md_doc, 9, ok("block:9-9:-")),
            ("n.md", md_doc, 10, ok("block:10-10:kept")),
            ("n.md", md_doc, 12, Err(CodeBlock(12))),
            ("n.md", md_doc, 13, Err(CodeBlock(13))),
            ("n.md", md_doc, 14, Err(CodeBlock(14))),
            ("n.md", md_doc, 16, Err(CodeBlock(16))),
            ("n.md", md_doc, 18, Err(Table(18))),
            ("n.md", md_doc, 19, Err(Table(19))),
            ("n.md", md_doc, 20, Err(Table(20))),
            ("n.md", md_doc, 22, Err(CodeBlock(22))),
            ("n.md", md_doc, 24, ok("block:24-24:end")),
            ("n.md", md_doc, 0, Err(LineZero)),
            (
                "n.md",
                md_doc,
                25,
                Err(OutOfRange {
                    line: 25,
                    total: 24,
                }),
            ),
            ("n.markdown", "para\nmore\n", 2, ok("block:1-2:-")),
            // markdown setext heading: both lines
            ("n.md", "Title\n=====\n\ntext\n", 1, ok("heading:Title")),
            ("n.md", "Title\n=====\n\ntext\n", 2, ok("heading:Title")),
            // txt: a paragraph is the run of lines between blank lines
            ("n.txt", "one\ntwo\nthree\n\nfour\n", 2, ok("block:1-3:-")),
            ("n.txt", "one\ntwo\nthree\n\nfour\n", 4, Err(Blank(4))),
            ("n.txt", "one\ntwo\nthree\n\nfour\n", 5, ok("block:5-5:-")),
            ("n.txt", "one\ntwo ^abc1\n", 1, ok("block:1-2:abc1")),
            ("n.txt", "one\n", 3, Err(OutOfRange { line: 3, total: 1 })),
            ("n.txt", "", 1, Err(OutOfRange { line: 1, total: 0 })),
            // org
            ("n.org", org, 1, ok("heading:Heading")),
            ("n.org", org, 3, ok("block:2-3:-")),
            ("n.org", org, 5, Err(CodeBlock(5))),
            ("n.org", org, 6, Err(CodeBlock(6))),
            // rst: an underlined title (both lines), a literal block
            ("n.rst", rst, 1, ok("heading:Title")),
            ("n.rst", rst, 2, ok("heading:Title")),
            ("n.rst", rst, 4, ok("block:4-4:-")),
            ("n.rst", rst, 8, Err(CodeBlock(8))),
            // adoc
            ("n.adoc", adoc, 1, ok("heading:Title")),
            ("n.adoc", adoc, 4, ok("block:3-4:-")),
            ("n.adoc", adoc, 7, Err(CodeBlock(7))),
        ];
        for (name, content, line, want) in cases {
            assert_eq!(
                located(name, content, line),
                want,
                "{name} line {line} of {content:?}"
            );
        }
    }

    #[test]
    fn locate_block_gives_a_nested_list_item_its_own_block() {
        let content = "- parent\n  - child\n- sibling\n";
        assert_eq!(located("n.md", content, 1).unwrap(), "block:1-1:-");
        assert_eq!(located("n.md", content, 2).unwrap(), "block:2-2:-");
        assert_eq!(located("n.md", content, 3).unwrap(), "block:3-3:-");
    }

    #[test]
    fn locate_block_reports_the_reason_in_words() {
        for (err, word) in [
            (LocateError::LineZero, "line 0"),
            (
                LocateError::OutOfRange { line: 9, total: 3 },
                "out of range",
            ),
            (LocateError::Blank(2), "blank"),
            (LocateError::Frontmatter(1), "frontmatter"),
            (LocateError::CodeBlock(4), "code"),
            (LocateError::Table(4), "table"),
        ] {
            assert!(err.to_string().contains(word), "{err}");
        }
    }

    // -- insert_marker ------------------------------------------

    #[test]
    fn insert_marker_is_byte_exact_for_lf() {
        assert_eq!(
            insert_marker("a\nb\nc\n", 2, "k3f9a2").unwrap(),
            "a\nb ^k3f9a2\nc\n"
        );
        assert_eq!(
            insert_marker("a\nb\nc\n", 1, "k3f9a2").unwrap(),
            "a ^k3f9a2\nb\nc\n"
        );
    }

    #[test]
    fn insert_marker_puts_the_id_before_crlf() {
        assert_eq!(
            insert_marker("a\r\nb\r\nc\r\n", 2, "k3f9a2").unwrap(),
            "a\r\nb ^k3f9a2\r\nc\r\n"
        );
        // Mixed endings: only the named line's own ending matters.
        assert_eq!(
            insert_marker("a\nb\r\nc", 3, "k3f9a2").unwrap(),
            "a\nb\r\nc ^k3f9a2"
        );
    }

    #[test]
    fn insert_marker_goes_at_eof_without_adding_a_newline() {
        assert_eq!(
            insert_marker("a\nlast", 2, "k3f9a2").unwrap(),
            "a\nlast ^k3f9a2"
        );
        assert_eq!(insert_marker("only", 1, "k3f9a2").unwrap(), "only ^k3f9a2");
    }

    #[test]
    fn insert_marker_keeps_unicode_intact_and_lands_on_a_char_boundary() {
        let content = "héllo wörld 🎉\nnext\n";
        let out = insert_marker(content, 1, "k3f9a2").unwrap();
        assert_eq!(out, "héllo wörld 🎉 ^k3f9a2\nnext\n");
        let out = insert_marker("日本語のテキスト🙂", 1, "k3f9a2").unwrap();
        assert_eq!(out, "日本語のテキスト🙂 ^k3f9a2");
    }

    #[test]
    fn insert_marker_keeps_a_hard_break_and_goes_after_it() {
        assert_eq!(
            insert_marker("text  \nnext\n", 1, "k3f9a2").unwrap(),
            "text   ^k3f9a2\nnext\n"
        );
        assert_eq!(
            insert_marker("text  \r\nnext\r\n", 1, "k3f9a2").unwrap(),
            "text   ^k3f9a2\r\nnext\r\n"
        );
    }

    #[test]
    fn insert_marker_keeps_a_bom_and_every_other_line() {
        let content = "\u{FEFF}a\n\n\nb\t\n";
        assert_eq!(
            insert_marker(content, 4, "k3f9a2").unwrap(),
            "\u{FEFF}a\n\n\nb\t ^k3f9a2\n"
        );
    }

    #[test]
    fn insert_marker_is_one_insertion_and_nothing_else() {
        let content = "one\r\ntwo  \nthree\n\nfour";
        for line in [1, 2, 3, 5] {
            let out = insert_marker(content, line, "abc123").unwrap();
            assert_eq!(out.len(), content.len() + " ^abc123".len());
            let at = out.find(" ^abc123").unwrap();
            assert_eq!(format!("{}{}", &out[..at], &out[at + 8..]), content);
        }
    }

    #[test]
    fn insert_marker_refuses_a_line_that_does_not_exist() {
        assert_eq!(insert_marker("a\nb\n", 3, "k3f9a2"), None);
        assert_eq!(insert_marker("a\nb\n", 0, "k3f9a2"), None);
        assert_eq!(insert_marker("", 1, "k3f9a2"), None);
    }

    // -- new_id -------------------------------------------------

    fn is_new_id_shape(id: &str) -> bool {
        id.len() == ID_LEN
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }

    #[test]
    fn new_id_is_six_lowercase_alphanumerics_over_10k_draws() {
        let mut rng = StdRng::seed_from_u64(7);
        let none = HashSet::new();
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = new_id(&none, &mut rng).unwrap();
            assert!(is_new_id_shape(&id), "bad id {id:?}");
            seen.insert(id);
        }
        // 10k draws from 36^6 should be (all but) all distinct.
        assert!(seen.len() > 9_990, "only {} distinct", seen.len());
    }

    #[test]
    fn new_id_uses_the_whole_alphabet() {
        let mut rng = StdRng::seed_from_u64(1);
        let none = HashSet::new();
        let mut chars = HashSet::new();
        for _ in 0..2_000 {
            chars.extend(new_id(&none, &mut rng).unwrap().chars());
        }
        assert_eq!(chars.len(), 36);
    }

    /// Seeded so that the first draw is known: it is put in `existing`, so
    /// the result must be a later draw, never the colliding one.
    #[test]
    fn new_id_redraws_when_the_first_draw_collides() {
        let none = HashSet::new();
        let first = new_id(&none, &mut StdRng::seed_from_u64(42)).unwrap();
        let existing = HashSet::from([first.clone()]);
        let got = new_id(&existing, &mut StdRng::seed_from_u64(42)).unwrap();
        assert_ne!(got, first);
        assert!(is_new_id_shape(&got));
        // It is exactly the second draw of that seed.
        let mut rng = StdRng::seed_from_u64(42);
        let _ = new_id(&none, &mut rng);
        assert_eq!(got, new_id(&none, &mut rng).unwrap());
    }

    #[test]
    fn new_id_gives_up_only_if_every_draw_collides() {
        // A random source that always draws the same ID.
        struct Constant;
        impl RngCore for Constant {
            fn next_u32(&mut self) -> u32 {
                0
            }
            fn next_u64(&mut self) -> u64 {
                0
            }
            fn fill_bytes(&mut self, dest: &mut [u8]) {
                dest.fill(0);
            }
            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
                dest.fill(0);
                Ok(())
            }
        }
        let first = new_id(&HashSet::new(), &mut Constant).unwrap();
        assert_eq!(new_id(&HashSet::from([first]), &mut Constant), None);
    }

    #[test]
    fn ids_in_file_covers_every_occurrence_case_insensitively() {
        let content = "one ^Summary\n\ntwo ^ab12cd\n\nthree ^ab12cd\n\nfour\nfive ^mid\nsix\n";
        let parsed = tephra_parser::parse_unit(Path::new("n.md"), content);
        let ids = ids_in_file(&parsed);
        assert!(ids.contains("summary"));
        assert!(ids.contains("ab12cd"));
        assert!(
            ids.contains("mid"),
            "a bare mid-paragraph anchor counts too"
        );
        assert_eq!(ids.len(), 3);
    }

    // -- link_for -----------------------------------------------

    #[test]
    fn link_for_drops_md_and_keeps_every_other_extension() {
        assert_eq!(
            link_for("notes", "Tephra test.md", "^ab12cd"),
            "[[@notes:Tephra test#^ab12cd]]"
        );
        for (path, want) in [
            ("log.txt", "[[@notes:log.txt#^ab12cd]]"),
            ("a.org", "[[@notes:a.org#^ab12cd]]"),
            ("a.rst", "[[@notes:a.rst#^ab12cd]]"),
            ("a.adoc", "[[@notes:a.adoc#^ab12cd]]"),
            ("a.markdown", "[[@notes:a.markdown#^ab12cd]]"),
            ("a.md.txt", "[[@notes:a.md.txt#^ab12cd]]"),
            ("md", "[[@notes:md#^ab12cd]]"),
        ] {
            assert_eq!(link_for("notes", path, "^ab12cd"), want);
        }
    }

    #[test]
    fn link_for_keeps_folders_spaces_and_unicode() {
        assert_eq!(
            link_for("notes", "Projects/Café Notes/Réunion 🎉.md", "^ab12cd"),
            "[[@notes:Projects/Café Notes/Réunion 🎉#^ab12cd]]"
        );
        assert_eq!(
            link_for("notes", "a/b.md", "Some Heading"),
            "[[@notes:a/b#Some Heading]]"
        );
    }

    #[test]
    fn unlinkable_char_finds_what_a_wikilink_cannot_hold() {
        assert_eq!(unlinkable_char("C#/notes.md"), Some('#'));
        assert_eq!(unlinkable_char("a|b.md"), Some('|'));
        assert_eq!(unlinkable_char("a[1].md"), Some('['));
        assert_eq!(unlinkable_char("Café Notes/Réunion.md"), None);
    }

    #[test]
    fn heading_anchor_cleans_what_wikilinks_give_meaning_to() {
        assert_eq!(heading_anchor("Plain"), Some("Plain".into()));
        assert_eq!(heading_anchor("A  B"), Some("A B".into()));
        assert_eq!(heading_anchor("C# | notes: [x]^"), Some("C notes x".into()));
        assert_eq!(heading_anchor("# | ^"), None);
    }

    // -- plan_ensure --------------------------------------------

    fn plan(path: &str, content: &str, line: usize) -> Result<Plan, BlockIdError> {
        plan_ensure(
            Path::new(path),
            content,
            line,
            &mut StdRng::seed_from_u64(9),
        )
    }

    #[test]
    fn plan_ensure_returns_an_existing_id_untouched_including_typed_ones() {
        for id in ["summary", "my-note-1", "AB12CD", "x"] {
            let content = format!("para one\nlast line ^{id}\n");
            assert_eq!(
                plan("n.md", &content, 1).unwrap(),
                Plan::Existing { id: id.into() }
            );
            assert_eq!(
                plan("n.md", &content, 2).unwrap(),
                Plan::Existing { id: id.into() }
            );
        }
    }

    #[test]
    fn plan_ensure_inserts_on_the_last_line_from_any_line_of_the_paragraph() {
        let content = "intro\nmiddle\nend\n\nother\n";
        let mut got = Vec::new();
        for line in 1..=3 {
            match plan("n.md", content, line).unwrap() {
                Plan::Insert { id, new_content } => {
                    assert_eq!(new_content, format!("intro\nmiddle\nend ^{id}\n\nother\n"));
                    got.push(id);
                }
                other => panic!("{other:?}"),
            }
        }
        // Same seed, same draw: the ID is a pure function of the RNG.
        assert!(got.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn plan_ensure_gives_each_list_item_its_own_id_on_its_own_line() {
        let content = "- a\n- b\n";
        let Plan::Insert { new_content, id } = plan("n.md", content, 2).unwrap() else {
            panic!()
        };
        assert_eq!(new_content, format!("- a\n- b ^{id}\n"));
    }

    #[test]
    fn plan_ensure_links_a_heading_and_never_inserts() {
        assert_eq!(
            plan("n.md", "## My Heading\n\ntext\n", 1).unwrap(),
            Plan::Heading {
                anchor: "My Heading".into()
            }
        );
        assert_eq!(
            plan("n.org", "* Org Head\ntext\n", 1).unwrap(),
            Plan::Heading {
                anchor: "Org Head".into()
            }
        );
        assert_eq!(
            plan("n.rst", "Rst Title\n=========\n\ntext\n", 2).unwrap(),
            Plan::Heading {
                anchor: "Rst Title".into()
            }
        );
        assert_eq!(
            plan("n.adoc", "== Adoc Title\n\ntext\n", 1).unwrap(),
            Plan::Heading {
                anchor: "Adoc Title".into()
            }
        );
    }

    #[test]
    fn plan_ensure_never_draws_an_id_the_file_already_uses() {
        // Whatever the seed draws first is already in the file.
        let first = new_id(&HashSet::new(), &mut StdRng::seed_from_u64(9)).unwrap();
        let content = format!("taken ^{first}\n\nfree\n");
        let Plan::Insert { id, .. } = plan("n.md", &content, 3).unwrap() else {
            panic!()
        };
        assert_ne!(id, first);
    }

    #[test]
    fn plan_ensure_on_a_file_with_duplicate_ids_returns_each_occurrence_and_adds_none() {
        let content = "first ^dup\n\nsecond ^dup\n\nthird\n";
        for line in [1, 3] {
            assert_eq!(
                plan("n.md", content, line).unwrap(),
                Plan::Existing { id: "dup".into() }
            );
        }
        let Plan::Insert { id, .. } = plan("n.md", content, 5).unwrap() else {
            panic!()
        };
        assert_ne!(id, "dup");
    }

    #[test]
    fn plan_ensure_refusals_carry_their_reason() {
        let md = "---\nk: v\n---\n\n```\ncode\n```\n\n| a |\n|---|\n| 1 |\n";
        for (line, word) in [
            (1, "frontmatter"),
            (4, "blank"),
            (6, "code"),
            (11, "table"),
            (0, "line 0"),
            (99, "out of range"),
        ] {
            let Err(BlockIdError::Refused(msg)) = plan("n.md", md, line) else {
                panic!("line {line}")
            };
            assert!(msg.contains(word), "line {line}: {msg}");
        }
    }

    #[test]
    fn safe_join_refuses_anything_but_a_plain_relative_path() {
        let root = Path::new("/r");
        assert_eq!(safe_join(root, "a/b.md").unwrap(), Path::new("/r/a/b.md"));
        for bad in ["../x.md", "/etc/passwd", "a/../../x", "./a.md"] {
            assert!(safe_join(root, bad).is_err(), "{bad}");
        }
    }
}
