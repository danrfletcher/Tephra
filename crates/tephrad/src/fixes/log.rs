//! What the fix log says, in the two shapes it takes: the JSON kept in
//! `fix_log.detail_json` (per-file results) and the wire entry
//! `fix.list` returns (G10, G18).
//!
//! The overall status of a fix is never stored on its own: it is
//! worked out from the per-file results by [`overall_status`], so the
//! two cannot disagree.

use serde::{Deserialize, Serialize};
use tephra_cache::FixLogRow;

/// A fix of a rename: the links to a file that moved.
pub const KIND_RENAME: &str = "rename";

/// A file that vanished with no certain pair: its links are broken and
/// nothing is queued (G10).
pub const KIND_UNCERTAIN: &str = "uncertain";

/// Every file rewritten, dropped as not needing a fix, or cancelled.
pub const STATUS_APPLIED: &str = "applied";
/// Some file is rewritten and some other is still to do or needs a
/// person; or nothing is written yet and work is still queued.
pub const STATUS_PARTIAL: &str = "partial";
/// No file was rewritten and the ones left need a person.
pub const STATUS_NEEDS_MANUAL_FIX: &str = "needs_manual_fix";
/// The status of a [`KIND_UNCERTAIN`] entry.
pub const STATUS_UNCERTAIN: &str = "uncertain";

/// A file still to do.
pub const FILE_PENDING: &str = "pending";
/// A file rewritten.
pub const FILE_WRITTEN: &str = "written";
/// A file with nothing to rewrite: the link is already right or gone,
/// the old name was reused, or the target moved or was deleted again.
pub const FILE_NOTHING_TO_FIX: &str = "nothing_to_fix";
/// A file Tephra will not rewrite by itself, or gave up on.
pub const FILE_NEEDS_MANUAL_FIX: &str = "needs_manual_fix";
/// A file whose fix was cancelled before it wrote: an exact event
/// corrected the target.
pub const FILE_CANCELLED: &str = "cancelled";

/// One linking file in a fix's detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub source: String,
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub links_changed: usize,
    /// Write attempts in the last call that tried this file.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Edits by someone else the write loop got past before it wrote
    /// this file (counted over every try).
    #[serde(default)]
    pub recovered_edits: u32,
    /// The renames from the old target to the one written, when the
    /// target moved on while the file waited (`[A, B, C]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
}

impl FileEntry {
    pub fn pending(source: &str, path: &str) -> Self {
        FileEntry {
            source: source.to_string(),
            path: path.to_string(),
            status: FILE_PENDING.to_string(),
            links_changed: 0,
            attempts: 0,
            reason: None,
            recovered_edits: 0,
            chain: Vec::new(),
        }
    }
}

/// `fix_log.detail_json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixDetail {
    #[serde(default)]
    pub files: Vec<FileEntry>,
    /// Recovered edits over every file: a changed-then-written file the
    /// write loop got through (G22).
    #[serde(default)]
    pub recovered_edits: u32,
    /// Free text, for an entry that has no files (an uncertain one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FixDetail {
    pub fn parse(raw: Option<&str>) -> Self {
        raw.and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("FixDetail always serializes")
    }

    /// The entry for `(source, path)`, added as pending when absent.
    pub fn file_mut(&mut self, source: &str, path: &str) -> &mut FileEntry {
        let at = match self
            .files
            .iter()
            .position(|f| f.source == source && f.path == path)
        {
            Some(i) => i,
            None => {
                self.files.push(FileEntry::pending(source, path));
                self.files.len() - 1
            }
        };
        &mut self.files[at]
    }

    pub fn files_written(&self) -> i64 {
        self.files
            .iter()
            .filter(|f| f.status == FILE_WRITTEN)
            .count() as i64
    }
}

/// The overall status of a fix from its files. No files (nothing linked
/// to the target) is applied. Any file still pending is partial. Files
/// that need a person with none pending are partial when some other
/// file was written, and `needs_manual_fix` when none was.
pub fn overall_status(files: &[FileEntry]) -> &'static str {
    let any = |status: &str| files.iter().any(|f| f.status == status);
    if any(FILE_PENDING) {
        STATUS_PARTIAL
    } else if any(FILE_NEEDS_MANUAL_FIX) {
        if any(FILE_WRITTEN) {
            STATUS_PARTIAL
        } else {
            STATUS_NEEDS_MANUAL_FIX
        }
    } else {
        STATUS_APPLIED
    }
}

/// One entry of the `fixes` list a `graph:changed` push carries (G17,
/// G25): data only, naming the rename and what came of it. `new_source`
/// is present only when the move crosses sources. `deferred` counts the
/// linking files left for a retry or for a person (0 when every file was
/// written); `files_changed` counts only files actually rewritten.
///
/// It is the same fact as the fix log's entry, so it is built from the
/// log row ([`FixNotice::from_row`]) and cannot disagree with `fix.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixNotice {
    pub kind: String,
    pub source: String,
    pub old: String,
    pub new: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_source: Option<String>,
    pub files_changed: i64,
    pub status: String,
    pub deferred: i64,
}

impl FixNotice {
    /// The notice for the log entry `row`, as the entry stands now.
    pub fn from_row(row: &FixLogRow) -> Self {
        let detail = FixDetail::parse(row.detail_json.as_deref());
        let deferred = detail
            .files
            .iter()
            .filter(|f| f.status == FILE_PENDING || f.status == FILE_NEEDS_MANUAL_FIX)
            .count() as i64;
        let source = row.old_source.clone().unwrap_or_default();
        let new_source = row
            .new_source
            .clone()
            .filter(|s| !s.is_empty() && *s != source);
        FixNotice {
            kind: row.kind.clone(),
            source,
            old: row.old_path.clone().unwrap_or_default(),
            new: row.new_path.clone().unwrap_or_default(),
            new_source,
            files_changed: row.files_changed,
            status: row.status.clone(),
            deferred,
        }
    }
}

/// One entry of `fix.list`'s answer: time, kind, source(s), old, new,
/// files changed and status (G18). `new` is `null` for an uncertain
/// entry, which has no pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixEntryWire {
    pub id: i64,
    /// Epoch seconds.
    pub time: i64,
    pub kind: String,
    pub sources: Vec<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub files_changed: i64,
    pub status: String,
}

impl From<&FixLogRow> for FixEntryWire {
    fn from(row: &FixLogRow) -> Self {
        let mut sources: Vec<String> = Vec::new();
        for s in [&row.old_source, &row.new_source].into_iter().flatten() {
            if !sources.contains(s) {
                sources.push(s.clone());
            }
        }
        FixEntryWire {
            id: row.id,
            time: row.created_at,
            kind: row.kind.clone(),
            sources,
            old: row.old_path.clone(),
            new: row.new_path.clone(),
            files_changed: row.files_changed,
            status: row.status.clone(),
        }
    }
}
