//! `link.rename_event` (G3, G4, G21): Obsidian, through Atlas, says
//! exactly which file or folder it renamed, and Tephra fixes the links to
//! it and answers with what came of it.
//!
//! The event is an exact signal. It is never matched by content or by
//! name similarity: the two absolute paths it carries are mapped to
//! sources by longest root, turned into source-relative paths, and the
//! rename they name is the one fixed. Everything about whether it was
//! fixed already is the fix log's ([`super::state::decide`]): the watcher
//! sees the same rename too, and whichever signal comes second finds the
//! first one's entry and does nothing.
//!
//! One event is handled start to end under [`FixEngine::decision_guard`]:
//! read the log, decide, bring the cache up to date with the rename, log
//! the entry with its pending rows, write, answer. Two events for one
//! rename, or an event and the watcher's pairing, are therefore
//! serialised: the second one sees the first one's entry.
//!
//! The reply is sent after the writes and the log, and before anything
//! rescans a source: a leftover (a file that changed under every write
//! attempt, a read-only file) is already in `pending_fixes` when
//! `partial` goes out, so Atlas may drop its queued event.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tephra_cache::{Cache, FixLogChange, SourceStatus};
use tephra_graph::{Edge, UnitGraph};

use super::log::{
    overall_status, FileEntry, FixDetail, FixNotice, FILE_NEEDS_MANUAL_FIX, FILE_PENDING,
    FILE_WRITTEN, KIND_RENAME, STATUS_APPLIED,
};
use super::state::{decide, Decision, LogRec};
use super::{FixAnnouncement, FixEngine, FixError, WrittenFile};
use crate::fix::FixSpelling;
use crate::ignore::IgnoreRules;
use crate::refactor::fix_write::{FileFixRequest, WriteStatus};
use crate::registry::SourceRegistry;

/// What an entry of the log says when the event names a file that
/// vanished from the sources altogether.
pub const MSG_MOVED_OUTSIDE: &str = "vanished, moved outside sources";

/// The stable reason of the refusal for a path in no source.
pub const REASON_NOT_INSIDE_A_SOURCE: &str = "not_inside_a_source";
/// The stable reason of the refusal for a source that is offline.
pub const REASON_SOURCE_OFFLINE: &str = "source_offline";

// ---------------------------------------------------------------
// The wire
// ---------------------------------------------------------------

/// One file a fix could not write yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeferredWire {
    /// The linking file: its path in the source of the rename, or
    /// `@source:path` when it is in another source.
    pub file: String,
    pub reason: String,
}

/// The answer to `link.rename_event`: exactly one of four, never a bare
/// "received".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RenameEventWire {
    /// Every linking file is written (or needed nothing).
    Applied { files_changed: i64 },
    /// Some linking files are written and some are left for a retry or
    /// for a person. They are in the pending-fixes table.
    Partial {
        files_changed: i64,
        deferred: Vec<DeferredWire>,
    },
    /// The fix log has this rename already.
    AlreadyApplied,
    /// Nothing can be fixed: no such rename to follow, or nothing it
    /// would change.
    NothingToFix,
}

/// Why `link.rename_event` could not answer at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameEventError {
    /// A missing, empty, relative or malformed path.
    InvalidParams(String),
    /// `path` is in no source. Never an acknowledgement.
    NotInsideASource { path: String },
    /// A refusal with a stable `reason`.
    Refused {
        reason: &'static str,
        message: String,
    },
    /// The cache or the disk failed.
    Failed(String),
}

impl RenameEventError {
    /// The stable reason of a refusal, for the error's `data`.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            RenameEventError::NotInsideASource { .. } => Some(REASON_NOT_INSIDE_A_SOURCE),
            RenameEventError::Refused { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

impl std::fmt::Display for RenameEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameEventError::InvalidParams(m)
            | RenameEventError::Refused { message: m, .. }
            | RenameEventError::Failed(m) => write!(f, "{m}"),
            RenameEventError::NotInsideASource { path } => {
                write!(f, "{path} is not inside any registered source")
            }
        }
    }
}

impl std::error::Error for RenameEventError {}

impl From<FixError> for RenameEventError {
    fn from(e: FixError) -> Self {
        RenameEventError::Failed(e.to_string())
    }
}

// ---------------------------------------------------------------
// Paths
// ---------------------------------------------------------------

/// `path` as `source.register` would store it: symlinks resolved and
/// `.`/`..` removed. The part that does not exist (the old path of a
/// rename is gone by now) is kept as given, with `.` and `..` resolved
/// lexically; a `..` that would climb out of the part that exists is an
/// error.
pub fn canonicalize_lenient(path: &Path) -> Result<PathBuf, RenameEventError> {
    let comps: Vec<Component> = path.components().collect();
    for keep in (1..=comps.len()).rev() {
        let head: PathBuf = comps[..keep].iter().collect();
        let Ok(mut out) = head.canonicalize() else {
            continue;
        };
        let mut tail: Vec<OsString> = Vec::new();
        for c in &comps[keep..] {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    if tail.pop().is_none() {
                        return Err(RenameEventError::InvalidParams(format!(
                            "\"{}\" climbs out of its existing folder with \"..\"",
                            path.display()
                        )));
                    }
                }
                Component::Normal(n) => tail.push(n.to_os_string()),
                Component::RootDir | Component::Prefix(_) => {}
            }
        }
        out.extend(tail);
        return Ok(out);
    }
    Err(RenameEventError::InvalidParams(format!(
        "\"{}\" cannot be resolved",
        path.display()
    )))
}

/// The file name the client gave, kept over the one `canonicalize`
/// answered with. On a case-insensitive disk the old path of a case-only
/// rename (`Plan.md` to `plan.md`) resolves to the new spelling, which
/// would make the two paths one and the rename invisible. Only a
/// difference of case is taken from the client (a symlink or a
/// Unicode-normalisation difference is not), and only for the last
/// segment: a path that is a source root is left alone.
fn given_spelling(
    raw: &Path,
    abs: PathBuf,
    roots: &[(String, PathBuf)],
) -> (PathBuf, Option<Located>) {
    let Some(mut at) = locate(roots, &abs) else {
        return (abs, None);
    };
    let Some(Component::Normal(given)) = raw.components().next_back() else {
        return (abs, Some(at));
    };
    let Some(given) = given.to_str() else {
        return (abs, Some(at));
    };
    let (parent, last) = match at.rel.rsplit_once('/') {
        Some((parent, last)) => (Some(parent), last),
        None => (None, at.rel.as_str()),
    };
    if at.rel.is_empty() || last == given || last.to_lowercase() != given.to_lowercase() {
        return (abs, Some(at));
    }
    at.rel = match parent {
        Some(parent) => format!("{parent}/{given}"),
        None => given.to_string(),
    };
    (abs.with_file_name(given), Some(at))
}

/// Whether `rel` is on disk under `root` with this exact spelling of
/// every segment. A case-insensitive disk also answers `stat` for a
/// spelling that differs only in case; this does not. Names that differ
/// otherwise (Unicode normalisation) still count as present, as `stat`
/// says. `listings` keeps each folder's names for the length of one
/// event.
fn exists_as_spelled(
    root: &Path,
    rel: &str,
    listings: &RefCell<HashMap<PathBuf, Vec<String>>>,
) -> bool {
    if root.join(rel).symlink_metadata().is_err() {
        return false;
    }
    let mut dir = root.to_path_buf();
    for segment in rel.split('/') {
        let mut cache = listings.borrow_mut();
        let names = cache.entry(dir.clone()).or_insert_with(|| {
            std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default()
        });
        if !names.iter().any(|n| n == segment) {
            let lower = segment.to_lowercase();
            if names.iter().any(|n| n.to_lowercase() == lower) {
                return false;
            }
        }
        drop(cache);
        dir.push(segment);
    }
    true
}

/// A path inside one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub source: String,
    pub root: PathBuf,
    /// `/`-separated, relative to the root; empty for the root itself.
    pub rel: String,
}

/// The source whose root is the longest prefix of `abs` (so a source
/// nested in another wins), with the path relative to it.
pub fn locate(roots: &[(String, PathBuf)], abs: &Path) -> Option<Located> {
    let (name, root) = roots
        .iter()
        .filter(|(_, root)| abs.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())?;
    let rel = abs.strip_prefix(root).ok()?;
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?
        .join("/");
    Some(Located {
        source: name.clone(),
        root: root.clone(),
        rel,
    })
}

fn check_param(name: &str, raw: &str) -> Result<PathBuf, RenameEventError> {
    if raw.is_empty() {
        return Err(RenameEventError::InvalidParams(format!(
            "\"{name}\" must not be empty"
        )));
    }
    if raw.contains('\0') {
        return Err(RenameEventError::InvalidParams(format!(
            "\"{name}\" must not contain a NUL byte"
        )));
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(RenameEventError::InvalidParams(format!(
            "\"{name}\" must be an absolute path, got \"{raw}\""
        )));
    }
    Ok(path)
}

// ---------------------------------------------------------------
// The engine's half
// ---------------------------------------------------------------

impl FixEngine {
    fn source_root(&self, cache: &Cache, source: &str) -> Result<PathBuf, FixError> {
        cache
            .get_source(source)?
            .map(|row| PathBuf::from(row.path))
            .ok_or_else(|| FixError::Refused(format!("no source named '{source}' is registered")))
    }

    /// `source`'s renames in the log, oldest first, as the decision
    /// reads them. Entries that cross sources are no part of a chain.
    pub(crate) fn log_recs(&self, cache: &Cache, source: &str) -> Result<Vec<LogRec>, FixError> {
        Ok(cache
            .fix_log_summaries(source)?
            .into_iter()
            .filter(|e| e.kind == KIND_RENAME)
            .filter_map(|e| {
                if e.old_source.as_deref() != Some(source)
                    || e.new_source.as_deref() != Some(source)
                {
                    return None;
                }
                Some(LogRec {
                    id: e.id,
                    old: e.old_path?,
                    new: e.new_path?,
                })
            })
            .collect())
    }

    /// Whether the log has `path` of `source` leaving it by a rename.
    pub(crate) fn log_says_departed(&self, source: &str, path: &str) -> Result<bool, FixError> {
        let cache = self.open()?;
        let log = self.log_recs(&cache, source)?;
        Ok(super::state::departed(&log, path))
    }

    /// The watcher's question: fix `old` to `new`? `Some(target)` when
    /// yes (the log may have sent `new` on to another path), `None` when
    /// the log answers it (done already, an exact event said otherwise,
    /// nothing to fix). The caller holds the decision guard.
    pub(crate) fn decide_for_watcher(
        &self,
        source: &str,
        old: &str,
        new: &str,
    ) -> Result<Option<String>, FixError> {
        let cache = self.open()?;
        let root = self.source_root(&cache, source)?;
        let log = self.log_recs(&cache, source)?;
        let exists = |p: &str| root.join(p).symlink_metadata().is_ok();
        Ok(match decide(&log, old, new, false, &exists) {
            Decision::Apply { target, .. } => Some(target),
            Decision::Override { .. } => unreachable!("only an exact event overrides"),
            other => {
                tracing::debug!(source, old, new, decision = ?other, "watcher pairing left to the log");
                None
            }
        })
    }

    /// The watcher's folder question: the moves of `moves` still to fix,
    /// each with the target the log gives it. `None` when the folder
    /// itself is in the log already, or every move is answered by it. An
    /// empty `moves` (a folder with nothing linked to) stays as it is.
    pub(crate) fn watcher_folder_moves(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<Option<Vec<(String, String)>>, FixError> {
        let cache = self.open()?;
        let root = self.source_root(&cache, source)?;
        let log = self.log_recs(&cache, source)?;
        let exists = |p: &str| root.join(p).symlink_metadata().is_ok();
        match decide(&log, old_dir, new_dir, false, &exists) {
            Decision::Apply { .. } => {}
            _ => return Ok(None),
        }
        if moves.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let todo: Vec<(String, String)> = moves
            .iter()
            .filter_map(|(old, new)| match decide(&log, old, new, false, &exists) {
                Decision::Apply { target, .. } => Some((old.clone(), target)),
                _ => None,
            })
            .collect();
        Ok((!todo.is_empty()).then_some(todo))
    }

    /// How many files the entry `id` has work for (written or not).
    pub(crate) fn files_in(&self, id: i64) -> Result<usize, FixError> {
        let cache = self.open()?;
        let row = cache
            .get_fix_log(id)?
            .ok_or_else(|| FixError::Cache(format!("fix log entry {id} is gone")))?;
        Ok(FixDetail::parse(row.detail_json.as_deref()).files.len())
    }

    /// What the entry `id` says to the caller of `link.rename_event`.
    pub(crate) fn reply_of(&self, id: i64) -> Result<RenameEventWire, FixError> {
        let cache = self.open()?;
        let row = cache
            .get_fix_log(id)?
            .ok_or_else(|| FixError::Cache(format!("fix log entry {id} is gone")))?;
        let detail = FixDetail::parse(row.detail_json.as_deref());
        let own = row.old_source.clone().unwrap_or_default();
        let deferred: Vec<DeferredWire> = detail
            .files
            .iter()
            .filter(|f| f.status == FILE_PENDING || f.status == FILE_NEEDS_MANUAL_FIX)
            .map(|f| DeferredWire {
                file: if f.source == own {
                    f.path.clone()
                } else {
                    format!("@{}:{}", f.source, f.path)
                },
                reason: f.reason.clone().unwrap_or_default(),
            })
            .collect();
        Ok(if row.status == STATUS_APPLIED && deferred.is_empty() {
            RenameEventWire::Applied {
                files_changed: row.files_changed,
            }
        } else {
            RenameEventWire::Partial {
                files_changed: row.files_changed,
                deferred,
            }
        })
    }

    /// E6: the exact event beat the entry `guess_id`, which sent the old
    /// path to `from_path`. The links that entry wrote are pointed at
    /// `to_path` instead, in the files it wrote, and only when every
    /// link to `from_path` in the file is one it wrote (the count of
    /// links is the count it recorded). A file that has changed since,
    /// or that cannot be written now, is left as it is and logged
    /// `needs_manual_fix` on `entry_id`. Nothing is queued for retry.
    fn repoint_guess(
        &self,
        source: &str,
        guess_id: i64,
        from_path: &str,
        to_path: &str,
        entry_id: i64,
        announcement: &mut FixAnnouncement,
    ) -> Result<(), FixError> {
        let cache = self.open()?;
        let Some(guess) = cache.get_fix_log(guess_id)? else {
            return Ok(());
        };
        let root = self.source_root(&cache, source)?;
        let guess_detail = FixDetail::parse(guess.detail_json.as_deref());
        let graph = UnitGraph::build_from_cache(&cache)?;
        let from_id = format!("@{source}:{from_path}");
        let to_id = format!("@{source}:{to_path}");
        if graph.unit(&from_id).is_none() || graph.unit(&to_id).is_none() {
            return Ok(());
        }
        let spelling = FixSpelling::for_target(&graph, &to_id, self.qualify_ambiguous);
        let to_files = graph.get_backlinks_all(&from_id);

        let mut outcomes: Vec<(String, String, Result<usize, String>)> = Vec::new();
        for f in guess_detail
            .files
            .iter()
            .filter(|f| f.status == FILE_WRITTEN && f.source == source)
        {
            let file_id = format!("@{}:{}", f.source, f.path);
            let edges: Vec<Edge> = to_files
                .iter()
                .filter(|e| e.kind == tephra_parser::LINK_KIND_WIKILINK && e.from == file_id)
                .cloned()
                .collect();
            if edges.is_empty() {
                continue;
            }
            if edges.len() != f.links_changed {
                outcomes.push((
                    f.source.clone(),
                    f.path.clone(),
                    Err(format!(
                        "the file has {} links to {from_path} now and the earlier fix wrote {}: left as it is",
                        edges.len(),
                        f.links_changed
                    )),
                ));
                continue;
            }
            let abs = root.join(&f.path);
            let req = FileFixRequest::new(
                file_id,
                abs.clone(),
                edges,
                from_path,
                to_path,
                spelling.rewrite_mode(),
            );
            let result = self.writer.write_file(&req);
            let outcome = match result.status {
                WriteStatus::Written => {
                    announcement.merge(FixAnnouncement {
                        written: vec![WrittenFile {
                            source: f.source.clone(),
                            path: abs,
                        }],
                        ..FixAnnouncement::default()
                    });
                    Ok(result.links_changed)
                }
                WriteStatus::NothingToWrite | WriteStatus::NotFound => continue,
                WriteStatus::Deferred | WriteStatus::NeedsManualFix => Err(format!(
                    "could not point the link from a wrong guess at {to_path}: {}",
                    result.reason.unwrap_or_default()
                )),
            };
            outcomes.push((f.source.clone(), f.path.clone(), outcome));
        }
        if outcomes.is_empty() {
            return Ok(());
        }

        let Some(row) = cache.get_fix_log(entry_id)? else {
            return Ok(());
        };
        let mut detail = FixDetail::parse(row.detail_json.as_deref());
        for (file_source, path, outcome) in outcomes {
            let at = match detail
                .files
                .iter()
                .position(|f| f.source == file_source && f.path == path)
            {
                Some(i) => i,
                None => {
                    detail.files.push(FileEntry::pending(&file_source, &path));
                    detail.files.len() - 1
                }
            };
            let entry = &mut detail.files[at];
            match outcome {
                Ok(links) => {
                    entry.links_changed = if entry.status == FILE_WRITTEN {
                        entry.links_changed + links
                    } else {
                        links
                    };
                    entry.status = FILE_WRITTEN.to_string();
                }
                Err(reason) => {
                    entry.status = FILE_NEEDS_MANUAL_FIX.to_string();
                    entry.reason = Some(reason);
                }
            }
        }
        let change = FixLogChange {
            status: overall_status(&detail.files).to_string(),
            files_changed: detail.files_written(),
            detail_json: Some(detail.to_json()),
        };
        self.write_cache(|| cache.update_fix_log(entry_id, &change))?;
        // The notice of this entry as it stands now.
        if let Some(row) = cache.get_fix_log(entry_id)? {
            let now = FixNotice::from_row(&row);
            match announcement
                .notices
                .iter_mut()
                .find(|n| n.kind == now.kind && n.old == now.old && n.new == now.new)
            {
                Some(slot) => *slot = now,
                None if now.files_changed > 0 || now.deferred > 0 => announcement.notices.push(now),
                None => {}
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------
// The handler
// ---------------------------------------------------------------

/// `link.rename_event`, over the registry (sources, indexing) and the
/// fix engine (the log, the pending rows, the writes).
pub struct RenameEvents {
    registry: Arc<SourceRegistry>,
    engine: Arc<FixEngine>,
}

/// One side of an event, resolved.
struct Side {
    abs: PathBuf,
    at: Located,
}

impl RenameEvents {
    pub fn new(registry: Arc<SourceRegistry>, engine: Arc<FixEngine>) -> Self {
        RenameEvents { registry, engine }
    }

    /// Answers one event. `old_path` and `new_path` are absolute paths
    /// on disk; the rename has already happened there.
    pub fn handle(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Result<RenameEventWire, RenameEventError> {
        let old_raw = check_param("old_path", old_path)?;
        let new_raw = check_param("new_path", new_path)?;
        let old_abs = canonicalize_lenient(&old_raw)?;
        let new_abs = canonicalize_lenient(&new_raw)?;

        let rows = self
            .registry
            .list()
            .map_err(|e| RenameEventError::Failed(e.to_string()))?;
        let roots: Vec<(String, PathBuf)> = rows
            .iter()
            .map(|r| (r.name.clone(), PathBuf::from(&r.path)))
            .collect();
        let offline = |source: &str| {
            rows.iter()
                .any(|r| r.name == source && r.status == SourceStatus::Offline)
        };

        let (old_abs, old_at) = given_spelling(&old_raw, old_abs, &roots);
        let (new_abs, new_at) = given_spelling(&new_raw, new_abs, &roots);

        let Some(old_at) = old_at else {
            return Err(RenameEventError::NotInsideASource {
                path: old_path.to_string(),
            });
        };
        if offline(&old_at.source) {
            return Err(RenameEventError::Refused {
                reason: REASON_SOURCE_OFFLINE,
                message: format!("source '{}' is offline", old_at.source),
            });
        }
        let old = Side {
            abs: old_abs,
            at: old_at,
        };
        // The root itself is no file or folder of the source.
        if old.at.rel.is_empty() {
            return Ok(RenameEventWire::NothingToFix);
        }

        let Some(new_at) = new_at else {
            return self.moved_outside(&old);
        };
        if new_at.rel.is_empty() {
            return Ok(RenameEventWire::NothingToFix);
        }
        if offline(&new_at.source) {
            return Err(RenameEventError::Refused {
                reason: REASON_SOURCE_OFFLINE,
                message: format!("source '{}' is offline", new_at.source),
            });
        }
        let crosses = new_at.source != old.at.source;
        let new = Side {
            abs: new_abs,
            at: new_at,
        };
        if crosses {
            return self.moved(&old, &new);
        }

        // A path in an ignored folder or `.tephraignore` is not indexed:
        // nothing links to it as far as Tephra knows.
        let ignore =
            IgnoreRules::load(&old.at.root).map_err(|e| RenameEventError::Failed(e.to_string()))?;
        if ignore.is_ignored(Path::new(&old.at.rel)) || ignore.is_ignored(Path::new(&new.at.rel)) {
            return Ok(RenameEventWire::NothingToFix);
        }

        self.rename(&old, &new)
    }

    /// The old path was in one source and the new one is in another
    /// (G6): the file moved. The same move the watcher's pool pairs, so
    /// whichever signal comes second answers `already_applied` and logs
    /// nothing (E7). A folder is left to the watcher, which pairs its
    /// files one by one; a copy (the old path is still there) is no move.
    fn moved(&self, old: &Side, new: &Side) -> Result<RenameEventWire, RenameEventError> {
        let old_ignore =
            IgnoreRules::load(&old.at.root).map_err(|e| RenameEventError::Failed(e.to_string()))?;
        let new_ignore =
            IgnoreRules::load(&new.at.root).map_err(|e| RenameEventError::Failed(e.to_string()))?;
        if old_ignore.is_ignored(Path::new(&old.at.rel))
            || new_ignore.is_ignored(Path::new(&new.at.rel))
            || new.abs.is_dir()
        {
            return Ok(RenameEventWire::NothingToFix);
        }

        let _decision = self.engine.decision_guard();
        let (old_source, new_source) = (old.at.source.as_str(), new.at.source.as_str());
        if self
            .engine
            .move_logged(old_source, &old.at.rel, new_source, &new.at.rel)?
        {
            return Ok(RenameEventWire::AlreadyApplied);
        }
        if old.abs.symlink_metadata().is_ok() {
            // Something is at the old path still: a copy, not a move.
            return Ok(RenameEventWire::NothingToFix);
        }

        // The cache learns of the move now, in both sources, so the fix
        // reads the graph as the watchers would have left it.
        let index_old = [old.abs.clone()];
        let index_new = [new.abs.clone()];
        self.registry
            .reindex_paths(old_source, &index_old)
            .map_err(|e| RenameEventError::Failed(e.to_string()))?;
        self.registry
            .reindex_paths(new_source, &index_new)
            .map_err(|e| RenameEventError::Failed(e.to_string()))?;

        let (entry, mut announcement) = match self.engine.begin_move_announced(
            old_source,
            &old.at.rel,
            new_source,
            &new.at.rel,
        ) {
            Ok(done) => done,
            // The new path is not a unit (a file Tephra does not index):
            // there is nothing to link to.
            Err(FixError::Refused(why)) => {
                tracing::debug!(old_source, old = %old.at.rel, new_source, new = %new.at.rel, %why, "move event refused by the engine");
                self.announce_paths(old_source, &index_old);
                self.announce_paths(new_source, &index_new);
                return Ok(RenameEventWire::NothingToFix);
            }
            Err(e) => return Err(e.into()),
        };
        for (source, paths) in [(old_source, &index_old), (new_source, &index_new)] {
            for path in paths {
                announcement.merge(FixAnnouncement {
                    indexed: vec![WrittenFile {
                        source: source.to_string(),
                        path: path.clone(),
                    }],
                    ..FixAnnouncement::default()
                });
            }
        }
        self.engine.announce(announcement);
        // Nothing links to the file and it links to nothing that changes:
        // the move is logged (so the watcher's pairing of it answers
        // `already_applied`), but there was nothing to fix.
        if self.engine.files_in(entry.id)? == 0 {
            return Ok(RenameEventWire::NothingToFix);
        }
        Ok(self.engine.reply_of(entry.id)?)
    }

    /// The old path was in a source and the new one is in none: the file
    /// left the sources. Its links are broken and there is nothing to
    /// rewrite them to; the log says so, once.
    fn moved_outside(&self, old: &Side) -> Result<RenameEventWire, RenameEventError> {
        let _decision = self.engine.decision_guard();
        let source = &old.at.source;
        let cache = Cache::open(self.engine_cache_path()).map_err(fix_err)?;
        let row = cache
            .get_source(source)
            .map_err(fix_err)?
            .ok_or_else(|| RenameEventError::Failed(format!("no source named '{source}'")))?;
        if old.abs.symlink_metadata().is_ok() {
            // Something is at the old path still.
            return Ok(RenameEventWire::NothingToFix);
        }
        let known = cache
            .get_unit_by_rel_path(row.id, &old.at.rel)
            .map_err(fix_err)?
            .is_some()
            || !cache
                .units_under_prefix(row.id, &old.at.rel)
                .map_err(fix_err)?
                .is_empty();
        if known {
            self.engine.record_uncertain_inner(
                source,
                &old.at.rel,
                MSG_MOVED_OUTSIDE,
                super::Dedupe::AnyRecent,
            )?;
        }
        self.registry
            .reindex_paths(source, std::slice::from_ref(&old.abs))
            .map_err(|e| RenameEventError::Failed(e.to_string()))?;
        if known {
            self.engine.announce(FixAnnouncement {
                indexed: vec![WrittenFile {
                    source: source.clone(),
                    path: old.abs.clone(),
                }],
                ..FixAnnouncement::default()
            });
        }
        Ok(RenameEventWire::NothingToFix)
    }

    fn engine_cache_path(&self) -> &Path {
        &self.engine.cache_path
    }

    fn rename(&self, old: &Side, new: &Side) -> Result<RenameEventWire, RenameEventError> {
        let source = old.at.source.as_str();
        let root = old.at.root.as_path();
        let is_dir = new.abs.is_dir();

        let _decision = self.engine.decision_guard();
        let cache = Cache::open(self.engine_cache_path()).map_err(fix_err)?;
        let log = self.engine.log_recs(&cache, source)?;
        // Spelled as given: on a case-insensitive disk `Plan.md` is
        // still "there" after a rename to `plan.md`, and that must not
        // make the old name look in use.
        let listings = RefCell::new(HashMap::new());
        let exists = |p: &str| exists_as_spelled(root, p, &listings);
        let decision = decide(&log, &old.at.rel, &new.at.rel, true, &exists);

        let (target, guess) = match decision {
            Decision::AlreadyApplied => return Ok(RenameEventWire::AlreadyApplied),
            Decision::NothingToFix(_) => return Ok(RenameEventWire::NothingToFix),
            Decision::Apply { target, .. } => (target, None),
            // A folder is fixed file by file below; re-pointing a guess
            // is for the one file the guess named.
            Decision::Override {
                target,
                guess_id,
                guess_target,
                ..
            } if !is_dir => (target, Some((guess_id, guess_target))),
            Decision::Override { target, .. } => (target, None),
        };
        let target_abs = root.join(&target);

        // The cache learns of the rename now: the old path is gone and
        // the new one is a unit, so the fix reads the graph as the
        // watcher would have left it.
        let mut index = vec![new.abs.clone()];
        if target_abs != new.abs {
            index.push(target_abs.clone());
        }
        // The paths the push names. The old spelling of a case-only
        // rename is one of them although it is never indexed: a client
        // that reads the push's paths learns that the old unit is gone.
        let mut named = Vec::new();
        if old.abs.symlink_metadata().is_ok() {
            // The old spelling still stats (case-insensitive disk): it
            // is the new file under another name, so its rows are
            // dropped by name; indexing the path would add them again.
            self.registry
                .forget_spellings(source, std::slice::from_ref(&old.at.rel))
                .map_err(|e| RenameEventError::Failed(e.to_string()))?;
            named.push(old.abs.clone());
        } else {
            index.push(old.abs.clone());
        }
        self.registry
            .reindex_paths(source, &index)
            .map_err(|e| RenameEventError::Failed(e.to_string()))?;
        named.extend(index.iter().cloned());

        let started = if is_dir {
            self.fix_folder(&cache, source, root, &log, &old.at.rel, &target, &exists)?
        } else {
            self.engine
                .begin_rename_announced(source, &old.at.rel, &target)
                .map(Some)
        };
        let (entry, mut announcement) = match started {
            Ok(Some(done)) => done,
            Ok(None) => return Ok(RenameEventWire::AlreadyApplied),
            // The new path is not a unit (a file Tephra does not index,
            // or one that vanished again): there is nothing to link to.
            Err(FixError::Refused(why)) => {
                tracing::debug!(source, old = %old.at.rel, new = %new.at.rel, %why, "rename event refused by the engine");
                self.announce_paths(source, &named);
                return Ok(RenameEventWire::NothingToFix);
            }
            Err(e) => return Err(e.into()),
        };

        if let Some((guess_id, guess_target)) = guess {
            if let Err(e) = self.engine.repoint_guess(
                source,
                guess_id,
                &guess_target,
                &target,
                entry.id,
                &mut announcement,
            ) {
                tracing::warn!(error = %e, "re-pointing the links of an earlier guess failed");
            }
        }

        // The rename itself is news too: one push, after the log entry
        // is complete. The paths were indexed above, so the announcement
        // names them without indexing them again (the old spelling of a
        // case-only rename must not be indexed at all).
        for path in &named {
            announcement.merge(FixAnnouncement {
                indexed: vec![WrittenFile {
                    source: source.to_string(),
                    path: path.clone(),
                }],
                ..FixAnnouncement::default()
            });
        }
        self.engine.announce(announcement);

        Ok(self.engine.reply_of(entry.id)?)
    }

    fn announce_paths(&self, source: &str, paths: &[PathBuf]) {
        self.engine.announce(FixAnnouncement {
            indexed: paths
                .iter()
                .map(|p| WrittenFile {
                    source: source.to_string(),
                    path: p.clone(),
                })
                .collect(),
            ..FixAnnouncement::default()
        });
    }

    /// A folder: every file now under `new_dir` is the file at the same
    /// place under `old_dir` (the event says the folder was renamed, so
    /// the structure is exact, nothing is guessed). The moves the log
    /// already answers are left out; `None` when it answers all of them.
    #[allow(clippy::too_many_arguments)]
    fn fix_folder(
        &self,
        cache: &Cache,
        source: &str,
        _root: &Path,
        log: &[LogRec],
        old_dir: &str,
        new_dir: &str,
        exists: &dyn Fn(&str) -> bool,
    ) -> Result<Result<Option<(super::FixEntryWire, FixAnnouncement)>, FixError>, RenameEventError>
    {
        let row = cache
            .get_source(source)
            .map_err(fix_err)?
            .ok_or_else(|| RenameEventError::Failed(format!("no source named '{source}'")))?;
        let units = cache.units_under_prefix(row.id, new_dir).map_err(fix_err)?;
        let prefix = format!("{new_dir}/");
        let mut moves: Vec<(String, String)> = Vec::new();
        let mut answered = 0usize;
        for unit in &units {
            let Some(rest) = unit.rel_path.strip_prefix(&prefix) else {
                continue;
            };
            let old_file = format!("{old_dir}/{rest}");
            match decide(log, &old_file, &unit.rel_path, true, exists) {
                Decision::AlreadyApplied => answered += 1,
                Decision::Apply { target, .. } | Decision::Override { target, .. } => {
                    moves.push((old_file, target));
                }
                Decision::NothingToFix(_) => {}
            }
        }
        if moves.is_empty() {
            return Ok(if answered > 0 {
                Ok(None)
            } else {
                Err(FixError::Refused(format!(
                    "no file of '{new_dir}' is indexed, so no link to it can be fixed"
                )))
            });
        }
        Ok(self
            .engine
            .begin_folder_rename_announced(source, old_dir, new_dir, &moves)
            .map(Some))
    }
}

fn fix_err(e: tephra_cache::CacheError) -> RenameEventError {
    RenameEventError::Failed(e.to_string())
}
