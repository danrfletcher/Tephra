//! Duplicate block IDs inside one file (G19, E7).
//!
//! A scan never writes. The one exception is this: an edit (a paragraph
//! copied, a Rust fn pasted with its `// ^id` comment) can make an ID
//! the file already had in the cache appear a second time. Then the new
//! copy is renamed and the original keeps its ID, so links stay on it.
//!
//! What is renamed, and what is not:
//!
//! - An ID the cache held for this file **exactly once** and that the
//!   file now holds twice or more is renamed (every occurrence but the
//!   original).
//! - An ID the cache did not hold for this file (a block pasted twice
//!   from another file), or held twice already (duplicates the file had
//!   when it was first scanned, E7), is never written. It is logged, and
//!   resolving it gives the ambiguity error that lists every line.
//! - Only prose and code files are written, and only when valid UTF-8.
//!   A `^id` in a fence, a code span, frontmatter or a string literal is
//!   not an occurrence at all (the parser never reports it), so it is
//!   never renamed. The same ID in two *files* is never touched.
//!
//! The original is the occurrence whose indexed text equals the cached
//! block's text; then the one on the cached line; if none is on that
//! line, the nearest one below it (a copy pasted above the original
//! pushes the original down, never up); then the first. A typed
//! ID (`^summary`) gets a numeric suffix (`^summary-1`, then `-2`, ...
//! skipping any suffix the file already uses). A Tephra-format ID (6
//! characters of `[a-z0-9]`) gets a fresh 6-character ID that is unique
//! in the file.
//!
//! Only the ID token changes: the rest of the line and the file keep
//! their bytes. The new content is parsed again and must hold exactly
//! the renamed IDs where the old ones were, or nothing is written.
//!
//! The write is the block-ID writer's: temp file, fsync, rename
//! ([`rewrite_links_atomic_with`]), after a look at the file's size and
//! mtime ([`AtomicFs::fingerprint`]) that must be the same as when the
//! file was read; if not, nothing is written and the file is tried
//! again shortly ([`Fixed::Deferred`]). This module is the planner
//! (pure, table-tested below) and the writer; `scan` calls [`fix_file`] after a changed unit is
//! parsed and before its rows replace the cached ones, and indexes the
//! file again from disk after a rename, so the cache holds the new ID
//! straight away.
//!
//! Tephra's own write must not start another batch: [`note_self_write`]
//! records the file's size and mtime after the rename, and the
//! registry's watcher drops that path from the next batch with
//! [`drop_self_writes`] when the file is still exactly as written.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use rand::Rng;
use tephra_cache::IdBlockText;
use tephra_parser::{DiagnosticKind, IndexRow, ParsedUnit};

use crate::block_id::{ids_in_file, line_spans, new_id, ID_LEN};
use crate::file_lock;
use crate::refactor::{is_stale_temp_file_name, rewrite_links_atomic_with, AtomicFs};

// ---------------------------------------------------------------
// The pure planner
// ---------------------------------------------------------------

/// One block ID occurrence in the file as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub id: String,
    /// The block's line (a code symbol's first line), as the `blocks`
    /// table stores it.
    pub line: usize,
    /// The line the `^id` token itself is on.
    pub token_line: usize,
    /// The text the index holds for the block (the paragraph without its
    /// `^id`, a symbol's name), when it has a row.
    pub text: Option<String>,
}

/// One block ID the cache held for the file before the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedId {
    pub id: String,
    pub line: i64,
    pub text: Option<String>,
}

impl From<&IdBlockText> for CachedId {
    fn from(b: &IdBlockText) -> Self {
        CachedId {
            id: b.block_id.clone(),
            line: b.line,
            text: b.text.clone(),
        }
    }
}

/// One token to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    /// The block's line (what a link to the new ID resolves to).
    pub line: usize,
    /// The line the token is on.
    pub token_line: usize,
    pub old: String,
    pub new: String,
}

/// A repeated ID that is left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    /// The cache did not hold the ID for this file (a block pasted twice
    /// from elsewhere, or a file first scanned with it).
    NotCached { id: String, lines: Vec<usize> },
    /// The cache already held it more than once: duplicates the file had
    /// when it was first scanned (E7).
    AlreadyDuplicated { id: String, lines: Vec<usize> },
    /// No unused ID could be made.
    NoFreeId { id: String, lines: Vec<usize> },
}

/// The planner's answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub renames: Vec<Rename>,
    pub skipped: Vec<Skipped>,
}

/// Whether `id` is in the format Tephra's own IDs have: 6 characters of
/// `[a-z0-9]`.
pub fn is_tephra_format(id: &str) -> bool {
    id.len() == ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Which of `group` (all with the same ID, in file order) is the
/// original of `cached`: the text match, then the cached line, then the
/// first. Each step only narrows the candidates when it matches at least
/// one of them. When no candidate is on the cached line, the nearest one
/// below it (a later line) stands for the cached line before the first
/// does: a copy pasted above the original pushes the original down,
/// never up, and the copy is the one that must give up its ID.
fn pick_original(group: &[&Occurrence], cached: &CachedId) -> usize {
    let mut candidates: Vec<usize> = (0..group.len()).collect();
    if let Some(text) = &cached.text {
        let by_text: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| group[i].text.as_ref() == Some(text))
            .collect();
        if !by_text.is_empty() {
            candidates = by_text;
        }
    }
    let line_of = |i: usize| i64::try_from(group[i].line).unwrap_or(i64::MAX);
    if let Some(&on_line) = candidates.iter().find(|&&i| line_of(i) == cached.line) {
        return on_line;
    }
    candidates
        .iter()
        .copied()
        .filter(|&i| line_of(i) > cached.line)
        .min_by_key(|&i| line_of(i))
        .unwrap_or(candidates[0])
}

/// Decides which tokens to rename. `occurrences` are the file's block
/// IDs in file order, `cached` what the cache held for the file before
/// the edit, `taken` the IDs the file uses (lower-cased: Obsidian reads
/// IDs without regard to case, so `^AB12CD` and `^ab12cd` clash). Pure:
/// `rng` is injected.
pub fn plan<R: Rng + ?Sized>(
    occurrences: &[Occurrence],
    cached: &[CachedId],
    taken: &HashSet<String>,
    rng: &mut R,
) -> Plan {
    let mut taken = taken.clone();
    let mut order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<&Occurrence>> = HashMap::new();
    for o in occurrences {
        let group = groups.entry(o.id.as_str()).or_default();
        if group.is_empty() {
            order.push(o.id.as_str());
        }
        group.push(o);
    }

    let mut out = Plan::default();
    for id in order {
        let group = &groups[id];
        if group.len() < 2 {
            continue;
        }
        let lines: Vec<usize> = group.iter().map(|o| o.token_line).collect();
        let was: Vec<&CachedId> = cached.iter().filter(|c| c.id == id).collect();
        match was.len() {
            0 => {
                out.skipped.push(Skipped::NotCached {
                    id: id.to_string(),
                    lines,
                });
                continue;
            }
            1 => {}
            _ => {
                out.skipped.push(Skipped::AlreadyDuplicated {
                    id: id.to_string(),
                    lines,
                });
                continue;
            }
        }
        let keep = pick_original(group, was[0]);
        let mut renames = Vec::new();
        let mut failed = false;
        for (i, occurrence) in group.iter().enumerate() {
            if i == keep {
                continue;
            }
            let fresh = if is_tephra_format(id) {
                new_id(&taken, rng)
            } else {
                (1u32..=u32::MAX)
                    .map(|n| format!("{id}-{n}"))
                    .find(|candidate| !taken.contains(&candidate.to_ascii_lowercase()))
            };
            let Some(fresh) = fresh else {
                failed = true;
                break;
            };
            taken.insert(fresh.to_ascii_lowercase());
            renames.push(Rename {
                line: occurrence.line,
                token_line: occurrence.token_line,
                old: id.to_string(),
                new: fresh,
            });
        }
        if failed {
            out.skipped.push(Skipped::NoFreeId {
                id: id.to_string(),
                lines,
            });
        } else {
            out.renames.extend(renames);
        }
    }
    out
}

/// The byte offset of the `^` of the token `^{id}` that ends `line`: the
/// last one, followed only by what may follow a block ID in a comment (a
/// block comment's closer) and preceded by whitespace or a comment
/// marker. `None` if there is none.
fn find_token(line: &str, id: &str) -> Option<usize> {
    line.match_indices('^').rev().find_map(|(at, _)| {
        let after = line[at + 1..].strip_prefix(id)?;
        let closer_only = after
            .trim()
            .chars()
            .all(|c| matches!(c, '*' | '/' | ']' | '='));
        let before = &line[..at];
        let standalone = before
            .chars()
            .last()
            .is_none_or(|c| c.is_whitespace() || "/*#-!;<>=[]".contains(c));
        (closer_only && standalone).then_some(at)
    })
}

/// `content` with each rename's token changed and nothing else: `None`
/// when a token cannot be found on its line.
pub fn rewrite_tokens(content: &str, renames: &[Rename]) -> Option<String> {
    let spans = line_spans(content);
    // (offset of the id after `^`, its length, the new id)
    let mut edits: Vec<(usize, usize, &str)> = Vec::with_capacity(renames.len());
    for r in renames {
        let (start, end) = *spans.get(r.token_line.checked_sub(1)?)?;
        let line = &content[start..end];
        let caret = find_token(line, &r.old)?;
        edits.push((start + caret + 1, r.old.len(), r.new.as_str()));
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    if edits.windows(2).any(|w| w[1].0 + w[1].1 > w[0].0) {
        return None;
    }
    let mut out = content.to_string();
    for (at, len, new) in edits {
        out.replace_range(at..at + len, new);
    }
    Some(out)
}

/// The block IDs of a parsed file, each with the text the index holds for
/// it. `rows` are the unit's index rows ([`ParsedUnit::index_rows`]).
pub fn occurrences(parsed: &ParsedUnit, rows: &[IndexRow]) -> Vec<Occurrence> {
    parsed
        .blocks
        .iter()
        .map(|a| {
            let paragraph_start = parsed
                .paragraphs
                .iter()
                .find(|p| p.end_line == a.line && p.block_id.as_deref() == Some(&a.block_id))
                .map(|p| p.start_line);
            let with_id = || {
                rows.iter()
                    .filter(|r| r.block_id.as_deref() == Some(a.block_id.as_str()))
            };
            let row = with_id()
                .find(|r| r.line == i64::try_from(a.line).ok())
                .or_else(|| {
                    paragraph_start
                        .and_then(|s| with_id().find(|r| r.line == i64::try_from(s).ok()))
                })
                .or_else(|| {
                    with_id()
                        .filter(|r| r.line.is_some_and(|l| l <= a.line as i64))
                        .max_by_key(|r| r.line)
                });
            Occurrence {
                id: a.block_id.clone(),
                line: a.line,
                token_line: a.token_line,
                text: row.map(|r| r.text.clone()),
            }
        })
        .collect()
}

/// The new content for `content` and the renames it holds, or `None`
/// when there is nothing to write (no plan, or a plan whose result does
/// not parse back as exactly the renamed IDs where the old ones were).
/// Everything left alone is in the second part. `parse` parses a text as
/// this file's unit.
pub fn fixed_content<R: Rng + ?Sized>(
    content: &str,
    parsed: &ParsedUnit,
    rows: &[IndexRow],
    cached: &[CachedId],
    parse: &dyn Fn(&str) -> ParsedUnit,
    rng: &mut R,
) -> (Option<(String, Vec<Rename>)>, Vec<Skipped>) {
    let found = occurrences(parsed, rows);
    let Plan { renames, skipped } = plan(&found, cached, &ids_in_file(parsed), rng);
    if renames.is_empty() {
        return (None, skipped);
    }
    let Some(new_content) = rewrite_tokens(content, &renames) else {
        tracing::warn!(
            "a duplicate block ID's token could not be found on its line; not rewriting"
        );
        return (None, skipped);
    };
    // The result must hold every new ID once, on the block line the old
    // one was, and no more or fewer IDs in all.
    let after = parse(&new_content);
    let mut count: HashMap<&str, usize> = HashMap::new();
    for a in &after.blocks {
        *count.entry(a.block_id.as_str()).or_default() += 1;
    }
    let renamed_from = |id: &str| renames.iter().filter(|r| r.old == id).count();
    let in_place = renames.iter().all(|r| {
        count.get(r.new.as_str()) == Some(&1)
            && after
                .blocks
                .iter()
                .any(|a| a.block_id == r.new && a.line == r.line)
    });
    let kept = found.iter().all(|o| {
        count.get(o.id.as_str()).copied().unwrap_or(0)
            == found.iter().filter(|f| f.id == o.id).count() - renamed_from(&o.id)
    });
    if after.blocks.len() != parsed.blocks.len() || !in_place || !kept {
        tracing::warn!(
            "renaming a duplicate block ID would not read back as expected; not rewriting"
        );
        return (None, skipped);
    }
    (Some((new_content, renames)), skipped)
}

// ---------------------------------------------------------------
// The writer
// ---------------------------------------------------------------

/// What [`fix_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fixed {
    /// Nothing to rename, or nothing could be written; the file is as it
    /// was.
    Untouched,
    /// The file was rewritten with these renames.
    Renamed(Vec<Rename>),
    /// The renames were not written because the file is being changed
    /// by someone else (another writer holds it, or its size or mtime
    /// are not what they were when it was read). Nothing was written.
    /// The caller leaves the file's cached rows as they were (or, in a
    /// rescan that has cleared them, leaves the file out for now) and
    /// tries the file again a moment later with the same cached IDs
    /// ([`crate::scan::settle_deferred`]): a lock holder such as
    /// `block.ensure_id` finishes, an edit settles. No further file
    /// event can be counted on (a held lock raises none), so the retry
    /// is the caller's own. After its last try it indexes the file as
    /// it is and keeps the pre-edit IDs ([`remember_given_up`]), so the
    /// file's next re-parse can still rename the copy.
    Deferred,
}

/// Renames the duplicate IDs of the file at `abs` if the rule above
/// allows, one write for all of them. `content` is the text the file was
/// parsed from, `fingerprint` the file's size and mtime from just before
/// it was read, `cached` the cache's block IDs for the file from before
/// the edit, `unit` the unit's id for the log. A failure is logged and
/// returns [`Fixed::Untouched`]: the scan carries on with the duplicate
/// rows. [`Fixed::Deferred`] is only for a file in flux, see there.
#[allow(clippy::too_many_arguments)]
pub fn fix_file<R: Rng + ?Sized>(
    fs_ops: &dyn AtomicFs,
    abs: &Path,
    unit: &dyn Fn() -> String,
    content: &str,
    fingerprint: Option<(u64, Option<SystemTime>)>,
    parsed: &ParsedUnit,
    rows: &[IndexRow],
    cached: &[CachedId],
    parse: &dyn Fn(&str) -> ParsedUnit,
    rng: &mut R,
) -> Fixed {
    if !parsed
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::DuplicateBlockId)
    {
        return Fixed::Untouched;
    }
    let (fixed, skipped) = fixed_content(content, parsed, rows, cached, parse, rng);
    for s in &skipped {
        let (why, id, lines) = match s {
            Skipped::NotCached { id, lines } => (
                "the file's cache did not hold it once, so it is not rewritten",
                id,
                lines,
            ),
            Skipped::AlreadyDuplicated { id, lines } => (
                "the file already had it more than once, so it is not rewritten",
                id,
                lines,
            ),
            Skipped::NoFreeId { id, lines } => ("no unused ID could be made", id, lines),
        };
        tracing::warn!(
            unit = %unit(),
            id = %id,
            lines = ?lines,
            "duplicate block ID left as it is: {why}"
        );
    }
    let Some((new_content, renames)) = fixed else {
        return Fixed::Untouched;
    };

    let readonly = std::fs::symlink_metadata(abs)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true);
    if readonly {
        tracing::warn!(unit = %unit(), "duplicate block ID not renamed: the file is read-only");
        return Fixed::Untouched;
    }
    // One writer per file. Never wait: this runs inside a cache
    // transaction, and `block.ensure_id` holds this lock while it waits
    // for the cache.
    let Some(_guard) = file_lock::try_lock(abs) else {
        tracing::info!(unit = %unit(), "duplicate block ID not renamed: another writer has the file; it is tried again shortly");
        return Fixed::Deferred;
    };
    // Still the file we read?
    let now = fs_ops.fingerprint(abs).ok();
    if fingerprint.is_none() || now != fingerprint {
        tracing::info!(unit = %unit(), "duplicate block ID not renamed: the file changed since it was read; it is tried again shortly");
        return Fixed::Deferred;
    }
    if let Err(e) = rewrite_links_atomic_with(fs_ops, abs, new_content.as_bytes()) {
        tracing::warn!(unit = %unit(), error = %e, "duplicate block ID not renamed: could not write the file; it is as it was");
        return Fixed::Untouched;
    }
    note_self_write(abs);
    for r in &renames {
        tracing::info!(
            unit = %unit(),
            old_id = %r.old,
            new_id = %r.new,
            line = r.line,
            "renamed a duplicate block ID"
        );
    }
    Fixed::Renamed(renames)
}

// ---------------------------------------------------------------
// A file whose rename was given up on keeps its pre-edit IDs
// ---------------------------------------------------------------

/// A file's place in the daemon's memory of renames it gave up on: the
/// cache database, the source and the path within it.
type GivenUpKey = (PathBuf, i64, String);

fn given_up() -> &'static Mutex<HashMap<GivenUpKey, Vec<CachedId>>> {
    static MAP: OnceLock<Mutex<HashMap<GivenUpKey, Vec<CachedId>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remembers that a rename for the file could not be written even on the
/// caller's last try, so that the file was indexed with the duplicate in
/// it. `cached` are the block IDs the cache held for it before the edit.
/// The cache now holds the ID twice, which the planner reads as a
/// duplicate from the file's first scan (E7) and never renames; this
/// record lets the file's next re-parse plan against the pre-edit IDs
/// instead ([`given_up_ids`]). Kept in memory only: after a restart such
/// a file counts as one scanned with its duplicate.
pub fn remember_given_up(cache: &Path, source_id: i64, rel_path: &str, cached: &[CachedId]) {
    given_up().lock().unwrap_or_else(|p| p.into_inner()).insert(
        (cache.to_path_buf(), source_id, rel_path.to_string()),
        cached.to_vec(),
    );
}

/// The pre-edit block IDs [`remember_given_up`] kept for the file, if it
/// has any.
pub fn given_up_ids(cache: &Path, source_id: i64, rel_path: &str) -> Option<Vec<CachedId>> {
    given_up()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(cache.to_path_buf(), source_id, rel_path.to_string()))
        .cloned()
}

/// Drops the record for the file: it was renamed, has no duplicate any
/// more, or is gone.
pub fn forget_given_up(cache: &Path, source_id: i64, rel_path: &str) {
    given_up()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&(cache.to_path_buf(), source_id, rel_path.to_string()));
}

// ---------------------------------------------------------------
// Tephra's own write is not a change to react to
// ---------------------------------------------------------------

/// How long a record of Tephra's own write is kept for its filesystem
/// event to arrive.
const SELF_WRITE_TTL: Duration = Duration::from_secs(60);

type Stamp = (u64, Option<SystemTime>, Instant);

fn self_writes() -> &'static Mutex<HashMap<PathBuf, Stamp>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, Stamp>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records the size and mtime `path` has now, just after Tephra wrote it.
pub fn note_self_write(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let mut map = self_writes().lock().unwrap_or_else(|p| p.into_inner());
    map.retain(|_, (_, _, at)| at.elapsed() < SELF_WRITE_TTL);
    map.insert(
        path.to_path_buf(),
        (meta.len(), meta.modified().ok(), Instant::now()),
    );
}

/// Removes from `changed` every path that is exactly as Tephra last wrote
/// it (it is the event of that write), and the temp files of that write
/// beside them. A path changed again since is kept, and so is every
/// record's path that the batch does not hold. Returns how many paths
/// were dropped as Tephra's own.
pub fn drop_self_writes(changed: &mut HashSet<PathBuf>) -> usize {
    let mut map = self_writes().lock().unwrap_or_else(|p| p.into_inner());
    if map.is_empty() {
        return 0;
    }
    let mut dropped = 0;
    let paths: Vec<PathBuf> = changed.iter().cloned().collect();
    for path in paths {
        let Some(&(len, modified, at)) = map.get(&path) else {
            continue;
        };
        let same = std::fs::symlink_metadata(&path)
            .map(|m| m.len() == len && m.modified().ok() == modified)
            .unwrap_or(false);
        if same && at.elapsed() < SELF_WRITE_TTL {
            changed.remove(&path);
            map.remove(&path);
            dropped += 1;
        } else if !same {
            // Changed since: the record is of no use any more.
            map.remove(&path);
        }
    }
    if dropped > 0 {
        changed.retain(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_stale_temp_file_name)
        });
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn md(content: &str) -> ParsedUnit {
        tephra_parser::parse_unit(Path::new("note.md"), content)
    }

    fn rust(content: &str) -> ParsedUnit {
        tephra_parser::parse_code_unit(Path::new("lib.rs"), tephra_parser::Language::Rust, content)
    }

    fn code(content: &str, name: &str, lang: tephra_parser::Language) -> ParsedUnit {
        tephra_parser::parse_code_unit(Path::new(name), lang, content)
    }

    fn cached(id: &str, line: i64, text: Option<&str>) -> CachedId {
        CachedId {
            id: id.into(),
            line,
            text: text.map(Into::into),
        }
    }

    /// Runs the whole pure path on a prose file: its renames and the
    /// content they give.
    fn run_md(content: &str, was: &[CachedId]) -> (Option<(String, Vec<Rename>)>, Vec<Skipped>) {
        let parsed = md(content);
        let rows = parsed.index_rows(content);
        let mut rng = StdRng::seed_from_u64(7);
        fixed_content(content, &parsed, &rows, was, &|t| md(t), &mut rng)
    }

    fn renamed(r: (Option<(String, Vec<Rename>)>, Vec<Skipped>)) -> String {
        r.0.expect("a rename").0
    }

    #[test]
    fn tephra_format_is_six_lowercase_alphanumerics() {
        for (id, want) in [
            ("k3f9a2", true),
            ("abcdef", true),
            ("012345", true),
            ("summary", false),
            ("abcde", false),
            ("ABCDEF", false),
            ("ab-def", false),
            ("", false),
        ] {
            assert_eq!(is_tephra_format(id), want, "{id}");
        }
    }

    #[test]
    fn typed_id_gets_a_numeric_suffix() {
        let content = "para ^summary\n\npara ^summary\n";
        let out = renamed(run_md(content, &[cached("summary", 1, Some("para"))]));
        assert_eq!(out, "para ^summary\n\npara ^summary-1\n");
    }

    #[test]
    fn suffixes_skip_ones_the_file_already_uses_and_count_up() {
        let content = "a ^summary\n\nb ^summary-1\n\na ^summary\n\na ^summary\n";
        let out = renamed(run_md(content, &[cached("summary", 1, Some("a"))]));
        assert_eq!(
            out,
            "a ^summary\n\nb ^summary-1\n\na ^summary-2\n\na ^summary-3\n"
        );
    }

    #[test]
    fn suffix_check_ignores_case() {
        let content = "a ^sum\n\nb ^SUM-1\n\na ^sum\n";
        let out = renamed(run_md(content, &[cached("sum", 1, Some("a"))]));
        assert_eq!(out, "a ^sum\n\nb ^SUM-1\n\na ^sum-2\n");
    }

    #[test]
    fn tephra_format_id_gets_a_fresh_unique_six_character_id() {
        let content = "para ^k3f9a2\n\nother ^zzzzzz\n\npara ^k3f9a2\n";
        let (fixed, _) = run_md(content, &[cached("k3f9a2", 1, Some("para"))]);
        let (out, renames) = fixed.expect("a rename");
        assert_eq!(renames.len(), 1);
        let new = &renames[0].new;
        assert!(is_tephra_format(new), "{new}");
        assert!(!["k3f9a2", "zzzzzz"].contains(&new.as_str()));
        assert_eq!(
            out,
            format!("para ^k3f9a2\n\nother ^zzzzzz\n\npara ^{new}\n")
        );
    }

    #[test]
    fn original_is_picked_by_text_then_line_then_first() {
        // Table: (content, cached block, the line whose copy is renamed).
        let table: &[(&str, CachedId, usize)] = &[
            // By text: the first block is another block's text.
            ("other ^x\n\nmine ^x\n", cached("x", 99, Some("mine")), 1),
            // Text ties, then the cached line decides.
            ("same ^x\n\nsame ^x\n", cached("x", 3, Some("same")), 1),
            // Text matches none: the cached line decides.
            ("one ^x\n\ntwo ^x\n", cached("x", 3, Some("gone")), 1),
            // Nothing matches: the first.
            ("one ^x\n\ntwo ^x\n", cached("x", 50, Some("gone")), 3),
            // No cached text at all: line, then first.
            ("one ^x\n\ntwo ^x\n", cached("x", 3, None), 1),
            // A copy pasted above pushes the original down: the nearest
            // occurrence below the cached line keeps the ID.
            (
                "same ^x\n\n# H\n\nsame ^x\n",
                cached("x", 1, Some("same")),
                5,
            ),
            (
                "same ^x\n\n# H\n\nsame ^x\n",
                cached("x", 3, Some("same")),
                1,
            ),
        ];
        for (content, was, renamed_line) in table {
            let (fixed, _) = run_md(content, std::slice::from_ref(was));
            let (_, renames) = fixed.expect("a rename");
            assert_eq!(renames.len(), 1, "{content:?} {was:?}");
            assert_eq!(renames[0].token_line, *renamed_line, "{content:?} {was:?}");
        }
    }

    #[test]
    fn an_id_the_cache_did_not_hold_is_not_planned() {
        let content = "para ^pasted\n\npara ^pasted\n";
        let (fixed, skipped) = run_md(content, &[cached("other", 1, None)]);
        assert!(fixed.is_none());
        assert_eq!(
            skipped,
            vec![Skipped::NotCached {
                id: "pasted".into(),
                lines: vec![1, 3]
            }]
        );
        // No cache at all (a first scan) is the same.
        let (fixed, skipped) = run_md(content, &[]);
        assert!(fixed.is_none());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn an_id_the_cache_held_twice_is_not_planned() {
        let content = "a ^x\n\na ^x\n";
        let was = [cached("x", 1, Some("a")), cached("x", 3, Some("a"))];
        let (fixed, skipped) = run_md(content, &was);
        assert!(fixed.is_none());
        assert!(matches!(skipped[0], Skipped::AlreadyDuplicated { .. }));
    }

    #[test]
    fn a_new_duplicate_is_fixed_beside_an_old_one_that_is_not() {
        let content = "a ^old\n\na ^old\n\nb ^new\n\nb ^new\n";
        let was = [
            cached("old", 1, Some("a")),
            cached("old", 3, Some("a")),
            cached("new", 5, Some("b")),
        ];
        let (fixed, skipped) = run_md(content, &was);
        assert_eq!(
            renamed((fixed, vec![])),
            "a ^old\n\na ^old\n\nb ^new\n\nb ^new-1\n"
        );
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn the_rest_of_the_file_is_kept_byte_for_byte() {
        let content = "\u{feff}---\ntitle: t\n---\r\n# H\r\n\r\npara  ^summary\r\n\r\ntext\ttwo ^summary  \r\nlast ^summary";
        let out = renamed(run_md(content, &[cached("summary", 6, Some("para"))]));
        assert_eq!(
            out,
            "\u{feff}---\ntitle: t\n---\r\n# H\r\n\r\npara  ^summary\r\n\r\ntext\ttwo ^summary-1  \r\nlast ^summary-2"
        );
    }

    #[test]
    fn an_id_in_a_fence_a_code_span_or_frontmatter_is_never_renamed() {
        let content =
            "---\nnote: x ^k\n---\npara ^k\n\n```\ncode ^k\n```\n\nuse `x ^k` here\n\npara ^k\n";
        let parsed = md(content);
        // Only the two real blocks are occurrences.
        assert_eq!(
            occurrences(&parsed, &parsed.index_rows(content))
                .iter()
                .map(|o| o.token_line)
                .collect::<Vec<_>>(),
            vec![4, 12]
        );
        let out = renamed(run_md(content, &[cached("k", 4, Some("para"))]));
        assert_eq!(
            out,
            "---\nnote: x ^k\n---\npara ^k\n\n```\ncode ^k\n```\n\nuse `x ^k` here\n\npara ^k-1\n"
        );
    }

    #[test]
    fn code_comment_markers_keep_everything_but_the_token() {
        use tephra_parser::Language as L;
        // (file name, language, content, the cached symbol's line, the
        // expected content after renaming the copy).
        let table: &[(&str, L, &str, i64, &str)] = &[
            (
                "lib.rs",
                L::Rust,
                "// ^k3f9a2\nfn a() {}\n\n// ^k3f9a2\nfn a() {}\n",
                2,
                "// ^k3f9a2\nfn a() {}\n\n// ^NEW\nfn a() {}\n",
            ),
            (
                "m.py",
                L::Python,
                "# ^k3f9a2\ndef a():\n    pass\n\n# ^k3f9a2\ndef a():\n    pass\n",
                2,
                "# ^k3f9a2\ndef a():\n    pass\n\n# ^NEW\ndef a():\n    pass\n",
            ),
            (
                "m.lua",
                L::Lua,
                "-- ^k3f9a2\nfunction a() end\n\n-- ^k3f9a2\nfunction a() end\n",
                2,
                "-- ^k3f9a2\nfunction a() end\n\n-- ^NEW\nfunction a() end\n",
            ),
            (
                "lib.rs",
                L::Rust,
                "fn a() {} // ^k3f9a2\n\nfn a() {} // why ^k3f9a2\n",
                1,
                "fn a() {} // ^k3f9a2\n\nfn a() {} // why ^NEW\n",
            ),
            (
                "lib.rs",
                L::Rust,
                "/* ^k3f9a2 */\nfn a() {}\n\n/* ^k3f9a2 */\nfn a() {}\n",
                2,
                "/* ^k3f9a2 */\nfn a() {}\n\n/* ^NEW */\nfn a() {}\n",
            ),
        ];
        for (name, lang, content, cached_line, want) in table {
            let parsed = code(content, name, *lang);
            let rows = parsed.index_rows(content);
            let was = [cached("k3f9a2", *cached_line, Some("a"))];
            let mut rng = StdRng::seed_from_u64(3);
            let (fixed, _) = fixed_content(
                content,
                &parsed,
                &rows,
                &was,
                &|t| code(t, name, *lang),
                &mut rng,
            );
            let (out, renames) =
                fixed.unwrap_or_else(|| panic!("a rename for {name}: {content:?}"));
            assert_eq!(renames.len(), 1, "{name}");
            let want = want.replace("NEW", &renames[0].new);
            assert!(is_tephra_format(&renames[0].new), "{name}");
            assert_eq!(out, want, "{name}");
        }
    }

    #[test]
    fn a_typed_code_id_gets_a_suffix_and_the_original_is_the_cached_line() {
        let content = "// ^parse\nfn a() {}\n\n// ^parse\nfn a() {}\n";
        let parsed = rust(content);
        let rows = parsed.index_rows(content);
        let mut rng = StdRng::seed_from_u64(1);
        let (fixed, _) = fixed_content(
            content,
            &parsed,
            &rows,
            &[cached("parse", 5, Some("a"))],
            &|t| rust(t),
            &mut rng,
        );
        // The cached block is on line 5 (the second fn): it keeps the ID.
        assert_eq!(
            fixed.expect("a rename").0,
            "// ^parse-1\nfn a() {}\n\n// ^parse\nfn a() {}\n"
        );
    }

    #[test]
    fn rewrite_finds_the_last_token_and_refuses_a_missing_one() {
        let r = |line, old: &str, new: &str| Rename {
            line,
            token_line: line,
            old: old.into(),
            new: new.into(),
        };
        assert_eq!(
            rewrite_tokens("a ^x b ^x\n", &[r(1, "x", "y")]).as_deref(),
            Some("a ^x b ^y\n")
        );
        assert_eq!(rewrite_tokens("a ^x b\n", &[r(1, "x", "y")]), None);
        assert_eq!(rewrite_tokens("a ^x\n", &[r(2, "x", "y")]), None);
        assert_eq!(rewrite_tokens("a ^xy\n", &[r(1, "x", "y")]), None);
    }

    #[test]
    fn own_writes_are_dropped_once_and_only_while_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("own-write.md");
        let other = dir.path().join("other.md");
        std::fs::write(&file, "one").unwrap();
        std::fs::write(&other, "x").unwrap();
        note_self_write(&file);

        let mut batch: HashSet<PathBuf> = [file.clone(), other.clone()].into();
        assert_eq!(drop_self_writes(&mut batch), 1);
        assert_eq!(batch, HashSet::from([other.clone()]));
        // Used up: the same path in a later batch is a real change.
        let mut batch: HashSet<PathBuf> = [file.clone()].into();
        assert_eq!(drop_self_writes(&mut batch), 0);
        assert_eq!(batch.len(), 1);

        // Changed after the write: kept.
        note_self_write(&file);
        std::fs::write(&file, "two, longer").unwrap();
        let mut batch: HashSet<PathBuf> = [file.clone()].into();
        assert_eq!(drop_self_writes(&mut batch), 0);
        assert_eq!(batch.len(), 1);
    }
}
