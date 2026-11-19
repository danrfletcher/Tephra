//! What the fix log already says about a rename (G4, FR7): the one
//! decision both signals go through, the exact `link.rename_event` and
//! the watcher's own pairing, so the same rename seen twice, late or out
//! of order is fixed once.
//!
//! [`decide`] is a pure function over the log (as [`LogRec`]s, oldest
//! first) and a question about the disk (`exists`). It reads no file and
//! opens no cache, so every state can be tabled in a unit test.
//!
//! The log is read as a story about one name at a time. The last entry
//! that touches the event's old name says where that name stands:
//!
//! - It left the name (`A to B`): the file already went from A; the log
//!   chain `A to B to C ...` says where it is by now. An event whose new
//!   path is anywhere on that chain is answered `already_applied`. One
//!   that names a different target disagrees with an earlier entry (a
//!   hash guess, or an earlier event): the exact event wins
//!   ([`Decision::Override`]) when it comes from a client, and is
//!   ignored when it comes from the watcher (a guess never beats an
//!   exact event).
//! - It arrived at the name (`X to A`): the file was at A after the last
//!   fix, so a rename away from A now is a new one. This is what makes
//!   `Plan to Training Plan to Plan to Training Plan` fix again instead
//!   of answering `already_applied` (E20).
//! - Nothing touches it: a new rename.
//!
//! The new target is followed through the log when it is no longer on
//! disk (it moved again: A to B, B to C, late event A to B), so a fix
//! never writes a path that is not there, and a target that cannot be
//! found is `nothing_to_fix`.

use std::collections::HashSet;

/// The most renames a chain is followed through (as in `pending`).
pub const MAX_CHAIN_HOPS: usize = 32;

/// One `rename` entry of the log, in one source. Entries of kind
/// `uncertain` and entries that cross sources are not part of a chain
/// and are left out by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRec {
    /// The entry's id; later is greater.
    pub id: i64,
    pub old: String,
    pub new: String,
}

impl LogRec {
    pub fn new(id: i64, old: &str, new: &str) -> Self {
        LogRec {
            id,
            old: old.to_string(),
            new: new.to_string(),
        }
    }
}

/// Why nothing is to be fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NothingReason {
    /// The old and the new path are the same.
    SamePath,
    /// Something is at the old path now: the links to it are right as
    /// they are (E22).
    OldNameInUse,
    /// The new target is not on disk and the log does not say where it
    /// went.
    TargetGone,
    /// The event is the watcher's guess and the log already holds a
    /// different target for the old path (an exact event beats it).
    LogHoldsAnotherTarget,
}

/// What to do about one rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The log already has this rename (or it went on along a logged
    /// chain): answer from the log, write nothing.
    AlreadyApplied,
    NothingToFix(NothingReason),
    /// Fix the links to the old path so they name `target`. `target`
    /// is the event's new path, or where the log says that went on to
    /// (`via` lists the renames, `[new, ..., target]`, empty when
    /// `target` is the event's own).
    Apply {
        target: String,
        via: Vec<String>,
    },
    /// The exact event disagrees with the entry `guess_id`, which sent
    /// the old path to `guess_target` (where the log chain ends and
    /// the file is). Fix as for [`Decision::Apply`], and also point the
    /// links that entry wrote at `guess_target` to `target`.
    Override {
        target: String,
        via: Vec<String>,
        guess_id: i64,
        guess_target: String,
    },
}

/// The last entry that touches `name`.
fn last_touching<'a>(log: &'a [LogRec], name: &str) -> Option<&'a LogRec> {
    log.iter()
        .filter(|r| r.old == name || r.new == name)
        .max_by_key(|r| r.id)
}

/// Whether the log says the file that was at `name` has left it: the
/// last entry touching the name is a rename away from it. A watcher that
/// sees `name` vanish with no pair has nothing to report then: an exact
/// event moved it already.
pub fn departed(log: &[LogRec], name: &str) -> bool {
    matches!(last_touching(log, name), Some(r) if r.old == name && r.new != name)
}

/// Where the log says the file that was at `start` is now: `start`
/// first, then each rename away from it, while the latest entry about
/// the name is a departure. A loop or a chain beyond
/// [`MAX_CHAIN_HOPS`] stops where it is.
pub fn follow(log: &[LogRec], start: &str) -> Vec<String> {
    let mut chain = vec![start.to_string()];
    let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
    let mut cur = start.to_string();
    for _ in 0..MAX_CHAIN_HOPS {
        match last_touching(log, &cur) {
            Some(r) if r.old == cur && r.new != cur => {
                cur = r.new.clone();
                if !seen.insert(cur.clone()) {
                    break;
                }
                chain.push(cur.clone());
            }
            _ => break,
        }
    }
    chain
}

/// Where a link to `new` has to point: `new` itself when it is on disk,
/// else the end of the log chain from it when that is. `None` when the
/// target cannot be found. The second value is the chain walked
/// (empty when `new` is on disk).
fn resolve_target(
    log: &[LogRec],
    new: &str,
    exists: &dyn Fn(&str) -> bool,
) -> Option<(String, Vec<String>)> {
    if exists(new) {
        return Some((new.to_string(), Vec::new()));
    }
    let chain = follow(log, new);
    let end = chain.last().expect("a chain has its start");
    if end != new && exists(end) {
        return Some((end.clone(), chain));
    }
    None
}

/// Decides one rename of `old` to `new` inside one source.
///
/// - `log`: that source's `rename` entries, oldest first.
/// - `exact`: `true` for a client's `link.rename_event`, `false` for
///   the watcher's pairing (which never overrides what the log says).
/// - `exists`: whether a source-relative path is on disk now.
pub fn decide(
    log: &[LogRec],
    old: &str,
    new: &str,
    exact: bool,
    exists: &dyn Fn(&str) -> bool,
) -> Decision {
    if old == new {
        return Decision::NothingToFix(NothingReason::SamePath);
    }
    if exists(old) {
        return Decision::NothingToFix(NothingReason::OldNameInUse);
    }

    let apply = |target_info: Option<(String, Vec<String>)>| match target_info {
        Some((target, via)) => Decision::Apply { target, via },
        None => Decision::NothingToFix(NothingReason::TargetGone),
    };

    match last_touching(log, old) {
        // The file left `old` already: follow it.
        Some(departure) if departure.old == old => {
            let chain = follow(log, &departure.new);
            // `new` is on the chain from where the log sent `old`, or
            // the log sends `new` on to where it sent `old` (a late
            // A to B that was fixed as A to C when B had moved to C).
            if departure.new == new
                || chain.iter().any(|p| p == new)
                || follow(log, new).contains(&departure.new)
            {
                return Decision::AlreadyApplied;
            }
            // The log sent it somewhere else.
            if !exact {
                return Decision::NothingToFix(NothingReason::LogHoldsAnotherTarget);
            }
            let guess_target = chain.last().expect("a chain has its start").clone();
            match resolve_target(log, new, exists) {
                Some((target, via)) if exists(&guess_target) && guess_target != target => {
                    Decision::Override {
                        target,
                        via,
                        guess_id: departure.id,
                        guess_target,
                    }
                }
                other => apply(other),
            }
        }
        // `old` was where the file was after the last fix, or the log
        // has never heard of it: a new rename.
        _ => apply(resolve_target(log, new, exists)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_disk<'a>(paths: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |p| paths.contains(&p)
    }

    #[test]
    fn a_repeat_of_a_logged_rename_is_already_applied() {
        let log = [LogRec::new(1, "A.md", "B.md")];
        let d = decide(&log, "A.md", "B.md", true, &on_disk(&["B.md"]));
        assert_eq!(d, Decision::AlreadyApplied);
    }

    #[test]
    fn follow_stops_at_a_loop() {
        let log = [LogRec::new(1, "A", "B"), LogRec::new(2, "B", "A")];
        // Latest about A is the arrival (2): the chain from A stops.
        assert_eq!(follow(&log, "A"), vec!["A"]);
    }
}
