//! `unit.search` inside one file (G9): `scope: "headings"` (the `#`
//! search) and `scope: "blocks"` (the `^` search), both with a required
//! `in: "@source:rel/path"`.
//!
//! What a file offers comes from the cache
//! ([`tephra_cache::Cache::in_file_candidates`]): headings, the line
//! ranges of paragraphs, symbols, and the page/slide/index of document
//! paragraphs. The cache stores no paragraph text, so a paragraph's text
//! is read back from the file here, once per search, and only when
//! that is safe (see [`read_fresh`]); a heading's or symbol's name is
//! cached in full and needs no read.
//!
//! Matching is [`super::fuzzy::Query::rank`], the same function the name
//! search calls, applied to the item's text with the name, stem and
//! title all set to that text. Results are ordered by score, then the
//! shorter text, then the position in the file, then (a document only)
//! the index within the page. A query that is empty or only whitespace
//! matches everything, in file order.
//!
//! Like every search this only reads: the cache through a read-only
//! connection, the file through a plain read that follows no symlink.
//! The scope is never widened: an `in` that names no unit is an error
//! naming it, never a search across sources (F7).

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use serde::Serialize;
use tephra_cache::{Cache, InFileCandidate, InFileScope, SourceStatus, UnitRow};

use super::{fuzzy, source_error, SearchError, MAX_LIMIT};
use crate::file_kind::{self, MAX_INDEXED_BYTES};
use crate::registry::{probe_path, PathProbe};

/// One result. Which fields are set depends on `kind`:
///
/// | `kind`               | fields besides `unit`, `kind`                    |
/// |----------------------|--------------------------------------------------|
/// | `heading`            | `text`, `level`, and `line` (or `page`/`slide`/`paragraph` for a document) |
/// | `paragraph`          | `text`, `line`, `block_id` when it has an ID    |
/// | `symbol`             | `name`, `symbol_kind`, `line`, `block_id` when an ID belongs to it |
/// | `document paragraph` | `text`, `page`/`slide`/`paragraph`, `index`      |
///
/// A `block_id` is only ever one that is already in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InFileHitWire {
    /// The id searched in, `@source:rel_path`.
    pub unit: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A symbol's kind: `fn`, `struct`, `enum`, `method`, `const`, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    /// 1-based. Not set for a document, which has a page, slide or
    /// paragraph number instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slide: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraph: Option<i64>,
    /// A document paragraph's 1-based position within its page or slide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
}

/// The `unit.search` result for the `headings` and `blocks` scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InFileSearchWire {
    /// The id searched in, as given.
    pub unit: String,
    /// `headings` or `blocks`.
    pub scope: &'static str,
    /// The unit's source is offline (its stored status says so, or its
    /// root is missing). The results come from the cache.
    pub offline: bool,
    /// Paragraph text could not be read back from the file (the source
    /// is offline, or the file changed or went missing since it was
    /// indexed). Paragraphs are then listed with no text and only an
    /// empty query lists them. Always `false` for `headings` and for
    /// code files.
    pub text_unavailable: bool,
    pub results: Vec<InFileHitWire>,
}

/// Checks a query before anything reads the cache. Unlike a name search,
/// an empty or whitespace-only query is allowed (`None`: match
/// everything); anything else must be at most
/// [`fuzzy::MAX_QUERY_CHARS`] characters.
pub fn parse_in_file_query(raw: &str) -> Result<Option<fuzzy::Query>, String> {
    match fuzzy::Query::new(raw) {
        None => Ok(None),
        Some(query) if query.len() > fuzzy::MAX_QUERY_CHARS => Err(format!(
            "\"query\" is longer than {} characters",
            fuzzy::MAX_QUERY_CHARS
        )),
        Some(query) => Ok(Some(query)),
    }
}

/// Splits `@source:rel_path` into its two non-empty parts.
pub fn split_unit_id(id: &str) -> Option<(&str, &str)> {
    let (source, rel_path) = id.strip_prefix('@')?.split_once(':')?;
    if source.is_empty() || rel_path.is_empty() {
        return None;
    }
    Some((source, rel_path))
}

/// One candidate with what its order and its response entry need.
struct Item {
    /// What the query is matched against; empty when unavailable.
    text: String,
    /// Line, page, slide or paragraph number.
    position: i64,
    /// Index within a page (document paragraphs), else 0.
    sub: i64,
    wire: InFileHitWire,
}

/// Searches one unit; see the module doc comment. `cache_path` is the
/// daemon's `cache.db`, opened read-only for the call.
pub fn search_in_file(
    cache_path: &Path,
    unit_id: &str,
    scope: InFileScope,
    query: &str,
    limit: usize,
) -> Result<InFileSearchWire, SearchError> {
    let query = parse_in_file_query(query).map_err(SearchError::InvalidParams)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(SearchError::InvalidParams(format!(
            "\"limit\" must be an integer from 1 to {MAX_LIMIT}"
        )));
    }
    let (source_name, rel_path) = split_unit_id(unit_id).ok_or_else(|| {
        SearchError::InvalidParams(format!(
            "\"in\" must be a unit id like \"@source:path/to/file\", got \"{unit_id}\""
        ))
    })?;

    let cache = Cache::open_read_only(cache_path).map_err(source_error)?;
    let no_such_unit = || SearchError::Source(format!("no such unit: {unit_id}"));
    let source = cache
        .get_source(source_name)
        .map_err(source_error)?
        .ok_or_else(no_such_unit)?;
    // A folder has no row, so it lands here too: `in` names a file.
    let unit = cache
        .get_unit_by_rel_path(source.id, rel_path)
        .map_err(source_error)?
        .ok_or_else(no_such_unit)?;

    let root = Path::new(&source.path);
    let probe = probe_path(root);
    if probe.is_permission_denied() {
        return Err(SearchError::Source(format!(
            "source {source_name} can't be read (permission denied)"
        )));
    }
    let offline = source.status == SourceStatus::Offline || probe == PathProbe::Missing;
    let online = !offline && probe == PathProbe::Ok;

    let candidates = cache
        .in_file_candidates(&unit, scope)
        .map_err(source_error)?;

    let (texts, text_unavailable) =
        if scope == InFileScope::Blocks && candidates.iter().any(needs_text) {
            match online
                .then(|| load_texts(root, &unit, &candidates))
                .flatten()
            {
                Some(texts) => (texts, false),
                None => (Texts::None, true),
            }
        } else {
            (Texts::None, false)
        };

    let mut items: Vec<Item> = candidates
        .into_iter()
        .map(|c| item(unit_id, &unit, c, &texts))
        .collect();

    match &query {
        // Everything, in file order (the cache's order, kept by the
        // stable sort).
        None => items.sort_by_key(|i| (i.position, i.sub)),
        Some(query) => {
            let mut scored: Vec<(f64, Item)> = items
                .into_iter()
                .filter_map(|i| {
                    // The name, stem and title are all the item's own
                    // text, and there is no path to fall back to.
                    query
                        .rank(&i.text, &i.text, "", "")
                        .map(|rank| (rank.score, i))
                })
                .collect();
            scored.sort_by(|(sa, a), (sb, b)| {
                sb.total_cmp(sa)
                    .then_with(|| a.text.chars().count().cmp(&b.text.chars().count()))
                    .then_with(|| a.position.cmp(&b.position))
                    .then_with(|| a.sub.cmp(&b.sub))
            });
            items = scored.into_iter().map(|(_, i)| i).collect();
        }
    }
    items.truncate(limit);

    Ok(InFileSearchWire {
        unit: unit_id.to_string(),
        scope: scope.as_str(),
        offline,
        text_unavailable,
        results: items.into_iter().map(|i| i.wire).collect(),
    })
}

fn needs_text(c: &InFileCandidate) -> bool {
    matches!(
        c,
        InFileCandidate::Paragraph { .. } | InFileCandidate::DocumentParagraph { .. }
    )
}

fn blank_wire(unit_id: &str, kind: &'static str) -> InFileHitWire {
    InFileHitWire {
        unit: unit_id.to_string(),
        kind,
        text: None,
        name: None,
        symbol_kind: None,
        level: None,
        line: None,
        page: None,
        slide: None,
        paragraph: None,
        index: None,
        block_id: None,
    }
}

/// Sets whichever of `page`, `slide` or `paragraph` the unit's file
/// extension says the number is.
fn set_location(wire: &mut InFileHitWire, unit: &UnitRow, number: i64) {
    let ext = Path::new(&unit.rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("pdf") => wire.page = Some(number),
        Some("pptx") => wire.slide = Some(number),
        _ => wire.paragraph = Some(number),
    }
}

fn item(unit_id: &str, unit: &UnitRow, candidate: InFileCandidate, texts: &Texts) -> Item {
    match candidate {
        InFileCandidate::Heading { level, text, line } => {
            let mut wire = blank_wire(unit_id, "heading");
            wire.text = Some(text.clone());
            wire.level = Some(level);
            wire.line = Some(line);
            Item {
                text,
                position: line,
                sub: 0,
                wire,
            }
        }
        InFileCandidate::DocumentHeading {
            level,
            text,
            number,
        } => {
            let mut wire = blank_wire(unit_id, "heading");
            wire.text = Some(text.clone());
            wire.level = Some(level);
            set_location(&mut wire, unit, number);
            Item {
                text,
                position: number,
                sub: 0,
                wire,
            }
        }
        InFileCandidate::Paragraph {
            line,
            end_line,
            block_id,
        } => {
            // The query is matched against the text as written, marker
            // included (so `^dup` finds the paragraphs ending `^dup`),
            // but the response carries the marker only as `block_id`.
            let text = texts.prose(line, end_line).unwrap_or_default();
            let mut wire = blank_wire(unit_id, "paragraph");
            wire.text = Some(strip_block_marker(&text, block_id.as_deref()).to_string());
            wire.line = Some(line);
            wire.block_id = block_id;
            Item {
                text,
                position: line,
                sub: 0,
                wire,
            }
        }
        InFileCandidate::Symbol {
            symbol_kind,
            name,
            line,
            block_id,
            ..
        } => {
            let mut wire = blank_wire(unit_id, "symbol");
            wire.name = Some(name.clone());
            wire.symbol_kind = Some(symbol_kind);
            wire.line = Some(line);
            wire.block_id = block_id;
            Item {
                text: name,
                position: line,
                sub: 0,
                wire,
            }
        }
        InFileCandidate::DocumentParagraph { number, index } => {
            let text = texts.document(number, index).unwrap_or_default();
            let mut wire = blank_wire(unit_id, "document paragraph");
            wire.text = Some(text.clone());
            set_location(&mut wire, unit, number);
            wire.index = Some(index);
            Item {
                text,
                position: number,
                sub: index,
                wire,
            }
        }
    }
}

/// `text` without the ` ^<block_id>` that ends it, and the whitespace
/// before it; unchanged when `block_id` is `None` or does not end the
/// text. A paragraph that is only its marker becomes empty.
fn strip_block_marker<'a>(text: &'a str, block_id: Option<&str>) -> &'a str {
    let Some(id) = block_id else {
        return text;
    };
    let trimmed = text.trim_end();
    match trimmed
        .strip_suffix(id)
        .and_then(|rest| rest.strip_suffix('^'))
    {
        Some(rest) if rest.is_empty() || rest.ends_with(char::is_whitespace) => rest.trim_end(),
        _ => text,
    }
}

/// Paragraph text read back from the file.
enum Texts {
    None,
    /// The file's lines, for a prose unit.
    Prose(Vec<String>),
    /// A document's paragraphs by `(page/slide/paragraph, index)`.
    Document(HashMap<(i64, i64), String>),
}

impl Texts {
    fn prose(&self, line: i64, end_line: i64) -> Option<String> {
        let Texts::Prose(lines) = self else {
            return None;
        };
        let first = usize::try_from(line).ok()?.checked_sub(1)?;
        let last = usize::try_from(end_line.max(line)).ok()?.min(lines.len());
        lines.get(first..last).map(|l| l.join("\n"))
    }

    fn document(&self, number: i64, index: i64) -> Option<String> {
        let Texts::Document(paragraphs) = self else {
            return None;
        };
        paragraphs.get(&(number, index)).cloned()
    }
}

/// Reads the text the candidates' paragraphs need, or `None` when the
/// file can't be trusted to match the cached rows: it is missing, not a
/// plain file, reached through a symlink, over the size cap, changed
/// since it was indexed, no longer parses, or no longer holds a line
/// or paragraph the cache lists.
fn load_texts(root: &Path, unit: &UnitRow, candidates: &[InFileCandidate]) -> Option<Texts> {
    let bytes = read_fresh(root, unit)?;
    match unit.kind.as_str() {
        "prose" => {
            let text = match file_kind::decode_utf16_with_bom(&bytes) {
                Some(text) => text,
                None => String::from_utf8(bytes).ok()?,
            };
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            let fits = candidates.iter().all(|c| match c {
                InFileCandidate::Paragraph { line, end_line, .. } => {
                    *line >= 1
                        && usize::try_from((*end_line).max(*line)).is_ok_and(|n| n <= lines.len())
                }
                _ => true,
            });
            fits.then_some(Texts::Prose(lines))
        }
        "document" => {
            let doc =
                tephra_parser::parse_document_bytes(Path::new(&unit.rel_path), &bytes).ok()?;
            let map: HashMap<(i64, i64), String> = doc
                .paragraphs
                .into_iter()
                .map(|p| ((p.location.number() as i64, p.index as i64), p.text))
                .collect();
            let fits = candidates.iter().all(|c| match c {
                InFileCandidate::DocumentParagraph { number, index } => {
                    map.contains_key(&(*number, *index))
                }
                _ => true,
            });
            fits.then_some(Texts::Document(map))
        }
        _ => None,
    }
}

/// The bytes of `unit`'s file, if and only if the file is the one that
/// was indexed. Nothing is followed through a symlink (the file or any
/// directory above it below the root), only a regular file of at most
/// [`MAX_INDEXED_BYTES`] is read, and its size and modification time
/// must equal the cached ones. `rel_path` comes from the cache but the
/// request chose which row, so it is checked as untrusted anyway: only
/// plain components, never `..` or an absolute path.
fn read_fresh(root: &Path, unit: &UnitRow) -> Option<Vec<u8>> {
    if unit.too_large || unit.needs_rescan {
        return None;
    }
    let rel = Path::new(&unit.rel_path);
    let mut current = root.to_path_buf();
    let mut components = rel.components().peekable();
    components.peek()?;
    for component in components {
        let Component::Normal(part) = component else {
            return None;
        };
        current.push(part);
        if fs::symlink_metadata(&current)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let metadata = fs::symlink_metadata(&current).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEXED_BYTES {
        return None;
    }
    if let Some(size) = unit.size {
        if i64::try_from(metadata.len()).ok()? != size {
            return None;
        }
    }
    if let Some(mtime) = unit.mtime {
        let modified = metadata.modified().ok()?;
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        if i64::try_from(secs).ok()? != mtime {
            return None;
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(&current)
        .ok()?
        .take(MAX_INDEXED_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    // Grew (or shrank) between the stat and the read.
    (bytes.len() as u64 == metadata.len()).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::strip_block_marker;

    #[test]
    fn the_trailing_marker_and_the_space_before_it_are_dropped() {
        assert_eq!(strip_block_marker("Text. ^abc", Some("abc")), "Text.");
        assert_eq!(strip_block_marker("a\nb  ^abc  ", Some("abc")), "a\nb");
        assert_eq!(strip_block_marker("^abc", Some("abc")), "");
    }

    #[test]
    fn text_without_that_exact_marker_is_left_alone() {
        assert_eq!(strip_block_marker("Text. ^abc", None), "Text. ^abc");
        assert_eq!(strip_block_marker("Text. ^abc", Some("ab")), "Text. ^abc");
        assert_eq!(strip_block_marker("Text. ^abc", Some("xabc")), "Text. ^abc");
        assert_eq!(strip_block_marker("Text^abc", Some("abc")), "Text^abc");
    }
}
