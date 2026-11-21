//! The rewrite half of the automatic link fix after a rename.
//!
//! [`fix_rename`] takes one certain rename of a file inside one source
//! (`old_path` to `new_path`) and, for every wikilink that named the
//! old file, writes a link that resolves to the renamed file. It is a
//! function, not a service: noticing the rename (the watcher), the
//! `link.rename_event` call, the fix log and moves across sources are
//! other work. Callers and tests hand it the graph as it is *after* the
//! rename was indexed, so the old path is no longer a unit, its
//! incoming links are dangling, and the renamed file is a unit at
//! `new_path`.
//!
//! What it reuses from [`crate::refactor`]: [`crate::refactor::compute_fix_changes`]
//! (which spans change and to what), [`apply_changes_to_content`] and
//! the same atomic write ([`rewrite_links_atomic`]) the manual
//! `link.apply_refactor` goes through, under the same per-file lock
//! [`crate::file_lock`] gives `block.ensure_id`.
//!
//! Rules, each from the spec's fence:
//!
//! - Only wikilink spans are written (`[[...]]` and `![[...]]`, in a body
//!   or a frontmatter property, or in a code file's comment). A plain
//!   Markdown link `[t](Old.md)` is never touched here (F7).
//! - A bare target is written as the new file name only when the graph
//!   says that name reaches the renamed file; otherwise as the full
//!   source-relative stem ([`tephra_graph::UnitGraph::shortest_unique_target`]).
//!   With the Atlas fallback on (`qualify_ambiguous_links`, G20) a plain
//!   link whose name also exists in another source is the full stem too.
//!   [`FixSpelling`] decides both, here and for every later fix kind.
//! - A docx, pptx or PDF is never opened for writing. It is named in
//!   the result as [`FixStatus::NeedsManualFix`].
//! - A file that is read-only, or a symlink, is not written and is
//!   named as [`FixStatus::NeedsManualFix`].
//! - The renamed file is fixed like any other referrer (its own links
//!   to its old name are rewritten) and is written only for that.
//! - Each file is read, the changes computed from what was just read,
//!   and written, all under the file's lock. A link the file no longer
//!   contains is simply not found, and a file with nothing to change is
//!   never written, so its mtime stays as it was and a second run
//!   writes nothing.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tephra_graph::{Edge, UnitGraph};

use crate::refactor::fix_write::RewriteMode;
use crate::refactor::{apply_changes_to_content, rewrite_links_atomic, DOCUMENT_READ_ONLY};
use crate::rpc::backend::SourceBackend;

/// What happened to one file that has a link to the renamed file.
/// A file with nothing to change is not listed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixStatus {
    /// Rewritten on disk.
    Changed,
    /// Left as it is because Tephra does not write it (a document, a
    /// read-only file, a symlink). The person has to fix the link.
    NeedsManualFix,
    /// A read, a check or the write failed; the file is as it was.
    Failed,
}

/// One file's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileFix {
    /// The file's federated id (`@source:rel_path`).
    pub file: String,
    pub status: FixStatus,
    /// How many link spans were rewritten (`0` unless `Changed`).
    pub links_changed: usize,
    /// Why, for `NeedsManualFix` and `Failed`.
    pub reason: Option<String>,
}

/// [`fix_rename`]'s result: every file it wrote or could not write, and
/// how many it wrote. A rename nothing links to, or a second run after
/// success, gives an empty list and `0`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FixResult {
    pub files_changed: usize,
    pub files: Vec<FileFix>,
}

impl FixResult {
    /// The files with the given status, in file order.
    pub fn with_status(&self, status: FixStatus) -> Vec<&FileFix> {
        self.files.iter().filter(|f| f.status == status).collect()
    }
}

/// How the automatic fix spells a plain link (no `@source:` prefix) to
/// one target. **The one place that decides it:** the rename fix
/// ([`fix_rename_with`]) and every later fix kind (the engine's move,
/// promote and unpromote in `crate::fixes`) get their spelling here and
/// nowhere else, so the Atlas fallback (G20, `qualify_ambiguous_links`)
/// is a single branch ([`FixSpelling::for_target`]) and no other code
/// spells a link target.
///
/// Two facts, kept apart because a link with a prefix is not affected by
/// the second:
///
/// - `bare_resolves`: the bare file name reaches the target inside its
///   own source (the resolver's own rule).
/// - `qualify`: the Atlas fallback is on, the target has a folder, and
///   another registered source holds a file of the same bare name, so a
///   plain link is written as the full source-relative stem
///   (`docs/Plan`) even though the bare name would resolve in the
///   linking file's own source. Case-sensitive, like the resolver.
///
/// The fallback changes how a target is spelled, never what it targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixSpelling {
    pub bare_resolves: bool,
    pub qualify: bool,
}

impl FixSpelling {
    /// The spelling for target unit `id` in `graph`. `qualify_ambiguous`
    /// is the `qualify_ambiguous_links` setting.
    pub fn for_target(graph: &UnitGraph, id: &str, qualify_ambiguous: bool) -> Self {
        Self::for_target_ignoring(graph, id, qualify_ambiguous, &[])
    }

    /// [`FixSpelling::for_target`] for a file that has moved to another
    /// source while `graph` still holds it at its old id (`moved_from`):
    /// that unit is the target itself, so it does not make the name
    /// "also in another source".
    pub fn for_target_ignoring(
        graph: &UnitGraph,
        id: &str,
        qualify_ambiguous: bool,
        moved_from: &[&str],
    ) -> Self {
        let bare_resolves = graph
            .shortest_unique_target(id)
            .is_some_and(|t| !t.contains('/'));
        // The only branch of the Atlas fallback. A target at its source
        // root has no fuller same-source path: it stays the bare name.
        let qualify = qualify_ambiguous
            && has_folder(id)
            && graph.name_in_other_source_ignoring(id, moved_from);
        FixSpelling {
            bare_resolves,
            qualify,
        }
    }

    /// Whether a link with no `@source:` prefix may be written as the
    /// bare file name.
    pub fn plain_bare(self) -> bool {
        self.bare_resolves && !self.qualify
    }

    /// The rewrite of a rename within one source ([`RewriteMode`]).
    pub fn rewrite_mode(self) -> RewriteMode {
        if self.qualify {
            RewriteMode::FixQualifying {
                bare_resolves: self.bare_resolves,
            }
        } else {
            RewriteMode::Fix {
                bare_resolves: self.bare_resolves,
            }
        }
    }

    /// The text of a link with no prefix to unit `id`: the shortest
    /// that resolves, or the full stem when [`FixSpelling::qualify`].
    /// `None` when `id` is not in `graph`.
    pub fn target_text(graph: &UnitGraph, id: &str, qualify_ambiguous: bool) -> Option<String> {
        if Self::for_target(graph, id, qualify_ambiguous).qualify {
            graph.full_target(id)
        } else {
            graph.shortest_unique_target(id)
        }
    }
}

/// Whether the unit `id` (`@source:rel_path`) is inside a folder of its
/// source, so a fuller same-source path than its bare name exists.
fn has_folder(id: &str) -> bool {
    id.split_once(':')
        .is_some_and(|(_, rel)| crate::refactor::strip_md_ext(rel).0.contains('/'))
}

/// Rewrites every wikilink that named `old_path` of `source` so it
/// names `new_path`. `graph` is the graph after the rename was indexed
/// (see the module doc) and `sources` gives each source's root on disk.
///
/// `Err` only for a call that cannot be answered at all: `source` is not
/// registered, `new_path` is not a unit in `graph`, or `old_path` still
/// is one (nothing was renamed, and rewriting links to a file that still
/// exists would break them). Per-file problems are in the result.
pub fn fix_rename(
    graph: &UnitGraph,
    sources: &dyn SourceBackend,
    source: &str,
    old_path: &str,
    new_path: &str,
) -> Result<FixResult, String> {
    fix_rename_with(graph, sources, source, old_path, new_path, false)
}

/// [`fix_rename`] with the Atlas fallback (G20) as the caller's
/// `qualify_ambiguous_links` setting says: when `qualify_ambiguous` is
/// `true`, a plain link whose bare name also exists in another source is
/// written as the full same-source path. See [`FixSpelling`]; `false`
/// is exactly [`fix_rename`].
pub fn fix_rename_with(
    graph: &UnitGraph,
    sources: &dyn SourceBackend,
    source: &str,
    old_path: &str,
    new_path: &str,
    qualify_ambiguous: bool,
) -> Result<FixResult, String> {
    let roots: HashMap<String, String> = sources
        .list()?
        .into_iter()
        .map(|r| (r.name, r.path))
        .collect();
    if !roots.contains_key(source) {
        return Err(format!("no source named '{source}' is registered"));
    }
    let old_id = format!("@{source}:{old_path}");
    let new_id = format!("@{source}:{new_path}");
    if old_path == new_path {
        return Ok(FixResult::default());
    }
    if graph.unit(&new_id).is_none() {
        return Err(format!("'{new_path}' is not a unit in source '{source}'"));
    }
    if graph.unit(&old_id).is_some() {
        return Err(format!(
            "'{old_path}' is still a unit in source '{source}': the rename has not been indexed"
        ));
    }

    // The bare file name is kept only when it still reaches the renamed
    // file (and the Atlas fallback does not ask for more); otherwise
    // every bare link gets the full stem.
    let spelling = FixSpelling::for_target(graph, &new_id, qualify_ambiguous);

    // Every link that named the old path: those that now dangle, and
    // those the rename handed to another file of the same name (they
    // resolve, so a lookup by the old id alone would never find them).
    // A self-link of the renamed file may still be recorded under the
    // old id if the cache has not caught up; the file now lives at the
    // new one.
    let mut by_file: HashMap<String, Vec<Edge>> = HashMap::new();
    for mut edge in graph.get_rename_backlinks(&old_id, &new_id) {
        if edge.kind != tephra_parser::LINK_KIND_WIKILINK {
            continue;
        }
        if edge.from == old_id {
            edge.from = new_id.clone();
        }
        by_file.entry(edge.from.clone()).or_default().push(edge);
    }

    let mut files: Vec<&String> = by_file.keys().collect();
    files.sort();
    let mut result = FixResult::default();
    for file_id in files {
        let outcome = fix_one_file(
            file_id,
            &by_file[file_id],
            &roots,
            old_path,
            new_path,
            spelling,
        );
        if let Some(fix) = outcome {
            if fix.status == FixStatus::Changed {
                result.files_changed += 1;
            }
            result.files.push(fix);
        }
    }
    Ok(result)
}

/// One referring file: `None` when there is nothing to write and
/// nothing to report.
fn fix_one_file(
    file_id: &str,
    edges: &[Edge],
    roots: &HashMap<String, String>,
    old_path: &str,
    new_path: &str,
    spelling: FixSpelling,
) -> Option<FileFix> {
    let report = |status, links_changed, reason: Option<String>| {
        Some(FileFix {
            file: file_id.to_string(),
            status,
            links_changed,
            reason,
        })
    };
    let Some(abs) = abs_path(file_id, roots) else {
        return report(
            FixStatus::Failed,
            0,
            Some("source no longer registered".to_string()),
        );
    };

    // F4: a document is never opened, not even to read.
    if tephra_parser::DocumentFormat::from_path(&abs).is_some() {
        return report(
            FixStatus::NeedsManualFix,
            0,
            Some(DOCUMENT_READ_ONLY.to_string()),
        );
    }

    // Held from the read to the rename, so a `block.ensure_id` or another
    // fix on this file never starts from bytes this one is replacing.
    let _guard = crate::file_lock::lock(&abs);

    let bytes = match fs::read(&abs) {
        Ok(b) => b,
        // The file is gone: nothing to fix, and not an error to raise.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return report(FixStatus::Failed, 0, Some(e.to_string())),
    };
    let content = match String::from_utf8(bytes) {
        Ok(c) => c,
        Err(_) => {
            return report(
                FixStatus::NeedsManualFix,
                0,
                Some("the file is not UTF-8 text, so its links are not rewritten".to_string()),
            )
        }
    };

    // Computed from what was just read: a link the file no longer
    // contains is not found, so it gives no change and no error.
    let contents = HashMap::from([(file_id.to_string(), content.clone())]);
    let changes = spelling
        .rewrite_mode()
        .changes(edges, &contents, old_path, new_path);
    if changes.is_empty() {
        return None;
    }

    // `rewrite_links_atomic` replaces the file by rename, which works on
    // a read-only file inside a writable folder. A read-only file is the
    // person's way of saying "do not edit", so honour it: owner-write
    // bit off (F14/FR27/G24), which `Permissions::readonly()` does not
    // test on Unix (it is true only when no write bit at all is set).
    match fs::metadata(&abs) {
        Ok(m) if owner_write_off(&m) => {
            return report(
                FixStatus::NeedsManualFix,
                0,
                Some("the file is read-only, so its links are not rewritten".to_string()),
            );
        }
        Ok(_) => {}
        Err(e) => return report(FixStatus::Failed, 0, Some(e.to_string())),
    }

    let refs: Vec<_> = changes.iter().collect();
    let new_content = match apply_changes_to_content(&content, &refs) {
        Ok(c) => c,
        Err(e) => return report(FixStatus::Failed, 0, Some(e)),
    };
    match rewrite_links_atomic(&abs, new_content.as_bytes()) {
        Ok(()) => report(FixStatus::Changed, changes.len(), None),
        // A symlink is refused on purpose (see `rewrite_links_atomic_with`).
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            report(FixStatus::NeedsManualFix, 0, Some(e.to_string()))
        }
        Err(e) => report(FixStatus::Failed, 0, Some(e.to_string())),
    }
}

/// Whether the file's owner-write bit is off. On Unix that is
/// `mode & 0o200 == 0`; elsewhere the platform's read-only attribute.
fn owner_write_off(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o200 == 0
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly()
    }
}

fn abs_path(id: &str, roots: &HashMap<String, String>) -> Option<PathBuf> {
    let (source, rel_path) = id.strip_prefix('@')?.split_once(':')?;
    Some(Path::new(roots.get(source)?).join(rel_path))
}
