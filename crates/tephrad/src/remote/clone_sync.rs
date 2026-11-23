//! The pass that looks after a GitHub source's git clone (atlas-27 PR-9).
//!
//! The clone is `<data_dir>/remote/<source>/clone/` with a `.git` in it: Dan's
//! own `git clone`. A source whose `clone/` has no `.git` is a plain cache of
//! downloaded files and nothing here applies to it.
//!
//! A pass runs after every sync (the poller calls it with [`Pass::Sync`]) and
//! on a save in the clone, after the watcher's debounce ([`Pass::Recount`]).
//! At most one runs per source at a time, so two syncs and a watcher event at
//! the same moment make one look after the other and never fight over git's
//! lock files.
//!
//! What a pass does, in order:
//!
//! 1. Skips the clone entirely when Dan is mid rebase, merge, cherry-pick or
//!    revert, or git holds `index.lock`; the next pass tries again.
//! 2. [`Pass::Sync`] only: `git fetch` of the tracked branch. This writes
//!    inside `.git` and nowhere else. A failure sets `offline` or
//!    `auth_required` and keeps every number the last good pass found.
//! 3. Looks at the clone ([`clone_state::snapshot`]): how far ahead and
//!    behind the remote tip it is, and which files differ from it.
//! 4. Decides ([`clone_state::decide`]). Only a clean clone strictly behind
//!    on the tracked branch gets `merge --ff-only --no-overwrite-ignore` (git
//!    then also refuses to replace one of Dan's ignored files); every other
//!    state is a fetch and nothing more. A rewritten remote history (the tip
//!    recorded before is no ancestor of the new one, whether the branch was
//!    replaced or moved back) is never pulled.
//! 5. Writes the numbers, the item states (`not_pushed`, and `needs_attention`
//!    for a file changed on both sides) and the source's status, and
//!    announces one change if anything visible moved.
//!
//! Tephra never commits, pushes, rebases, resets, stashes or checks out: see
//! [`clone_state::allowed`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tephra_cache::{
    clone_reason_not_on_branch, is_clone_reason, Cache, CloneChanges, CloneState,
    CLONE_FETCH_REASON_PREFIX, CLONE_REASON_DIVERGED, CLONE_REASON_REWRITTEN, STATUS_ACTIVE,
    STATUS_AUTH_REQUIRED, STATUS_NEEDS_ATTENTION, STATUS_OFFLINE,
};

use crate::ignore::IgnoreRules;
use crate::registry::{retry_cache_on_busy, DaemonEvent};
use crate::remote::clone_state::{
    both_sides, classify_fetch_failure, decide, history_rewritten, operation_in_progress, resolve,
    safe_branch, snapshot, tracking_ref, Decision, Facts, FetchFailure, Git, GitError, Snapshot,
};
use crate::remote::poll::{clone_copy, CopyState};
use crate::remote_github::{printable, RemoteHub, GITHUB_KIND};
use crate::watcher::{self, WatcherHandle};
use crate::writer_gate::WriterGate;

/// What a pass does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// After a sync: fetch, then look and decide.
    Sync,
    /// After a save in the clone: look only. No network, no fast-forward.
    Recount,
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The source has no git clone: nothing was done.
    NoClone,
    /// The clone was not looked at (an operation is in progress, or git
    /// failed); nothing was changed.
    Skipped,
    /// The clone was looked at. `changed` is true when something a client
    /// shows moved.
    Looked { changed: bool },
}

/// What a look found that the source's status says.
struct Look {
    /// The remote's history was rewritten and the clone is not on the new one.
    rewritten: bool,
    /// HEAD is not the tracked branch.
    off_branch: bool,
    /// What HEAD is (a branch, or `HEAD` when detached).
    current: String,
    /// HEAD and the remote tip each have commits the other lacks.
    diverged: bool,
}

/// What a pass needs: the cache, the writer gate, the event channel and the
/// hub (the clock, the git program, the content cache's place).
#[derive(Clone)]
pub(crate) struct CloneCtx {
    pub(crate) name: String,
    pub(crate) cache_path: PathBuf,
    pub(crate) gate: Arc<WriterGate>,
    pub(crate) events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
    pub(crate) hub: Arc<RemoteHub>,
}

/// `true` when `root` is a folder (not a link) with a `.git` (not a link)
/// directly in it.
pub(crate) fn is_git_clone(root: &Path) -> bool {
    let plain = |p: &Path, dir: bool| {
        std::fs::symlink_metadata(p).is_ok_and(|m| {
            let t = m.file_type();
            !t.is_symlink() && if dir { t.is_dir() } else { true }
        })
    };
    plain(root, true) && plain(&root.join(".git"), false)
}

impl RemoteHub {
    /// The one lock of `source`'s clone: a pass holds it from start to end.
    pub(crate) fn clone_lock(&self, source: &str) -> Arc<Mutex<()>> {
        self.clone_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(source.to_string())
            .or_default()
            .clone()
    }
}

impl CloneCtx {
    fn announce(&self) {
        let _ = self.events_tx.send(DaemonEvent::SourceChanged {
            source: self.name.clone(),
            paths: Vec::new(),
            fixes: Vec::new(),
        });
    }

    /// Runs one pass, blocking. Call it from the blocking pool.
    pub(crate) fn run(&self, pass: Pass) -> Outcome {
        self.run_then(pass, || {})
    }

    /// [`CloneCtx::run`], calling `begun` once this pass holds the clone's
    /// lock and is about to look.
    pub(crate) fn run_then(&self, pass: Pass, begun: impl FnOnce()) -> Outcome {
        let lock = self.hub.clone_lock(&self.name);
        let _one = lock.lock().unwrap_or_else(|p| p.into_inner());
        begun();
        match self.pass(pass) {
            Ok(outcome) => outcome,
            Err(why) => {
                tracing::warn!(source = %self.name, why, "the clone was not looked at");
                Outcome::Skipped
            }
        }
    }

    fn db<T>(&self, f: impl Fn() -> Result<T, tephra_cache::CacheError>) -> Result<T, String> {
        retry_cache_on_busy(&self.gate, f).map_err(|e| e.to_string())
    }

    fn pass(&self, pass: Pass) -> Result<Outcome, String> {
        let cache = Cache::open(&self.cache_path).map_err(|e| e.to_string())?;
        let Some(source) = self
            .db(|| cache.remote_source(&self.name))?
            .filter(|s| s.kind == GITHUB_KIND)
        else {
            return Ok(Outcome::NoClone);
        };
        let Some(branch) = source.branch.clone().filter(|b| safe_branch(b)) else {
            return Ok(Outcome::NoClone);
        };
        let root = self.hub.cache_dir(&self.name).join("clone");
        if !is_git_clone(&root) {
            return Ok(Outcome::NoClone);
        }
        let git = Git::new(self.hub.git_program.clone(), root.clone());

        // Dan is in the middle of something: leave the clone alone.
        let git_dir = git.run(&["rev-parse", "--git-dir"]).map_err(err_text)?;
        if !git_dir.ok() {
            return Ok(Outcome::Skipped);
        }
        let git_dir = root.join(git_dir.line());
        if let Some(marker) = operation_in_progress(&git_dir) {
            tracing::info!(source = %self.name, marker, "the clone is mid-operation; this pass skips it");
            return Ok(Outcome::Skipped);
        }

        let prev = self.db(|| cache.remote_clone_state(&self.name))?;
        let remote_ref = tracking_ref(&branch);
        let before = resolve(&git, &remote_ref).map_err(err_text)?;

        // The fetch: writes inside `.git` only.
        let mut fetched = false;
        let mut failure = None;
        if pass == Pass::Sync {
            let refspec = format!("+refs/heads/{branch}:{remote_ref}");
            match git.run(&[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "origin",
                &refspec,
            ]) {
                Ok(out) if out.ok() => fetched = true,
                // The complaint is sorted and dropped: it may quote a URL
                // with the person's own credentials.
                Ok(out) => failure = Some(classify_fetch_failure(&out.stderr)),
                Err(GitError::TimedOut) => failure = Some(FetchFailure::Offline),
                Err(e) => return Err(e.to_string()),
            }
        }

        let mut snap = match snapshot(&git, &branch) {
            Ok(snap) => snap,
            Err(e) => return Err(e.to_string()),
        };
        let prev_state = prev.clone().unwrap_or_default();
        let mut ff_done = false;
        let mut rewritten = false;
        let mut old_tip = None;

        if let Some(s) = &snap {
            old_tip = prev_state.tip.clone().or(before);
            // A new tip that does not descend from the old one is a
            // force-push: the branch was replaced or moved back. It stays
            // flagged (and the old tip stays recorded) until Dan brings the
            // clone to the new history, with his own `git pull --rebase`.
            rewritten = match (&old_tip, &s.tip) {
                (Some(old), Some(new)) => {
                    history_rewritten(&git, old, new, &s.head, s.behind).map_err(err_text)?
                }
                _ => false,
            };
            let decision = decide(&Facts {
                ahead: s.ahead,
                behind: s.behind,
                dirty: s.dirty,
                untracked: s.untracked,
                on_branch: s.branch_now == branch,
                rewritten,
            });
            if decision == Decision::FastForward && fetched {
                match git.run(&["merge", "--ff-only", "--no-overwrite-ignore", &remote_ref]) {
                    Ok(out) if out.ok() => ff_done = true,
                    // Git refused (an untracked or ignored file in the way,
                    // an edit made since the look): the clone is not clean
                    // after all. Nothing was changed; the next pass looks
                    // again.
                    _ => {
                        tracing::info!(source = %self.name, "git would not fast-forward the clone; it was left as it is")
                    }
                }
                if ff_done {
                    snap = snapshot(&git, &branch).map_err(|e| e.to_string())?;
                }
            }
        }

        let mut changed = ff_done;
        if let Some(s) = &snap {
            changed |= self.record(&cache, &root, &branch, s, &prev, rewritten, old_tip)?;
        }
        changed |= self.set_status(
            &cache,
            pass,
            failure,
            snap.as_ref().map(|s| Look {
                rewritten,
                off_branch: s.branch_now != branch,
                current: s.branch_now.clone(),
                diverged: s.ahead > 0 && s.behind > 0,
            }),
            &branch,
        )?;
        if changed {
            self.announce();
        }
        Ok(Outcome::Looked { changed })
    }

    /// Writes the numbers and the item states of a look. `true` when a
    /// count, `behind` or an item changed.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        cache: &Cache,
        root: &Path,
        _branch: &str,
        s: &Snapshot,
        prev: &Option<CloneState>,
        rewritten: bool,
        old_tip: Option<String>,
    ) -> Result<bool, String> {
        let state = CloneState {
            tip: if rewritten { old_tip } else { s.tip.clone() },
            behind: s.behind,
            not_pushed: s.local.len() as u64,
        };
        let shown_before = prev.as_ref().map(|p| (p.behind, p.not_pushed));
        let moved = shown_before != Some((state.behind, state.not_pushed))
            && !(prev.is_none() && state.behind == 0 && state.not_pushed == 0);
        if prev.as_ref() != Some(&state) {
            self.db(|| cache.set_remote_clone_state(&self.name, &state))?;
        }
        let changes = CloneChanges {
            changed: s.local.iter().cloned().collect(),
            both: both_sides(&s.local, &s.remote).into_iter().collect(),
        };
        let has_copy = |path: &str| matches!(clone_copy(root, path), CopyState::File(_));
        let rows = self.db(|| cache.apply_clone_changes(&self.name, &changes, &has_copy))?;
        Ok(moved || rows > 0)
    }

    /// Sets the source's status for what the pass found, only over a status
    /// that is `active` or that the clone's own pass wrote. `true` when it
    /// changed.
    fn set_status(
        &self,
        cache: &Cache,
        pass: Pass,
        failure: Option<FetchFailure>,
        look: Option<Look>,
        branch: &str,
    ) -> Result<bool, String> {
        let wanted: Option<(&'static str, String)> = match failure {
            Some(FetchFailure::Offline) => Some((
                STATUS_OFFLINE,
                format!("{CLONE_FETCH_REASON_PREFIX}the remote cannot be reached; the last counts are kept"),
            )),
            Some(FetchFailure::Auth) => Some((
                STATUS_AUTH_REQUIRED,
                format!("{CLONE_FETCH_REASON_PREFIX}the remote refused the credentials git has for this clone; the last counts are kept"),
            )),
            Some(FetchFailure::NoRemoteBranch) => Some((
                STATUS_NEEDS_ATTENTION,
                format!(
                    "{CLONE_FETCH_REASON_PREFIX}the remote no longer has branch `{}`; nothing was changed",
                    printable(branch)
                ),
            )),
            None => match look {
                Some(Look { rewritten: true, .. }) => {
                    Some((STATUS_NEEDS_ATTENTION, CLONE_REASON_REWRITTEN.to_string()))
                }
                Some(Look { off_branch: true, current, .. }) => Some((
                    STATUS_NEEDS_ATTENTION,
                    clone_reason_not_on_branch(&printable(branch), &printable(&current)),
                )),
                // Both sides have commits: not an error, so the source stays
                // `active`; the note is the guidance (F14).
                Some(Look { diverged: true, .. }) => {
                    Some((STATUS_ACTIVE, CLONE_REASON_DIVERGED.to_string()))
                }
                _ => None,
            },
        };
        let Some(current) = self.db(|| cache.remote_source(&self.name))? else {
            return Ok(false);
        };
        let reason = current.status_reason.as_deref();
        let mine = reason.is_some_and(is_clone_reason);
        let idle = current.status == STATUS_ACTIVE && reason.is_none();
        // A recount never fetched, so it cannot say a failed fetch is over:
        // it leaves that status as it is, whatever it found.
        let fetch_reason = reason.is_some_and(|r| r.starts_with(CLONE_FETCH_REASON_PREFIX));
        match wanted {
            Some((status, why)) => {
                if !(idle || mine)
                    || (current.status == status && reason == Some(why.as_str()))
                    || (fetch_reason && pass == Pass::Recount && failure.is_none())
                {
                    return Ok(false);
                }
                self.db(|| cache.set_remote_status(&self.name, status, Some(&why), None))?;
                Ok(true)
            }
            None => {
                if !mine || (fetch_reason && pass == Pass::Recount) {
                    return Ok(false);
                }
                self.db(|| cache.set_remote_status(&self.name, STATUS_ACTIVE, None, None))?;
                Ok(true)
            }
        }
    }
}

fn err_text(e: GitError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// The watcher: a save in the clone starts a recount after the debounce.
// ---------------------------------------------------------------------------

/// Starts watching `source`'s clone if it has git data and is not watched
/// yet. Called by the poller, so a clone made after the source was added is
/// picked up within a poll tick. Events under `.git` never reach the
/// debounce (the daemon's ignore rules drop them).
pub(crate) fn ensure_watch(ctx: &CloneCtx, runtime: &tokio::runtime::Handle) {
    let hub = &ctx.hub;
    let root = hub.cache_dir(&ctx.name).join("clone");
    if !is_git_clone(&root) {
        return;
    }
    let mut watchers = hub.clone_watchers.lock().unwrap_or_else(|p| p.into_inner());
    if watchers.contains_key(&ctx.name) {
        return;
    }
    // The watcher reports canonical paths (a macOS temp dir is a link).
    let Ok(root) = std::fs::canonicalize(&root) else {
        return;
    };
    let Ok(rules) = IgnoreRules::load(&root) else {
        return;
    };
    let tap = match watcher::start(root.clone(), Arc::new(RwLock::new(rules))) {
        Ok(tap) => tap,
        Err(e) => {
            tracing::warn!(source = %ctx.name, error = %e, "could not watch the clone");
            return;
        }
    };
    let queued = Arc::new(AtomicBool::new(false));
    let on_batch = {
        let ctx = ctx.clone();
        let runtime = runtime.clone();
        move |_batch: HashSet<PathBuf>| {
            // One recount waiting is enough: it looks at everything the
            // events that arrive meanwhile touched.
            if queued.swap(true, Ordering::SeqCst) {
                return;
            }
            let ctx = ctx.clone();
            let queued = queued.clone();
            runtime.spawn_blocking(move || {
                // The flag drops when the look starts, not before: events
                // that arrive while this waits for a pass in flight are
                // covered by the look that follows.
                ctx.run_then(Pass::Recount, || queued.store(false, Ordering::SeqCst));
            });
        }
    };
    let on_removed = {
        let hub = hub.clone();
        let name = ctx.name.clone();
        move || {
            hub.clone_watchers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&name);
        }
    };
    let handle = watcher::spawn_debounce(tap, root, runtime.clone(), on_batch, on_removed);
    watchers.insert(ctx.name.clone(), handle);
}

/// Stops the watcher of `source`'s clone, if it has one.
pub(crate) fn stop_watch(hub: &RemoteHub, source: &str) {
    let handle: Option<WatcherHandle> = hub
        .clone_watchers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(source);
    if let Some(handle) = handle {
        handle.stop();
    }
}
