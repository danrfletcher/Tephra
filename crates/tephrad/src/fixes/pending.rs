//! The intent re-checks a pending fix goes through before every retry
//! (G9). A pending row says "this file named `old`, and should name
//! `new`"; it says nothing about whether that is still true. Before the
//! write loop is asked to touch the file, the row is checked against
//! the graph as it is now and the fix log:
//!
//! 1. Was the old name reused? A new file now at the old path means the
//!    links to it are right as they are. Dropped as `nothing_to_fix`,
//!    never rewritten to the wrong file.
//! 2. Did an exact event correct the target? A later `rename` entry
//!    from the same old path to a different new one cancels the row
//!    before it writes.
//! 3. Did the target move or vanish? A chain A to B to C resolves to C:
//!    the row is re-pointed. A target that is gone with no logged
//!    rename (renamed or deleted again while Tephra was down) is
//!    dropped as `nothing_to_fix`.
//!
//! The checks read the graph and the log only; no file is opened.

use tephra_cache::{Cache, PendingFixRow};
use tephra_graph::UnitGraph;

use super::log::{FILE_CANCELLED, FILE_NEEDS_MANUAL_FIX, FILE_NOTHING_TO_FIX};
use super::FixError;

/// The most renames a chain is followed through. A longer one, or a
/// loop in a damaged log, is dropped rather than followed forever.
pub const MAX_CHAIN_HOPS: usize = 32;

/// What the re-checks decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Write, with this target. `chain` is the renames from the old
    /// path to it (`[A, B, C]`; `[A, B]` when the target did not move).
    Proceed {
        source: String,
        path: String,
        chain: Vec<String>,
    },
    /// Do not write: the row is finished with this file status
    /// (`nothing_to_fix`, `cancelled` or `needs_manual_fix`) and reason.
    Finish {
        status: &'static str,
        reason: String,
    },
}

fn unit_id(source: &str, path: &str) -> String {
    format!("@{source}:{path}")
}

fn finish(status: &'static str, reason: impl Into<String>) -> Verdict {
    Verdict::Finish {
        status,
        reason: reason.into(),
    }
}

/// Runs the re-checks for `row` against `graph` (the graph as it is
/// now) and the log in `cache`.
pub fn recheck(cache: &Cache, graph: &UnitGraph, row: &PendingFixRow) -> Result<Verdict, FixError> {
    let (Some(old_source), Some(old_path), Some(new_source), Some(new_path)) = (
        row.old_source.as_deref(),
        row.old_path.as_deref(),
        row.new_source.as_deref(),
        row.new_path.as_deref(),
    ) else {
        return Ok(finish(
            FILE_NOTHING_TO_FIX,
            "the stored fix has no old or new target",
        ));
    };

    // 1. The old name was reused.
    if graph.unit(&unit_id(old_source, old_path)).is_some() {
        return Ok(finish(
            FILE_NOTHING_TO_FIX,
            format!("the old name {old_path} is now used by another file"),
        ));
    }

    // 2. A later exact event named a different target for the same old
    // path. (The same pair logged twice is not a correction.)
    let after = row.fix_id.unwrap_or(0);
    for later in cache.fix_renames_from(old_source, old_path, after)? {
        let same = later.new_source.as_deref() == Some(new_source)
            && later.new_path.as_deref() == Some(new_path);
        if !same {
            return Ok(finish(
                FILE_CANCELLED,
                format!(
                    "a later rename event gave {} as the target of {old_path}",
                    later.new_path.as_deref().unwrap_or("another file")
                ),
            ));
        }
    }

    // 3. Follow the target through later renames until it is a unit.
    let mut source = new_source.to_string();
    let mut path = new_path.to_string();
    let mut chain = vec![old_path.to_string(), new_path.to_string()];
    let mut after = after;
    for _ in 0..=MAX_CHAIN_HOPS {
        if graph.unit(&unit_id(&source, &path)).is_some() {
            if source != old_source {
                // Rewriting a link across sources is not done by this
                // writer: it is left for a person.
                return Ok(finish(
                    FILE_NEEDS_MANUAL_FIX,
                    format!("the target moved to source {source}: links across sources are not rewritten"),
                ));
            }
            return Ok(Verdict::Proceed {
                source,
                path,
                chain,
            });
        }
        let next = cache
            .fix_renames_from(&source, &path, after)?
            .into_iter()
            .find(|e| e.new_path.is_some());
        match next {
            Some(e) => {
                after = e.id;
                source = e.new_source.clone().unwrap_or(source);
                path = e.new_path.clone().unwrap_or(path);
                chain.push(path.clone());
            }
            None => {
                return Ok(finish(
                    FILE_NOTHING_TO_FIX,
                    format!("the target {path} was renamed or deleted again"),
                ))
            }
        }
    }
    Ok(finish(
        FILE_NOTHING_TO_FIX,
        format!("the chain of renames from {old_path} is longer than {MAX_CHAIN_HOPS}"),
    ))
}
