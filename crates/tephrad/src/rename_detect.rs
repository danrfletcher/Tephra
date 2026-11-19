//! Telling a rename from a remove plus a create (G1, G2), and handing a
//! certain one to the automatic link fix.
//!
//! The watcher reports what a batch did to the index: files that
//! vanished (a cached row, no file) and files that appeared (a file, no
//! cached row). This module decides which of those are the same file.
//! It reads three facts and nothing else (F5):
//!
//! 1. the OS's own pairing: both paths carry the rename flag (FSEvents)
//!    or share an inotify cookie, AND the new file's inode and volume
//!    are the vanished row's ([`OsMark`], [`Identity`]);
//! 2. the file identity itself (part of 1);
//! 3. an exact content hash: exactly one vanished and one new file in
//!    ALL the sources together share it (FR21); or, when several share
//!    it inside one source, the path under a renamed folder (`Old/a.md`
//!    and `New/a.md`) names the pair.
//!
//! The pool of what is not paired yet spans every registered source
//! (G6): a file that vanished from one source and a file with the same
//! hash that appeared in another are one `move`, never a delete and a
//! create. The facts carry the name of their source.
//!
//! There is no similarity, edit distance or guessing. A pair by 1 is
//! certain with no wait. A pair by 3 alone ([`Basis::Hash`]) waits a
//! hold ([`crate::watcher::RENAME_HOLD`]) in case another twin turns
//! up, in any source; so does a pair by the relative path under a
//! folder. Where 1 and 3 disagree, 1 wins: a renamed and edited file
//! has a new hash and the OS still says it is the same file.
//!
//! Three layers, each testable alone:
//!
//! - [`pair`]: the pure rule. Facts in, pairs out. No filesystem, no
//!   clock.
//! - [`Detector`]: the pairing rule over time. It keeps what is not
//!   paired yet, applies a hash pair once its hold has passed and gives
//!   an unpaired file up when its own hold ends. It never reads a clock:
//!   callers pass `now`.
//! - [`run_holds`]: the task that wakes the detector when a hold ends.
//!   Hold timers do not run while a pause or the writer gate is active.
//!
//! [`dispatch`] turns the detector's decisions into calls on a
//! [`RenameFixer`] (in production, [`crate::fixes::FixEngine`]): one fix
//! per file, one fix per renamed folder, one `move` per file that
//! crossed from one source to another, and an `uncertain` log entry for
//! a vanished file that got no certain pair.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// What the OS said about a path in a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OsMark {
    /// FSEvents: the path carries the rename flag. Says nothing about
    /// which other path it pairs with; the file identity decides.
    Flag,
    /// inotify: the path took part in the rename with this cookie. Two
    /// paths with the same cookie are the two ends of one rename.
    Cookie(u64),
}

/// A file's inode and the volume it is on. Unique only as a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

/// A tracked file that is gone: what its cached row said, and what the
/// OS said about its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vanished {
    /// The source it vanished from. Filled in by the registry when the
    /// facts are handed to the pool ([`SourceRenames::on_batch`]); the
    /// code that finds the facts does not know the source's name.
    pub source: String,
    /// Relative to the source's root.
    pub path: String,
    pub identity: Option<Identity>,
    /// `None` for a file over the size cap, or one never hashed.
    pub hash: Option<String>,
    pub os: Option<OsMark>,
}

/// A file that is new to the index: what is on disk now, and what the
/// OS said about its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appeared {
    /// The source it appeared in (see [`Vanished::source`]).
    pub source: String,
    pub path: String,
    pub identity: Option<Identity>,
    /// `None` for a file over the size cap (identity only), or one not
    /// hashed.
    pub hash: Option<String>,
    pub os: Option<OsMark>,
}

/// Why a pair is certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The OS pair and the file identity agree. Applied with no hold.
    Os,
    /// Exactly one vanished and one new file share the hash, or the hash
    /// is shared by several and the relative path under the old and new
    /// folder names the pair. Held.
    Hash,
}

/// One pair, by index into the slices given to [`pair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub vanished: usize,
    pub appeared: usize,
    pub basis: Basis,
}

/// The pairing rule (G1, G6, E3, E4, F5, FR21). Pure: facts in, pairs
/// out. The facts come from every source at once (the pool spans all
/// of them), so a file that vanished from one source and one that
/// appeared in another can pair: that is a move.
///
/// First the OS pass: a vanished file and a new file are a pair when
/// both carry equal [`OsMark`]s (the flag, or the same cookie) and their
/// [`Identity`] is equal. Hashes play no part, so a renamed and edited
/// file pairs. Then the hash pass over what is left: a hash that exactly
/// one vanished and exactly one new file have, counted over ALL sources
/// together, is a pair. Two files with the same hash on either side
/// are ambiguous and pair with nothing, wherever they are: two empty
/// files that vanish in one source and one that appears in another make
/// no pair. Inside one source, files whose paths differ only by the
/// folder prefix are then paired by relative path
/// ([`relative_path_pairs`]); that pass never looks across sources. A
/// pair never has the same source and path on both sides: a path seen
/// gone in one batch and new in a later one is not a rename to itself,
/// so its vanished side stays unpaired and ends up `uncertain`.
pub fn pair(vanished: &[Vanished], appeared: &[Appeared]) -> Vec<Pair> {
    let mut pairs = Vec::new();
    let mut used_v = vec![false; vanished.len()];
    let mut used_a = vec![false; appeared.len()];

    for (vi, v) in vanished.iter().enumerate() {
        let (Some(mark), Some(identity)) = (v.os, v.identity) else {
            continue;
        };
        let found = appeared.iter().enumerate().find(|(ai, a)| {
            !used_a[*ai]
                && !(a.source == v.source && a.path == v.path)
                && a.os == Some(mark)
                && a.identity == Some(identity)
        });
        if let Some((ai, _)) = found {
            used_v[vi] = true;
            used_a[ai] = true;
            pairs.push(Pair {
                vanished: vi,
                appeared: ai,
                basis: Basis::Os,
            });
        }
    }

    let mut by_hash_v: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_hash_a: HashMap<&str, Vec<usize>> = HashMap::new();
    for (vi, v) in vanished.iter().enumerate() {
        if let (false, Some(h)) = (used_v[vi], v.hash.as_deref()) {
            by_hash_v.entry(h).or_default().push(vi);
        }
    }
    for (ai, a) in appeared.iter().enumerate() {
        if let (false, Some(h)) = (used_a[ai], a.hash.as_deref()) {
            by_hash_a.entry(h).or_default().push(ai);
        }
    }
    let mut hash_pairs: Vec<Pair> = by_hash_v
        .iter()
        .filter_map(
            |(h, vs)| match (vs.as_slice(), by_hash_a.get(h)?.as_slice()) {
                // A path that is on both sides (seen gone in one batch
                // and new in a later one) was not renamed to itself.
                ([vi], [ai])
                    if (&vanished[*vi].source, &vanished[*vi].path)
                        != (&appeared[*ai].source, &appeared[*ai].path) =>
                {
                    Some(Pair {
                        vanished: *vi,
                        appeared: *ai,
                        basis: Basis::Hash,
                    })
                }
                _ => None,
            },
        )
        .collect();
    hash_pairs.sort_by_key(|p| p.vanished);
    for p in &hash_pairs {
        used_v[p.vanished] = true;
        used_a[p.appeared] = true;
    }
    pairs.extend(hash_pairs);

    pairs.extend(relative_path_pairs(vanished, appeared, &used_v, &used_a));
    pairs
}

/// How many trailing path names `from` and `to` share, the file name
/// being the first, counting only runs that leave a folder name in front
/// on each side with the two folder prefixes different (`Old/a.md` and
/// `New/a.md` share 1; `Old/s/a.md` and `New/s/a.md` share 2). 0 when
/// there is no such run.
fn shared_tail(from: &str, to: &str) -> usize {
    let f: Vec<&str> = from.split('/').collect();
    let t: Vec<&str> = to.split('/').collect();
    let mut run = 0;
    while run + 1 < f.len() && run + 1 < t.len() && f[f.len() - 1 - run] == t[t.len() - 1 - run] {
        run += 1;
    }
    if run == 0 || f[..f.len() - run] == t[..t.len() - run] {
        return 0;
    }
    run
}

/// The third pass of [`pair`] (G1 folder line, E2): what the hash pass
/// found ambiguous, the relative path settles. A folder renamed with no
/// OS pair (a `git checkout`: new inodes, no rename flag) holds files
/// that may share a hash, such as two empty notes or two copies of one
/// template. A vanished and a new file with the same hash whose paths
/// differ only by the folder prefix (`Old/a.md`, `New/a.md`) are a pair
/// when each is the other's one best match: the same hash and the
/// longest shared relative path. A tie on either side pairs nothing.
fn relative_path_pairs(
    vanished: &[Vanished],
    appeared: &[Appeared],
    used_v: &[bool],
    used_a: &[bool],
) -> Vec<Pair> {
    // (vanished, appeared, shared length) for every candidate.
    let mut cands: Vec<(usize, usize, usize)> = Vec::new();
    for (vi, v) in vanished.iter().enumerate() {
        let Some(h) = v.hash.as_deref().filter(|_| !used_v[vi]) else {
            continue;
        };
        for (ai, a) in appeared.iter().enumerate() {
            // A folder is renamed inside its own source: a pair across
            // sources is a move of one file, never a folder.
            if used_a[ai] || a.source != v.source || a.hash.as_deref() != Some(h) {
                continue;
            }
            let shared = shared_tail(&v.path, &a.path);
            if shared > 0 {
                cands.push((vi, ai, shared));
            }
        }
    }
    // The one candidate with the longest shared path among `group`, or
    // none when the longest is shared by two.
    fn unique_best(group: &[&(usize, usize, usize)]) -> Option<(usize, usize)> {
        let top = group.iter().map(|c| c.2).max()?;
        let mut at_top = group.iter().filter(|c| c.2 == top);
        let first = at_top.next()?;
        at_top.next().is_none().then_some((first.0, first.1))
    }
    let mut out: Vec<Pair> = Vec::new();
    for &(vi, ai, _) in &cands {
        let of_v: Vec<_> = cands.iter().filter(|c| c.0 == vi).collect();
        let of_a: Vec<_> = cands.iter().filter(|c| c.1 == ai).collect();
        if unique_best(&of_v) == Some((vi, ai)) && unique_best(&of_a) == Some((vi, ai)) {
            out.push(Pair {
                vanished: vi,
                appeared: ai,
                basis: Basis::Hash,
            });
        }
    }
    out.sort_by_key(|p| p.vanished);
    out
}

// ---------------------------------------------------------------
// Holding what is not paired yet
// ---------------------------------------------------------------

/// A certain rename, or, when `source` and `to_source` differ, a move
/// from one source to another (G6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    /// The source the file left.
    pub source: String,
    pub from: String,
    /// The source the file is in now.
    pub to_source: String,
    pub to: String,
    pub basis: Basis,
}

impl Rename {
    /// Whether the file crossed from one source to another.
    pub fn is_move(&self) -> bool {
        self.source != self.to_source
    }
}

/// What [`Detector`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decisions {
    /// Certain renames, ready to fix.
    pub renames: Vec<Rename>,
    /// Vanished files whose hold ended with no certain pair: a plain
    /// delete, logged `uncertain`.
    pub gave_up_vanished: Vec<Vanished>,
    /// New files whose hold ended with no pair: a plain create.
    pub gave_up_appeared: Vec<Appeared>,
}

impl Decisions {
    pub fn is_empty(&self) -> bool {
        self.renames.is_empty()
            && self.gave_up_vanished.is_empty()
            && self.gave_up_appeared.is_empty()
    }
}

/// The files of every source that are not paired yet, and when each was
/// first seen: the pending-delete pool, which spans all sources (G6).
/// Never reads a clock: every call takes `now`.
///
/// - An OS pair is decided the moment both ends are known.
/// - A hash pair is decided `hold` after the later of its two ends was
///   first seen, if it is still the only pair for its hash then.
/// - A file with no pair is given up `hold` after it was first seen. A
///   twin that turns up later finds nothing to pair with: a plain
///   create, no late fix.
/// - While frozen ([`Detector::freeze`]) nothing is given up and no
///   hash pair ripens; [`Detector::thaw`] gives every file the time it
///   lost, so a long write cannot expire a hold.
pub struct Detector {
    hold: Duration,
    vanished: Vec<Vanished>,
    vanished_since: Vec<Instant>,
    appeared: Vec<Appeared>,
    appeared_since: Vec<Instant>,
    frozen_at: Option<Instant>,
}

impl Detector {
    pub fn new(hold: Duration) -> Self {
        Detector {
            hold,
            vanished: Vec::new(),
            vanished_since: Vec::new(),
            appeared: Vec::new(),
            appeared_since: Vec::new(),
            frozen_at: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.vanished.is_empty() && self.appeared.is_empty()
    }

    /// Whether a vanished file is waiting for a twin.
    pub fn has_vanished(&self) -> bool {
        !self.vanished.is_empty()
    }

    /// Sets the hold for what is ingested from now on and for what is
    /// held already (a hold is measured from when a file was first seen,
    /// so a longer hold lengthens every wait).
    pub fn set_hold(&mut self, hold: Duration) {
        self.hold = hold;
    }

    /// Drops everything held for `source`: it was unregistered, so
    /// nothing in it is gone or new any more.
    pub fn forget_source(&mut self, source: &str) {
        let take_v: HashSet<usize> = (0..self.vanished.len())
            .filter(|&i| self.vanished[i].source == source)
            .collect();
        let take_a: HashSet<usize> = (0..self.appeared.len())
            .filter(|&i| self.appeared[i].source == source)
            .collect();
        retain_not(&mut self.vanished, &mut self.vanished_since, &take_v);
        retain_not(&mut self.appeared, &mut self.appeared_since, &take_a);
    }

    /// Takes in what one batch found and decides what can be decided.
    pub fn ingest(
        &mut self,
        now: Instant,
        vanished: Vec<Vanished>,
        appeared: Vec<Appeared>,
    ) -> Decisions {
        let seen = self.frozen_at.unwrap_or(now);
        for v in vanished {
            // The same path vanishing again replaces what was held.
            if let Some(i) = self
                .vanished
                .iter()
                .position(|h| h.source == v.source && h.path == v.path)
            {
                self.vanished[i] = v;
                self.vanished_since[i] = seen;
                continue;
            }
            self.vanished.push(v);
            self.vanished_since.push(seen);
        }
        for a in appeared {
            if let Some(i) = self
                .appeared
                .iter()
                .position(|h| h.source == a.source && h.path == a.path)
            {
                self.appeared[i] = a;
                self.appeared_since[i] = seen;
                continue;
            }
            self.appeared.push(a);
            self.appeared_since.push(seen);
        }
        self.settle(now)
    }

    /// Decides what a passing hold has made decidable.
    pub fn expire(&mut self, now: Instant) -> Decisions {
        if self.frozen_at.is_some() {
            return Decisions::default();
        }
        self.settle(now)
    }

    /// When [`Detector::expire`] next has something to decide, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        let pairs = pair(&self.vanished, &self.appeared);
        let mut in_pair_v = HashSet::new();
        let mut in_pair_a = HashSet::new();
        let mut deadline: Option<Instant> = None;
        let mut soonest = |t: Instant| {
            deadline = Some(deadline.map_or(t, |d| d.min(t)));
        };
        for p in &pairs {
            in_pair_v.insert(p.vanished);
            in_pair_a.insert(p.appeared);
            if p.basis == Basis::Hash {
                soonest(self.ripe_at(p));
            }
        }
        for (i, since) in self.vanished_since.iter().enumerate() {
            if !in_pair_v.contains(&i) {
                soonest(*since + self.hold);
            }
        }
        for (i, since) in self.appeared_since.iter().enumerate() {
            if !in_pair_a.contains(&i) {
                soonest(*since + self.hold);
            }
        }
        deadline
    }

    /// Stops the hold timers at `now` (a pause or the writer gate is
    /// active). Idempotent.
    pub fn freeze(&mut self, now: Instant) {
        self.frozen_at.get_or_insert(now);
    }

    /// Starts them again at `now`: every file is given back the time it
    /// spent frozen. Idempotent.
    pub fn thaw(&mut self, now: Instant) {
        let Some(at) = self.frozen_at.take() else {
            return;
        };
        let lost = now.saturating_duration_since(at);
        for t in self
            .vanished_since
            .iter_mut()
            .chain(self.appeared_since.iter_mut())
        {
            *t += lost;
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen_at.is_some()
    }

    fn ripe_at(&self, p: &Pair) -> Instant {
        self.vanished_since[p.vanished].max(self.appeared_since[p.appeared]) + self.hold
    }

    fn settle(&mut self, now: Instant) -> Decisions {
        let now = self.frozen_at.unwrap_or(now);
        let pairs = pair(&self.vanished, &self.appeared);
        let mut out = Decisions::default();
        let mut take_v: HashSet<usize> = HashSet::new();
        let mut take_a: HashSet<usize> = HashSet::new();
        let mut protected_v: HashSet<usize> = HashSet::new();
        let mut protected_a: HashSet<usize> = HashSet::new();

        for p in &pairs {
            let ripe = match p.basis {
                Basis::Os => true,
                Basis::Hash => now >= self.ripe_at(p),
            };
            if ripe {
                take_v.insert(p.vanished);
                take_a.insert(p.appeared);
                out.renames.push(Rename {
                    source: self.vanished[p.vanished].source.clone(),
                    from: self.vanished[p.vanished].path.clone(),
                    to_source: self.appeared[p.appeared].source.clone(),
                    to: self.appeared[p.appeared].path.clone(),
                    basis: p.basis,
                });
            } else {
                // A pair still waiting out its hold is not given up
                // when either end's own hold would have ended.
                protected_v.insert(p.vanished);
                protected_a.insert(p.appeared);
            }
        }
        for i in 0..self.vanished.len() {
            if !take_v.contains(&i)
                && !protected_v.contains(&i)
                && now >= self.vanished_since[i] + self.hold
            {
                take_v.insert(i);
                out.gave_up_vanished.push(self.vanished[i].clone());
            }
        }
        for i in 0..self.appeared.len() {
            if !take_a.contains(&i)
                && !protected_a.contains(&i)
                && now >= self.appeared_since[i] + self.hold
            {
                take_a.insert(i);
                out.gave_up_appeared.push(self.appeared[i].clone());
            }
        }
        retain_not(&mut self.vanished, &mut self.vanished_since, &take_v);
        retain_not(&mut self.appeared, &mut self.appeared_since, &take_a);
        out
    }
}

fn retain_not<T>(items: &mut Vec<T>, since: &mut Vec<Instant>, take: &HashSet<usize>) {
    if take.is_empty() {
        return;
    }
    let mut i = 0;
    items.retain(|_| {
        let keep = !take.contains(&i);
        i += 1;
        keep
    });
    let mut i = 0;
    since.retain(|_| {
        let keep = !take.contains(&i);
        i += 1;
        keep
    });
}

/// How often [`run_holds`] looks at the block state while it holds
/// anything. A pause or the writer gate can start at any moment and
/// nothing wakes the task for it, so a hold is never slept through in
/// one go: the time before a block is noticed is at most this long, and
/// is the only time a block can fail to stop the clock.
pub const BLOCKED_POLL: Duration = Duration::from_millis(50);

/// Wakes `detector` when a hold ends and hands what it decides to
/// `on_decisions`. Runs until aborted.
///
/// `wake` is notified by whoever ingests new files, so a hold that is
/// shorter than the one being waited on is not missed. While `blocked`
/// says a pause or the writer gate is active the detector is frozen and
/// no hold runs down. The task looks at `blocked` at least every
/// [`BLOCKED_POLL`] while it holds anything.
pub async fn run_holds<B, D>(
    detector: Arc<Mutex<Detector>>,
    wake: Arc<Notify>,
    blocked: B,
    on_decisions: D,
) where
    B: Fn() -> bool,
    D: Fn(Decisions),
{
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let is_blocked = blocked();
        let now = Instant::now();
        let wait = {
            let mut d = detector.lock().unwrap_or_else(|p| p.into_inner());
            if is_blocked {
                d.freeze(now);
                (!d.is_empty()).then_some(now + BLOCKED_POLL)
            } else {
                d.thaw(now);
                d.next_deadline()
                    .map(|deadline| deadline.min(now + BLOCKED_POLL))
            }
        };
        match wait {
            None => notified.await,
            Some(at) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {}
                    _ = &mut notified => {}
                }
            }
        }
        if blocked() {
            continue;
        }
        let decisions = {
            let mut d = detector.lock().unwrap_or_else(|p| p.into_inner());
            d.thaw(Instant::now());
            d.expire(Instant::now())
        };
        if !decisions.is_empty() {
            on_decisions(decisions);
        }
    }
}

// ---------------------------------------------------------------
// Folders
// ---------------------------------------------------------------

/// What to fix: one file, or a renamed folder with the files it moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    File(Rename),
    Folder {
        old_dir: String,
        new_dir: String,
        files: Vec<Rename>,
    },
}

/// The folder pairs a file move can be part of, shortest folder first:
/// the file name is unchanged and everything under the pair's folders
/// (the relative path) is the same on both sides. `A/B/x.md` to
/// `A/C/x.md` gives `(A/B, A/C)`; `Notes/2024/a.md` to
/// `Archive/2024/a.md` gives `(Notes, Archive)` and then
/// `(Notes/2024, Archive/2024)`. Which one is the folder that was
/// renamed is for the caller to say: the first whose old folder is gone.
fn folder_candidates(from: &str, to: &str) -> Vec<(String, String)> {
    let f: Vec<&str> = from.split('/').collect();
    let t: Vec<&str> = to.split('/').collect();
    let mut out = Vec::new();
    if f.last() != t.last() {
        return out;
    }
    // `run` trailing names are shared, the file name being the first;
    // each side keeps at least one folder name in front of them.
    let mut run = 0;
    while run < f.len() && run < t.len() && f[f.len() - 1 - run] == t[t.len() - 1 - run] {
        run += 1;
    }
    for shared in (1..=run).rev() {
        let (old_dir, new_dir) = (&f[..f.len() - shared], &t[..t.len() - shared]);
        if !old_dir.is_empty() && !new_dir.is_empty() && old_dir != new_dir {
            out.push((old_dir.join("/"), new_dir.join("/")));
        }
    }
    out
}

/// Groups file renames that are one folder renamed (G1): at least two
/// files that moved from the same old folder to the same new one with
/// their names under it unchanged. The old folder is the shortest one
/// that `gone` says is no longer there, so `mv Notes/2024 Archive/2024`
/// is the folder `Notes/2024` even while `Notes` stays, and two files
/// moved out of a folder that is still there are not a folder rename.
/// Everything else stays a file rename. Order follows the first file of
/// each group.
pub fn group_folders(renames: Vec<Rename>, gone: impl Fn(&str) -> bool) -> Vec<Change> {
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, r) in renames.iter().enumerate() {
        if let Some(key) = folder_candidates(&r.from, &r.to)
            .into_iter()
            .find(|(old_dir, _)| gone(old_dir))
        {
            groups.entry(key).or_default().push(i);
        }
    }
    let mut folder_of_index: HashMap<usize, (String, String)> = HashMap::new();
    for (key, idxs) in &groups {
        if idxs.len() >= 2 {
            for i in idxs {
                folder_of_index.insert(*i, key.clone());
            }
        }
    }
    let mut out: Vec<Change> = Vec::new();
    let mut at: HashMap<(String, String), usize> = HashMap::new();
    for (i, r) in renames.into_iter().enumerate() {
        match folder_of_index.get(&i) {
            None => out.push(Change::File(r)),
            Some(key) => match at.get(key) {
                Some(&slot) => {
                    if let Change::Folder { files, .. } = &mut out[slot] {
                        files.push(r);
                    }
                }
                None => {
                    at.insert(key.clone(), out.len());
                    out.push(Change::Folder {
                        old_dir: key.0.clone(),
                        new_dir: key.1.clone(),
                        files: vec![r],
                    });
                }
            },
        }
    }
    out
}

// ---------------------------------------------------------------
// Handing decisions on
// ---------------------------------------------------------------

/// What [`dispatch`] calls. In production this is
/// [`crate::fixes::FixEngine`]; a test can stand in for it.
pub trait RenameFixer: Send + Sync {
    /// Fixes the links to `old` after it was renamed to `new`, and logs
    /// one entry.
    fn fix_rename(&self, source: &str, old: &str, new: &str) -> Result<(), String>;
    /// The same for a folder: `moves` are the `(old, new)` paths of its
    /// files. One entry with the folder's own old and new path.
    fn fix_folder_rename(
        &self,
        source: &str,
        old_dir: &str,
        new_dir: &str,
        moves: &[(String, String)],
    ) -> Result<(), String>;
    /// Fixes the links to `old` of `old_source` after the file moved to
    /// `new` of `new_source` (G6): the links in the old source, the links
    /// in the new one and the moved file's own. One entry of kind `move`.
    fn fix_move(
        &self,
        old_source: &str,
        old: &str,
        new_source: &str,
        new: &str,
    ) -> Result<(), String>;
    /// Logs a file that vanished with no certain pair (G10).
    fn record_uncertain(&self, source: &str, path: &str) -> Result<(), String>;
    /// [`dispatch`] calls this before the first fix of a batch and
    /// [`RenameFixer::finish_batch`] after the last, on the same thread,
    /// so a fixer can tell the clients of the whole batch at once.
    fn start_batch(&self) {}
    /// See [`RenameFixer::start_batch`].
    fn finish_batch(&self) {}
}

/// Ends the batch [`dispatch`] opened, however it returns.
struct BatchGuard<'a>(&'a dyn RenameFixer);

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        self.0.finish_batch();
    }
}

/// Where a source's root is, by name. `None` for a source that is not
/// registered (any more).
pub type RootOf<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// Fixes every certain rename in `decisions` and logs every vanished
/// file that was given up. A folder is fixed as one when its old path
/// is really gone from disk ([`group_folders`]); otherwise (two files
/// moved out of a folder that is still there) each file is fixed on its
/// own. A file that went from one source to another is a move
/// ([`RenameFixer::fix_move`]), one fix per file; a folder is only ever
/// renamed inside its source.
///
/// A rename whose old and new path are equal is no rename ([`pair`]
/// never makes one). If one arrives anyway it is not fixed; when the
/// path is gone from disk it is logged `uncertain` like any vanished
/// file with no pair.
pub fn dispatch(fixer: &dyn RenameFixer, root_of: RootOf<'_>, decisions: Decisions) {
    fixer.start_batch();
    let _batch = BatchGuard(fixer);
    let (selfs, rest): (Vec<Rename>, Vec<Rename>) = decisions
        .renames
        .into_iter()
        .partition(|r| !r.is_move() && r.from == r.to);
    for r in selfs {
        let gone = root_of(&r.source).is_some_and(|root| !root.join(&r.from).exists());
        if gone {
            if let Err(e) = fixer.record_uncertain(&r.source, &r.from) {
                tracing::warn!(source = %r.source, path = %r.from, error = %e, "could not log an uncertain vanish");
            }
        }
    }
    let (moves, renames): (Vec<Rename>, Vec<Rename>) = rest.into_iter().partition(Rename::is_move);

    // The renames of each source, in the order they were decided.
    let mut by_source: Vec<(String, Vec<Rename>)> = Vec::new();
    for r in renames {
        match by_source.iter_mut().find(|(s, _)| *s == r.source) {
            Some((_, list)) => list.push(r),
            None => by_source.push((r.source.clone(), vec![r])),
        }
    }
    for (source, list) in by_source {
        let root = root_of(&source);
        let gone = |dir: &str| root.as_ref().is_none_or(|root| !root.join(dir).exists());
        for change in group_folders(list, gone) {
            let result = match change {
                Change::File(r) => fixer.fix_rename(&source, &r.from, &r.to),
                Change::Folder {
                    old_dir,
                    new_dir,
                    files,
                } => {
                    let moves: Vec<(String, String)> = files
                        .iter()
                        .map(|r| (r.from.clone(), r.to.clone()))
                        .collect();
                    fixer.fix_folder_rename(&source, &old_dir, &new_dir, &moves)
                }
            };
            if let Err(e) = result {
                tracing::warn!(source = %source, error = %e, "automatic link fix of a rename failed");
            }
        }
    }
    for m in moves {
        if let Err(e) = fixer.fix_move(&m.source, &m.from, &m.to_source, &m.to) {
            tracing::warn!(from = %m.source, to = %m.to_source, error = %e, "automatic link fix of a move failed");
        }
    }
    for v in decisions.gave_up_vanished {
        if let Err(e) = fixer.record_uncertain(&v.source, &v.path) {
            tracing::warn!(source = %v.source, path = %v.path, error = %e, "could not log an uncertain vanish");
        }
    }
    for a in decisions.gave_up_appeared {
        tracing::debug!(source = %a.source, path = %a.path, "new file had no vanished twin: a plain create");
    }
}

// ---------------------------------------------------------------
// The pool, as the registry holds it
// ---------------------------------------------------------------

/// What every running source shares for rename detection: ONE detector
/// (the pending-delete pool spans all sources, so a delete in one and a
/// create in another can pair, G6), the wake-up its hold task waits on,
/// and where each source is and whether its watcher is paused.
pub struct RenamePool {
    pub detector: Arc<Mutex<Detector>>,
    pub wake: Arc<Notify>,
    roots: Mutex<HashMap<String, PathBuf>>,
    pauses: Mutex<HashMap<String, crate::watcher::WatcherPause>>,
}

impl RenamePool {
    pub fn new(hold: Duration) -> Self {
        RenamePool {
            detector: Arc::new(Mutex::new(Detector::new(hold))),
            wake: Arc::new(Notify::new()),
            roots: Mutex::new(HashMap::new()),
            pauses: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_hold(&self, hold: Duration) {
        self.detector
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set_hold(hold);
    }

    /// A source's watcher started: its root and pause control are known.
    pub fn add_source(&self, source: &str, root: PathBuf, pause: crate::watcher::WatcherPause) {
        self.roots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(source.to_string(), root);
        self.pauses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(source.to_string(), pause);
    }

    /// A source's watcher stopped: what was held for it is dropped.
    pub fn remove_source(&self, source: &str) {
        self.roots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(source);
        self.pauses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(source);
        self.detector
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .forget_source(source);
    }

    pub fn root_of(&self, source: &str) -> Option<PathBuf> {
        self.roots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(source)
            .cloned()
    }

    /// Whether any running source has its watcher paused: no hold runs
    /// down then (a refactor is writing files).
    pub fn any_paused(&self) -> bool {
        self.pauses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|p| p.is_paused())
    }

    /// What the hold task and a batch hand to [`dispatch`].
    pub fn dispatch(&self, fixer: &dyn RenameFixer, decisions: Decisions) {
        dispatch(fixer, &|source| self.root_of(source), decisions);
    }
}

/// One running source's way into the pool.
pub struct SourceRenames {
    pub source: String,
    pub root: PathBuf,
    pub pool: Arc<RenamePool>,
}

impl SourceRenames {
    pub fn new(source: String, root: PathBuf, pool: Arc<RenamePool>) -> Self {
        SourceRenames { source, root, pool }
    }

    /// Takes in what one applied batch found. `marks` are what the OS
    /// said about the paths of that batch ([`crate::watcher::OsHints`]);
    /// each file takes its own, else its folder's. What is certain
    /// already (an OS pair) is fixed before this returns.
    pub fn on_batch(
        &self,
        fixer: &dyn RenameFixer,
        marks: &HashMap<PathBuf, OsMark>,
        mut vanished: Vec<Vanished>,
        mut appeared: Vec<Appeared>,
    ) {
        if vanished.is_empty() && appeared.is_empty() {
            return;
        }
        let mark = |rel: &str| crate::watcher::mark_of(marks, &self.root, &self.root.join(rel));
        for v in &mut vanished {
            v.source = self.source.clone();
            v.os = mark(&v.path);
        }
        for a in &mut appeared {
            a.source = self.source.clone();
            a.os = mark(&a.path);
        }
        let decisions = {
            let mut d = self.pool.detector.lock().unwrap_or_else(|p| p.into_inner());
            d.ingest(Instant::now(), vanished, appeared)
        };
        self.pool.wake.notify_one();
        if !decisions.is_empty() {
            self.pool.dispatch(fixer, decisions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SEC: Duration = Duration::from_secs(1);

    fn id(device: u64, inode: u64) -> Option<Identity> {
        Some(Identity { device, inode })
    }

    fn van(
        path: &str,
        ident: Option<Identity>,
        hash: Option<&str>,
        os: Option<OsMark>,
    ) -> Vanished {
        Vanished {
            source: "notes".to_string(),
            path: path.to_string(),
            identity: ident,
            hash: hash.map(str::to_string),
            os,
        }
    }

    fn app(
        path: &str,
        ident: Option<Identity>,
        hash: Option<&str>,
        os: Option<OsMark>,
    ) -> Appeared {
        Appeared {
            source: "notes".to_string(),
            path: path.to_string(),
            identity: ident,
            hash: hash.map(str::to_string),
            os,
        }
    }

    /// `(from, to, basis)` of every pair.
    fn pairs_of(v: &[Vanished], a: &[Appeared]) -> Vec<(String, String, Basis)> {
        let mut out: Vec<_> = pair(v, a)
            .into_iter()
            .map(|p| {
                (
                    v[p.vanished].path.clone(),
                    a[p.appeared].path.clone(),
                    p.basis,
                )
            })
            .collect();
        out.sort_by(|x, y| x.0.cmp(&y.0));
        out
    }

    struct Case {
        name: &'static str,
        vanished: Vec<Vanished>,
        appeared: Vec<Appeared>,
        want: Vec<(&'static str, &'static str, Basis)>,
    }

    const F: Option<OsMark> = Some(OsMark::Flag);

    fn c(n: u64) -> Option<OsMark> {
        Some(OsMark::Cookie(n))
    }

    #[test]
    fn the_pairing_rule_over_every_case_of_the_spec() {
        let cases = vec![
            Case {
                name: "G1a flag on both and the same inode: a certain pair",
                vanished: vec![van("a.md", id(1, 10), Some("h1"), F)],
                appeared: vec![app("b.md", id(1, 10), Some("h1"), F)],
                want: vec![("a.md", "b.md", Basis::Os)],
            },
            Case {
                name: "G1a cookie shared and the same inode: a certain pair",
                vanished: vec![van("a.md", id(1, 10), Some("h1"), c(7))],
                appeared: vec![app("b.md", id(1, 10), Some("h1"), c(7))],
                want: vec![("a.md", "b.md", Basis::Os)],
            },
            Case {
                name: "E3 rename plus edit: the hash differs, the OS pair and inode agree, fixed",
                vanished: vec![van("a.md", id(1, 10), Some("old"), c(7))],
                appeared: vec![app("b.md", id(1, 10), Some("new"), c(7))],
                want: vec![("a.md", "b.md", Basis::Os)],
            },
            Case {
                name: "E3 git pull: new inode, no flag, differing hash: nothing",
                vanished: vec![van("a.md", id(1, 10), Some("old"), None)],
                appeared: vec![app("a2.md", id(1, 99), Some("new"), None)],
                want: vec![],
            },
            Case {
                name: "G1a the cookies differ and nothing else pairs them: nothing",
                vanished: vec![van("a.md", id(1, 10), Some("old"), c(7))],
                appeared: vec![app("b.md", id(1, 10), Some("new"), c(8))],
                want: vec![],
            },
            Case {
                name: "G1a flagged but another inode, hashes differ: nothing",
                vanished: vec![van("a.md", id(1, 10), Some("old"), F)],
                appeared: vec![app("b.md", id(1, 11), Some("new"), F)],
                want: vec![],
            },
            Case {
                name: "G1a flagged but another volume, hashes differ: nothing",
                vanished: vec![van("a.md", id(1, 10), Some("old"), F)],
                appeared: vec![app("b.md", id(2, 10), Some("new"), F)],
                want: vec![],
            },
            Case {
                name: "G1a no identity (not Unix): the OS pair alone is not enough",
                vanished: vec![van("a.md", None, Some("old"), F)],
                appeared: vec![app("b.md", None, Some("new"), F)],
                want: vec![],
            },
            Case {
                name: "G1b one vanished and one new share a hash: a hash pair",
                vanished: vec![van("a.md", id(1, 10), Some("h1"), None)],
                appeared: vec![app("b.md", id(1, 99), Some("h1"), None)],
                want: vec![("a.md", "b.md", Basis::Hash)],
            },
            Case {
                name: "G1 where the OS pair and the hash disagree, the OS pair wins",
                vanished: vec![van("a.md", id(1, 10), Some("h1"), c(7))],
                appeared: vec![
                    app("edited.md", id(1, 10), Some("h2"), c(7)),
                    app("twin.md", id(1, 55), Some("h1"), None),
                ],
                want: vec![("a.md", "edited.md", Basis::Os)],
            },
            Case {
                name: "G1 flags on many files: the inode says which is which",
                vanished: vec![
                    van("a.md", id(1, 10), Some("x"), F),
                    van("b.md", id(1, 11), Some("y"), F),
                ],
                appeared: vec![
                    app("n2.md", id(1, 11), Some("y2"), F),
                    app("n1.md", id(1, 10), Some("x2"), F),
                ],
                want: vec![("a.md", "n1.md", Basis::Os), ("b.md", "n2.md", Basis::Os)],
            },
            Case {
                name: "E4 two empty files vanish and two appear: ambiguous, nothing",
                vanished: vec![
                    van("a.md", id(1, 1), Some("empty"), None),
                    van("b.md", id(1, 2), Some("empty"), None),
                ],
                appeared: vec![
                    app("c.md", id(1, 3), Some("empty"), None),
                    app("d.md", id(1, 4), Some("empty"), None),
                ],
                want: vec![],
            },
            Case {
                name: "E4 unless an OS pair names which is which",
                vanished: vec![
                    van("a.md", id(1, 1), Some("empty"), c(5)),
                    van("b.md", id(1, 2), Some("empty"), None),
                ],
                appeared: vec![
                    app("c.md", id(1, 1), Some("empty"), c(5)),
                    app("d.md", id(1, 4), Some("empty"), None),
                ],
                want: vec![("a.md", "c.md", Basis::Os), ("b.md", "d.md", Basis::Hash)],
            },
            Case {
                name: "E4 two vanish and one appears with that hash: ambiguous, nothing",
                vanished: vec![
                    van("a.md", id(1, 1), Some("empty"), None),
                    van("b.md", id(1, 2), Some("empty"), None),
                ],
                appeared: vec![app("c.md", id(1, 3), Some("empty"), None)],
                want: vec![],
            },
            Case {
                name: "E5 a copy: the original is still there, so nothing vanished",
                vanished: vec![],
                appeared: vec![app("copy.md", id(1, 77), Some("h1"), None)],
                want: vec![],
            },
            Case {
                name: "E10 rename out of the source: a vanish with no new file",
                vanished: vec![van("a.md", id(1, 10), Some("h1"), c(3))],
                appeared: vec![],
                want: vec![],
            },
            Case {
                name: "E10 rename into the source from outside: a plain create",
                vanished: vec![],
                appeared: vec![app("in.md", id(1, 10), Some("h1"), c(3))],
                want: vec![],
            },
            Case {
                name: "F5 near-identical content, different names, no OS pair: nothing",
                vanished: vec![van("Plan.md", id(1, 10), Some("hash-of-text-v1"), None)],
                appeared: vec![app(
                    "Plan v2.md",
                    id(1, 11),
                    Some("hash-of-text-v1-and-a-dot"),
                    None,
                )],
                want: vec![],
            },
            Case {
                name: "F5 the same name stem, different content: nothing",
                vanished: vec![van("Notes/Plan.md", id(1, 10), Some("h1"), None)],
                appeared: vec![app("Plan.md", id(1, 11), Some("h2"), None)],
                want: vec![],
            },
            Case {
                name: "Added: over the size cap there is no hash, so no hash pair",
                vanished: vec![van("big.bin", id(1, 10), None, None)],
                appeared: vec![app("big2.bin", id(1, 11), None, None)],
                want: vec![],
            },
            Case {
                name: "Added: over the size cap an OS pair still pairs by identity",
                vanished: vec![van("big.bin", id(1, 10), None, F)],
                appeared: vec![app("big2.bin", id(1, 10), None, F)],
                want: vec![("big.bin", "big2.bin", Basis::Os)],
            },
            Case {
                name: "Added: multi-byte names next to the extension pair like any other",
                vanished: vec![van("Notes—v2.md", id(1, 10), Some("h1"), None)],
                appeared: vec![app("Notes—v3.md", id(1, 99), Some("h1"), None)],
                want: vec![("Notes—v2.md", "Notes—v3.md", Basis::Hash)],
            },
        ];
        for case in cases {
            let got = pairs_of(&case.vanished, &case.appeared);
            let want: Vec<(String, String, Basis)> = case
                .want
                .iter()
                .map(|(a, b, k)| (a.to_string(), b.to_string(), *k))
                .collect();
            assert_eq!(got, want, "{}", case.name);
        }
    }

    #[test]
    fn a_path_on_both_sides_is_never_a_pair() {
        // By hash alone.
        let v = vec![van("Near.md", id(1, 1), Some("h"), None)];
        let a = vec![app("Near.md", id(1, 2), Some("h"), None)];
        assert_eq!(pairs_of(&v, &a), vec![], "hash pass");
        // By OS mark and identity.
        let v = vec![van("Out.md", id(1, 1), Some("h"), F)];
        let a = vec![app("Out.md", id(1, 1), Some("h"), F)];
        assert_eq!(pairs_of(&v, &a), vec![], "OS pass");
        // The self path does not block the real pair next to it.
        let v = vec![
            van("Out.md", id(1, 1), Some("h"), F),
            van("a.md", id(1, 2), Some("g"), F),
        ];
        let a = vec![
            app("Out.md", id(1, 1), Some("h"), F),
            app("b.md", id(1, 2), Some("g"), F),
        ];
        assert_eq!(
            pairs_of(&v, &a),
            vec![("a.md".to_string(), "b.md".to_string(), Basis::Os)]
        );
    }

    #[test]
    fn a_folder_of_identical_files_pairs_by_relative_path() {
        let same = Some("empty");
        let v = vec![
            van("Old/a.md", id(1, 1), same, None),
            van("Old/b.md", id(1, 2), same, None),
        ];
        // Listed the other way round: the order of the files decides nothing.
        let a = vec![
            app("New/b.md", id(1, 4), same, None),
            app("New/a.md", id(1, 3), same, None),
        ];
        assert_eq!(
            pairs_of(&v, &a),
            vec![
                ("Old/a.md".into(), "New/a.md".into(), Basis::Hash),
                ("Old/b.md".into(), "New/b.md".into(), Basis::Hash),
            ]
        );
    }

    #[test]
    fn relative_path_pairing_prefers_the_longest_shared_path() {
        let same = Some("tpl");
        let v = vec![
            van("Old/x/a.md", id(1, 1), same, None),
            van("Old/y/a.md", id(1, 2), same, None),
        ];
        let a = vec![
            app("New/y/a.md", id(1, 4), same, None),
            app("New/x/a.md", id(1, 3), same, None),
        ];
        assert_eq!(
            pairs_of(&v, &a),
            vec![
                ("Old/x/a.md".into(), "New/x/a.md".into(), Basis::Hash),
                ("Old/y/a.md".into(), "New/y/a.md".into(), Basis::Hash),
            ]
        );
    }

    #[test]
    fn relative_path_pairing_never_guesses_between_equal_matches() {
        let same = Some("tpl");
        // Same file name in other folders on both sides: every candidate
        // shares just the file name, so nothing names the pair.
        let v = vec![
            van("A/a.md", id(1, 1), same, None),
            van("B/a.md", id(1, 2), same, None),
        ];
        let a = vec![
            app("C/a.md", id(1, 3), same, None),
            app("D/a.md", id(1, 4), same, None),
        ];
        assert!(pairs_of(&v, &a).is_empty());

        // A copy of one file beside a renamed folder: the copy ties with
        // the real one for Old/a.md, so that file is not paired; the
        // other has one match and is.
        let v = vec![
            van("Old/a.md", id(1, 1), same, None),
            van("Old/b.md", id(1, 2), same, None),
        ];
        let a = vec![
            app("New/a.md", id(1, 3), same, None),
            app("New/b.md", id(1, 4), same, None),
            app("Copy/a.md", id(1, 5), same, None),
        ];
        assert_eq!(
            pairs_of(&v, &a),
            vec![("Old/b.md".into(), "New/b.md".into(), Basis::Hash)]
        );
    }

    #[test]
    fn relative_path_pairing_needs_equal_hashes_the_same_file_name_and_a_folder_change() {
        let v = vec![
            van("Old/a.md", id(1, 1), Some("h1"), None),
            van("Old/c.md", id(1, 5), Some("h1"), None),
        ];
        let a = vec![
            // The folder name is the only thing the paths share, not the file name.
            app("New/z.md", id(1, 3), Some("h1"), None),
            // Same name, other content.
            app("New/c.md", id(1, 6), Some("h3"), None),
        ];
        let got = pairs_of(&v, &a);
        assert!(
            got.iter()
                .all(|(f, t, _)| !(f == "Old/a.md" && t == "New/z.md")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .all(|(f, t, _)| !(f == "Old/c.md" && t == "New/c.md")),
            "{got:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_relative_path_pair_is_held_like_any_hash_pair() {
        let mut d = Detector::new(2 * SEC);
        let same = Some("empty");
        let out = d.ingest(
            Instant::now(),
            vec![
                van("Old/a.md", id(1, 1), same, None),
                van("Old/b.md", id(1, 2), same, None),
            ],
            vec![
                app("New/a.md", id(1, 3), same, None),
                app("New/b.md", id(1, 4), same, None),
            ],
        );
        assert!(out.is_empty(), "held, not applied at once");
        tokio::time::advance(2 * SEC + Duration::from_millis(1)).await;
        let out = d.expire(Instant::now());
        assert_eq!(out.renames.len(), 2);
        assert!(out.gave_up_vanished.is_empty() && out.gave_up_appeared.is_empty());
    }

    #[test]
    fn a_file_is_in_at_most_one_pair() {
        let v = vec![
            van("a.md", id(1, 1), Some("h"), F),
            van("b.md", id(1, 1), Some("h"), F),
        ];
        let a = vec![app("c.md", id(1, 1), Some("h"), F)];
        let got = pair(&v, &a);
        assert_eq!(
            got.len(),
            1,
            "two hard-linked rows cannot both take one new file"
        );
    }

    #[test]
    fn folder_candidates_name_every_folder_pair_a_move_could_be_part_of() {
        let f = |a: &str, b: &str| folder_candidates(a, b);
        let p = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(f("A/x.md", "B/x.md"), vec![p("A", "B")]);
        assert_eq!(f("P/A/x.md", "P/B/x.md"), vec![p("P/A", "P/B")]);
        assert_eq!(
            f("A/s/y.md", "B/s/y.md"),
            vec![p("A", "B"), p("A/s", "B/s")],
            "shortest folder first"
        );
        assert_eq!(
            f("Notes/2024/a.md", "Archive/2024/a.md"),
            vec![p("Notes", "Archive"), p("Notes/2024", "Archive/2024")]
        );
        assert!(f("x.md", "y.md").is_empty(), "a plain file rename");
        assert!(f("A/x.md", "A/y.md").is_empty(), "the file name changed");
        assert_eq!(
            f("A/x.md", "A/B/x.md"),
            vec![p("A", "A/B")],
            "a folder pair; it is the caller's `gone` that rules it out, as A is still there"
        );
        assert!(f("x.md", "A/x.md").is_empty(), "no folder on the old side");
    }

    fn r(from: &str, to: &str) -> Rename {
        Rename {
            source: "notes".into(),
            from: from.into(),
            to_source: "notes".into(),
            to: to.into(),
            basis: Basis::Os,
        }
    }

    #[test]
    fn many_files_moved_from_one_folder_to_another_are_one_folder_rename() {
        let got = group_folders(
            vec![
                r("Old/a.md", "New/a.md"),
                r("Old/sub/b.md", "New/sub/b.md"),
                r("Solo.md", "Renamed.md"),
            ],
            |_| true,
        );
        assert_eq!(
            got,
            vec![
                Change::Folder {
                    old_dir: "Old".into(),
                    new_dir: "New".into(),
                    files: vec![r("Old/a.md", "New/a.md"), r("Old/sub/b.md", "New/sub/b.md")],
                },
                Change::File(r("Solo.md", "Renamed.md")),
            ]
        );
    }

    #[test]
    fn a_folder_moved_into_another_parent_is_the_folder_not_its_old_parent() {
        // `mv Notes/2024 Archive/2024`: Notes is still there.
        let moved = vec![
            r("Notes/2024/a.md", "Archive/2024/a.md"),
            r("Notes/2024/sub/b.md", "Archive/2024/sub/b.md"),
        ];
        let got = group_folders(moved.clone(), |dir| dir == "Notes/2024");
        assert_eq!(
            got,
            vec![Change::Folder {
                old_dir: "Notes/2024".into(),
                new_dir: "Archive/2024".into(),
                files: moved,
            }]
        );
    }

    #[test]
    fn files_moved_out_of_a_folder_that_is_still_there_stay_file_renames() {
        let got = group_folders(
            vec![r("Old/a.md", "New/a.md"), r("Old/b.md", "New/b.md")],
            |_| false,
        );
        assert_eq!(
            got,
            vec![
                Change::File(r("Old/a.md", "New/a.md")),
                Change::File(r("Old/b.md", "New/b.md")),
            ]
        );
    }

    #[test]
    fn one_file_moved_between_folders_stays_a_file_rename() {
        let got = group_folders(vec![r("Old/a.md", "New/a.md")], |_| true);
        assert_eq!(got, vec![Change::File(r("Old/a.md", "New/a.md"))]);
    }

    // ---- hold logic, on tokio's paused clock -----------------------

    fn hash_twins() -> (Vec<Vanished>, Vec<Appeared>) {
        (
            vec![van("a.md", id(1, 10), Some("h1"), None)],
            vec![app("b.md", id(1, 99), Some("h1"), None)],
        )
    }

    #[tokio::test(start_paused = true)]
    async fn an_os_pair_is_applied_at_once_with_no_hold() {
        let mut d = Detector::new(2 * SEC);
        let out = d.ingest(
            Instant::now(),
            vec![van("a.md", id(1, 10), Some("h1"), c(1))],
            vec![app("b.md", id(1, 10), Some("h1"), c(1))],
        );
        assert_eq!(out.renames, vec![r("a.md", "b.md")]);
        assert!(d.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hash_pair_waits_the_hold_and_is_then_applied() {
        let mut d = Detector::new(2 * SEC);
        let (v, a) = hash_twins();
        let out = d.ingest(Instant::now(), v, a);
        assert!(out.is_empty(), "a hash pair is held, not applied");
        tokio::time::advance(Duration::from_millis(1900)).await;
        assert!(d.expire(Instant::now()).is_empty(), "still inside the hold");
        tokio::time::advance(Duration::from_millis(200)).await;
        let out = d.expire(Instant::now());
        assert_eq!(out.renames.len(), 1);
        assert_eq!(out.renames[0].basis, Basis::Hash);
        assert!(out.gave_up_vanished.is_empty() && out.gave_up_appeared.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unpaired_vanish_and_create_are_given_up_when_the_hold_ends() {
        let mut d = Detector::new(2 * SEC);
        let out = d.ingest(
            Instant::now(),
            vec![van("gone.md", id(1, 1), Some("h1"), None)],
            vec![app("new.md", id(1, 2), Some("h2"), None)],
        );
        assert!(out.is_empty());
        assert_eq!(d.next_deadline(), Some(Instant::now() + 2 * SEC));
        tokio::time::advance(2 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_vanished[0].path, "gone.md");
        assert_eq!(out.gave_up_appeared.len(), 1);
        assert!(d.is_empty());
        assert_eq!(d.next_deadline(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_path_reported_in_two_batches_is_never_paired_with_itself() {
        let mut d = Detector::new(2 * SEC);
        // Batch 1: Out.md is gone.
        let out = d.ingest(
            Instant::now(),
            vec![van("Out.md", id(1, 10), Some("h1"), F)],
            vec![],
        );
        assert!(out.is_empty());
        // Batch 2, a moment later: the same path again, as a new file
        // with the same hash and the same OS facts.
        tokio::time::advance(SEC).await;
        let out = d.ingest(
            Instant::now(),
            vec![],
            vec![app("Out.md", id(1, 10), Some("h1"), F)],
        );
        assert!(out.renames.is_empty(), "no Out.md -> Out.md");
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty(), "no Out.md -> Out.md after the hold");
        assert_eq!(
            out.gave_up_vanished.len(),
            1,
            "the vanish ends as uncertain"
        );
        assert_eq!(out.gave_up_vanished[0].path, "Out.md");
        assert!(d.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_twin_that_shows_up_after_the_hold_is_a_plain_create_with_no_late_fix() {
        let mut d = Detector::new(2 * SEC);
        d.ingest(
            Instant::now(),
            vec![van("a.md", id(1, 10), Some("h1"), None)],
            vec![],
        );
        tokio::time::advance(2 * SEC + Duration::from_millis(1)).await;
        let out = d.expire(Instant::now());
        assert_eq!(out.gave_up_vanished.len(), 1);
        let out = d.ingest(
            Instant::now(),
            vec![],
            vec![app("b.md", id(1, 99), Some("h1"), None)],
        );
        assert!(out.renames.is_empty(), "no late fix");
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty());
        assert_eq!(out.gave_up_appeared.len(), 1, "a plain create");
    }

    #[tokio::test(start_paused = true)]
    async fn a_twin_inside_the_hold_pairs_and_the_early_end_is_not_given_up_first() {
        let mut d = Detector::new(2 * SEC);
        d.ingest(
            Instant::now(),
            vec![van("a.md", id(1, 10), Some("h1"), None)],
            vec![],
        );
        tokio::time::advance(Duration::from_millis(1500)).await;
        d.ingest(
            Instant::now(),
            vec![],
            vec![app("b.md", id(1, 99), Some("h1"), None)],
        );
        // The vanish's own hold ends at 2.0 s, but the pair ripens at 3.5 s.
        tokio::time::advance(Duration::from_millis(1000)).await;
        let out = d.expire(Instant::now());
        assert!(
            out.is_empty(),
            "the vanish is not given up while it has a pair waiting"
        );
        tokio::time::advance(Duration::from_millis(1100)).await;
        assert_eq!(d.expire(Instant::now()).renames.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_twin_inside_the_hold_makes_it_ambiguous() {
        let mut d = Detector::new(2 * SEC);
        let (v, a) = hash_twins();
        d.ingest(Instant::now(), v, a);
        tokio::time::advance(SEC).await;
        d.ingest(
            Instant::now(),
            vec![],
            vec![app("c.md", id(1, 100), Some("h1"), None)],
        );
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_appeared.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_frozen_detector_runs_no_hold_down_and_thaws_with_the_time_it_has_left() {
        let mut d = Detector::new(2 * SEC);
        let (v, a) = hash_twins();
        d.ingest(Instant::now(), v, a);
        tokio::time::advance(SEC).await;
        d.freeze(Instant::now());
        tokio::time::advance(30 * SEC).await;
        assert!(
            d.expire(Instant::now()).is_empty(),
            "frozen: nothing ripens"
        );
        d.thaw(Instant::now());
        tokio::time::advance(Duration::from_millis(900)).await;
        assert!(
            d.expire(Instant::now()).is_empty(),
            "1 s of the hold was left"
        );
        tokio::time::advance(Duration::from_millis(200)).await;
        assert_eq!(d.expire(Instant::now()).renames.len(), 1);
    }

    // ---- the task that wakes the detector ----------------------------

    type Seen = Arc<Mutex<Vec<Decisions>>>;

    fn start_holds(
        blocked: Arc<AtomicBool>,
    ) -> (
        Arc<Mutex<Detector>>,
        Arc<Notify>,
        Seen,
        tokio::task::JoinHandle<()>,
    ) {
        let detector = Arc::new(Mutex::new(Detector::new(2 * SEC)));
        let wake = Arc::new(Notify::new());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(run_holds(
            detector.clone(),
            wake.clone(),
            move || blocked.load(Ordering::SeqCst),
            {
                let seen = seen.clone();
                move |d| seen.lock().unwrap().push(d)
            },
        ));
        (detector, wake, seen, task)
    }

    async fn settle_tasks() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_holds_applies_a_hash_pair_after_the_hold_and_not_before() {
        let (detector, wake, seen, task) = start_holds(Arc::new(AtomicBool::new(false)));
        let (v, a) = hash_twins();
        let out = detector.lock().unwrap().ingest(Instant::now(), v, a);
        assert!(out.is_empty());
        wake.notify_one();
        settle_tasks().await;

        tokio::time::advance(Duration::from_millis(1900)).await;
        settle_tasks().await;
        assert!(seen.lock().unwrap().is_empty(), "inside the hold");
        tokio::time::advance(Duration::from_millis(200)).await;
        settle_tasks().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].renames.len(), 1);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_holds_gives_an_unpaired_vanish_up_when_its_hold_ends() {
        let (detector, wake, seen, task) = start_holds(Arc::new(AtomicBool::new(false)));
        detector.lock().unwrap().ingest(
            Instant::now(),
            vec![van("a.md", id(1, 10), Some("h1"), None)],
            vec![],
        );
        wake.notify_one();
        settle_tasks().await;
        tokio::time::advance(2 * SEC + Duration::from_millis(10)).await;
        settle_tasks().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].gave_up_vanished.len(), 1);
        assert!(seen[0].renames.is_empty());
        task.abort();
    }

    /// Lets `total` pass in 10 ms steps, running the tasks between them,
    /// the way a real clock would (one big `advance` skips the polls).
    async fn pass(total: Duration) {
        let step = Duration::from_millis(10);
        let mut left = total;
        while !left.is_zero() {
            let now = step.min(left);
            tokio::time::advance(now).await;
            settle_tasks().await;
            left -= now;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_holds_does_not_run_a_hold_down_while_blocked() {
        let blocked = Arc::new(AtomicBool::new(false));
        let (detector, wake, seen, task) = start_holds(blocked.clone());
        let (v, a) = hash_twins();
        detector.lock().unwrap().ingest(Instant::now(), v, a);
        wake.notify_one();
        settle_tasks().await;

        // 1 s of the 2 s hold runs, then a write begins. Nothing wakes
        // the task for that: it has to notice on its own.
        pass(SEC).await;
        blocked.store(true, Ordering::SeqCst);
        // A long write: far longer than the hold. The rename must not
        // be applied early or given up unpaired meanwhile.
        pass(Duration::from_secs(40)).await;
        assert!(seen.lock().unwrap().is_empty(), "no hold ran while blocked");
        assert!(detector.lock().unwrap().is_frozen());

        blocked.store(false, Ordering::SeqCst);
        pass(Duration::from_millis(800)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "about 1 s of the hold was still left"
        );
        pass(Duration::from_millis(400)).await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].renames.len(),
            1,
            "applied as a pair, not given up unpaired"
        );
        assert!(seen[0].gave_up_vanished.is_empty());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_os_pair_needs_no_hold_in_the_task_either() {
        let (detector, _wake, _seen, task) = start_holds(Arc::new(AtomicBool::new(false)));
        let out = detector.lock().unwrap().ingest(
            Instant::now(),
            vec![van("a.md", id(1, 10), Some("h1"), c(2))],
            vec![app("b.md", id(1, 10), Some("h2"), c(2))],
        );
        assert_eq!(
            out.renames.len(),
            1,
            "decided by ingest itself, with no time passing"
        );
        task.abort();
    }

    // ---- dispatch --------------------------------------------------

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl RenameFixer for Recorder {
        fn fix_rename(&self, source: &str, old: &str, new: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("file {source} {old} -> {new}"));
            Ok(())
        }
        fn fix_folder_rename(
            &self,
            source: &str,
            old_dir: &str,
            new_dir: &str,
            moves: &[(String, String)],
        ) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "folder {source} {old_dir} -> {new_dir} ({} files)",
                moves.len()
            ));
            Ok(())
        }
        fn fix_move(
            &self,
            old_source: &str,
            old: &str,
            new_source: &str,
            new: &str,
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("move {old_source}:{old} -> {new_source}:{new}"));
            Ok(())
        }
        fn record_uncertain(&self, source: &str, path: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("uncertain {source} {path}"));
            Ok(())
        }
    }

    /// The roots `dispatch` looks a source up in: `notes` is `dir`.
    fn notes_at(dir: &std::path::Path) -> impl Fn(&str) -> Option<PathBuf> + '_ {
        move |source| (source == "notes").then(|| dir.to_path_buf())
    }

    #[test]
    fn dispatch_fixes_a_folder_once_and_logs_the_vanished_that_were_given_up() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        dispatch(
            &rec,
            &notes_at(dir.path()),
            Decisions {
                renames: vec![
                    r("Old/a.md", "New/a.md"),
                    r("Old/b.md", "New/b.md"),
                    r("x.md", "y.md"),
                ],
                gave_up_vanished: vec![van("gone.md", None, None, None)],
                gave_up_appeared: vec![],
            },
        );
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![
                "folder notes Old -> New (2 files)".to_string(),
                "file notes x.md -> y.md".to_string(),
                "uncertain notes gone.md".to_string(),
            ]
        );
    }

    #[test]
    fn dispatch_fixes_file_by_file_when_the_old_folder_is_still_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Old")).unwrap();
        let rec = Recorder::default();
        dispatch(
            &rec,
            &notes_at(dir.path()),
            Decisions {
                renames: vec![r("Old/a.md", "New/a.md"), r("Old/b.md", "New/b.md")],
                ..Decisions::default()
            },
        );
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![
                "file notes Old/a.md -> New/a.md".to_string(),
                "file notes Old/b.md -> New/b.md".to_string(),
            ],
            "two files moved out of a folder that is still there are not a folder rename"
        );
    }

    #[test]
    fn dispatch_fixes_a_folder_moved_into_another_parent_as_one_while_the_parent_stays() {
        let dir = tempfile::tempdir().unwrap();
        // After `mv Notes/2024 Archive/2024`: Notes is still on disk.
        std::fs::create_dir_all(dir.path().join("Notes")).unwrap();
        std::fs::create_dir_all(dir.path().join("Archive/2024")).unwrap();
        let rec = Recorder::default();
        dispatch(
            &rec,
            &notes_at(dir.path()),
            Decisions {
                renames: vec![
                    r("Notes/2024/a.md", "Archive/2024/a.md"),
                    r("Notes/2024/b.md", "Archive/2024/b.md"),
                ],
                ..Decisions::default()
            },
        );
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec!["folder notes Notes/2024 -> Archive/2024 (2 files)".to_string()]
        );
    }

    // ---- F5 ----------------------------------------------------------

    #[test]
    fn dispatch_never_fixes_a_rename_of_a_path_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        dispatch(
            &rec,
            &notes_at(dir.path()),
            Decisions {
                renames: vec![r("Out.md", "Out.md"), r("x.md", "y.md")],
                ..Decisions::default()
            },
        );
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![
                "uncertain notes Out.md".to_string(),
                "file notes x.md -> y.md".to_string(),
            ],
            "the self pair is no fix; its path is gone from disk, so it is uncertain"
        );
    }

    // ---- across sources (G6, FR21) ---------------------------------

    fn van_in(source: &str, path: &str, hash: Option<&str>) -> Vanished {
        Vanished {
            source: source.to_string(),
            ..van(path, None, hash, None)
        }
    }

    fn app_in(source: &str, path: &str, hash: Option<&str>) -> Appeared {
        Appeared {
            source: source.to_string(),
            ..app(path, None, hash, None)
        }
    }

    /// `(from source, from, to source, to)` of every pair.
    fn cross_pairs(v: &[Vanished], a: &[Appeared]) -> Vec<(String, String, String, String)> {
        let mut out: Vec<_> = pair(v, a)
            .into_iter()
            .map(|p| {
                (
                    v[p.vanished].source.clone(),
                    v[p.vanished].path.clone(),
                    a[p.appeared].source.clone(),
                    a[p.appeared].path.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn mv(
        from_source: &str,
        from: &str,
        to_source: &str,
        to: &str,
    ) -> (String, String, String, String) {
        (
            from_source.to_string(),
            from.to_string(),
            to_source.to_string(),
            to.to_string(),
        )
    }

    struct CrossCase {
        name: &'static str,
        vanished: Vec<Vanished>,
        appeared: Vec<Appeared>,
        want: Vec<(String, String, String, String)>,
    }

    #[test]
    fn the_pool_spans_sources_and_a_hash_is_unique_over_all_of_them() {
        let cases = vec![
            CrossCase {
                name: "G6 one vanished in notes and one new in tephra with the same hash: a move",
                vanished: vec![van_in("notes", "Ideas/Rename Notes.md", Some("h"))],
                appeared: vec![app_in("tephra", "docs/rename-notes.md", Some("h"))],
                want: vec![mv(
                    "notes",
                    "Ideas/Rename Notes.md",
                    "tephra",
                    "docs/rename-notes.md",
                )],
            },
            CrossCase {
                name: "G6 the same file name in both sources is still a move, not the same path",
                vanished: vec![van_in("notes", "a.md", Some("h"))],
                appeared: vec![app_in("tephra", "a.md", Some("h"))],
                want: vec![mv("notes", "a.md", "tephra", "a.md")],
            },
            CrossCase {
                name: "E4 two empty files vanish in notes, one appears in tephra: no pair",
                vanished: vec![
                    van_in("notes", "a.md", Some("empty")),
                    van_in("notes", "b.md", Some("empty")),
                ],
                appeared: vec![app_in("tephra", "c.md", Some("empty"))],
                want: vec![],
            },
            CrossCase {
                name: "E4 one vanishes in notes, two appear (one per source): no pair",
                vanished: vec![van_in("notes", "a.md", Some("h"))],
                appeared: vec![
                    app_in("tephra", "b.md", Some("h")),
                    app_in("notes", "c.md", Some("h")),
                ],
                want: vec![],
            },
            CrossCase {
                name: "E4 a second vanished with the hash in a third source: no pair",
                vanished: vec![
                    van_in("notes", "a.md", Some("h")),
                    van_in("other", "z.md", Some("h")),
                ],
                appeared: vec![app_in("tephra", "b.md", Some("h"))],
                want: vec![],
            },
            CrossCase {
                name: "an unrelated hash is paired while the ambiguous one is not",
                vanished: vec![
                    van_in("notes", "a.md", Some("h")),
                    van_in("notes", "b.md", Some("h")),
                    van_in("notes", "x.md", Some("k")),
                ],
                appeared: vec![
                    app_in("tephra", "c.md", Some("h")),
                    app_in("tephra", "y.md", Some("k")),
                ],
                want: vec![mv("notes", "x.md", "tephra", "y.md")],
            },
            CrossCase {
                name: "no hash on either side: nothing to pair on",
                vanished: vec![van_in("notes", "a.md", None)],
                appeared: vec![app_in("tephra", "a2.md", None)],
                want: vec![],
            },
            CrossCase {
                name: "E5 a copy: the original is not vanished, so there is no vanished side",
                vanished: vec![],
                appeared: vec![app_in("tephra", "copy.md", Some("h"))],
                want: vec![],
            },
        ];
        for case in cases {
            assert_eq!(
                cross_pairs(&case.vanished, &case.appeared),
                case.want,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn an_os_pair_across_sources_is_a_move_when_the_identity_agrees() {
        let v = vec![Vanished {
            source: "notes".into(),
            ..van("a.md", id(1, 10), Some("old"), F)
        }];
        let a = vec![Appeared {
            source: "tephra".into(),
            ..app("docs/a.md", id(1, 10), Some("new"), F)
        }];
        assert_eq!(
            cross_pairs(&v, &a),
            vec![mv("notes", "a.md", "tephra", "docs/a.md")]
        );
        let other_inode = vec![Appeared {
            source: "tephra".into(),
            ..app("docs/a.md", id(1, 11), Some("new"), F)
        }];
        assert!(cross_pairs(&v, &other_inode).is_empty());
    }

    #[test]
    fn a_folder_is_only_ever_renamed_inside_its_source() {
        // Identical files under `Old/` in notes and `New/` in tephra: the
        // relative-path pass must not pair them as a folder rename.
        let v = vec![
            van_in("notes", "Old/a.md", Some("h")),
            van_in("notes", "Old/b.md", Some("h")),
        ];
        let a = vec![
            app_in("tephra", "New/a.md", Some("h")),
            app_in("tephra", "New/b.md", Some("h")),
        ];
        assert!(cross_pairs(&v, &a).is_empty());
        // Inside one source the same facts pair.
        let a_same = vec![
            app_in("notes", "New/a.md", Some("h")),
            app_in("notes", "New/b.md", Some("h")),
        ];
        assert_eq!(cross_pairs(&v, &a_same).len(), 2);
    }

    /// What the detector decides, with the clock handed in.
    #[tokio::test(start_paused = true)]
    async fn the_create_before_the_delete_and_the_delete_before_the_create_both_pair() {
        for create_first in [false, true] {
            let mut d = Detector::new(2 * SEC);
            let v = vec![van_in("notes", "a.md", Some("h"))];
            let a = vec![app_in("tephra", "docs/a.md", Some("h"))];
            let (first, second) = if create_first {
                ((vec![], a), (v, vec![]))
            } else {
                ((v, vec![]), (vec![], a))
            };
            assert!(d.ingest(Instant::now(), first.0, first.1).is_empty());
            tokio::time::advance(SEC).await;
            assert!(d.ingest(Instant::now(), second.0, second.1).is_empty());
            // Held from the later of the two.
            tokio::time::advance(Duration::from_millis(1900)).await;
            assert!(d.expire(Instant::now()).is_empty(), "still inside the hold");
            tokio::time::advance(Duration::from_millis(200)).await;
            let out = d.expire(Instant::now());
            assert_eq!(out.renames.len(), 1, "create_first = {create_first}");
            let m = &out.renames[0];
            assert!(m.is_move());
            assert_eq!(
                (
                    m.source.as_str(),
                    m.from.as_str(),
                    m.to_source.as_str(),
                    m.to.as_str()
                ),
                ("notes", "a.md", "tephra", "docs/a.md")
            );
            assert!(d.is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_partner_after_the_hold_is_no_late_move() {
        let mut d = Detector::new(2 * SEC);
        d.ingest(
            Instant::now(),
            vec![van_in("notes", "a.md", Some("h"))],
            vec![],
        );
        tokio::time::advance(2 * SEC + Duration::from_millis(1)).await;
        let out = d.expire(Instant::now());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_vanished[0].source, "notes");
        let out = d.ingest(
            Instant::now(),
            vec![],
            vec![app_in("tephra", "docs/a.md", Some("h"))],
        );
        assert!(out.renames.is_empty());
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty(), "no retroactive pairing");
        assert_eq!(out.gave_up_appeared.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_candidate_in_another_source_makes_the_pair_ambiguous() {
        let mut d = Detector::new(2 * SEC);
        d.ingest(
            Instant::now(),
            vec![van_in("notes", "a.md", Some("h"))],
            vec![app_in("tephra", "b.md", Some("h"))],
        );
        tokio::time::advance(SEC).await;
        d.ingest(
            Instant::now(),
            vec![],
            vec![app_in("other", "c.md", Some("h"))],
        );
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_appeared.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_path_in_two_sources_is_two_files_not_one() {
        let mut d = Detector::new(2 * SEC);
        // `a.md` vanishes from notes and a different file `a.md` appears
        // in tephra with another hash: nothing pairs, and neither is
        // mistaken for the other.
        d.ingest(
            Instant::now(),
            vec![van_in("notes", "a.md", Some("h1"))],
            vec![app_in("tephra", "a.md", Some("h2"))],
        );
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert!(out.renames.is_empty());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_appeared.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn forgetting_a_source_drops_what_is_held_for_it() {
        let mut d = Detector::new(2 * SEC);
        d.ingest(
            Instant::now(),
            vec![
                van_in("notes", "a.md", Some("h")),
                van_in("tephra", "t.md", Some("k")),
            ],
            vec![app_in("notes", "n.md", Some("g"))],
        );
        d.forget_source("notes");
        tokio::time::advance(3 * SEC).await;
        let out = d.expire(Instant::now());
        assert_eq!(out.gave_up_vanished.len(), 1);
        assert_eq!(out.gave_up_vanished[0].source, "tephra");
        assert!(out.gave_up_appeared.is_empty());
        assert!(d.is_empty());
    }

    #[test]
    fn dispatch_fixes_a_move_once_and_a_rename_in_the_old_source_as_before() {
        let notes = tempfile::tempdir().unwrap();
        let tephra = tempfile::tempdir().unwrap();
        let roots = |source: &str| match source {
            "notes" => Some(notes.path().to_path_buf()),
            "tephra" => Some(tephra.path().to_path_buf()),
            _ => None,
        };
        let rec = Recorder::default();
        dispatch(
            &rec,
            &roots,
            Decisions {
                renames: vec![
                    Rename {
                        source: "notes".into(),
                        from: "Ideas/Rename Notes.md".into(),
                        to_source: "tephra".into(),
                        to: "docs/rename-notes.md".into(),
                        basis: Basis::Hash,
                    },
                    r("x.md", "y.md"),
                ],
                gave_up_vanished: vec![van_in("notes", "gone.md", None)],
                gave_up_appeared: vec![],
            },
        );
        let log = rec.0.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "file notes x.md -> y.md".to_string(),
                "move notes:Ideas/Rename Notes.md -> tephra:docs/rename-notes.md".to_string(),
                "uncertain notes gone.md".to_string(),
            ]
        );
    }

    /// F5: nothing in this crate's manifest could measure how alike two
    /// files are. The detector reads the OS pair, the identity and an
    /// exact hash, and has no crate to do anything else with.
    #[test]
    fn the_manifest_has_no_similarity_or_edit_distance_dependency() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("read Cargo.toml");
        let deps: Vec<String> = manifest
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| l.split(['=', ' ']).next().unwrap_or("").to_lowercase())
            .collect();
        for banned in [
            "strsim",
            "levenshtein",
            "edit-distance",
            "edit_distance",
            "similar",
            "fuzzy-matcher",
            "fuzzy_matcher",
            "simhash",
            "ssdeep",
            "ngrammatic",
            "triple_accel",
            "textdistance",
            "rapidfuzz",
            "eddie",
            "jaro",
            "difflib",
            "sublime_fuzzy",
        ] {
            assert!(
                !deps.iter().any(|d| d == banned),
                "{banned} would let the rename detector guess: it may only read the OS pair, identity and an exact hash"
            );
        }
        // And the detector's own source names none of them.
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rename_detect.rs"))
                .expect("read rename_detect.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for word in ["levenshtein", "strsim", "similar", "edit_distance", "fuzzy"] {
            assert!(
                !code.to_lowercase().contains(word) || code.contains("no similarity"),
                "{word}"
            );
        }
    }
}
