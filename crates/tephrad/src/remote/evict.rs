//! The cap on the remote content cache (atlas-27 PR-8, G12, G20): which
//! cached files go back to `remote`, how, and the verbs that change the cap
//! and the per-source cache flag.
//!
//! What eviction is: a local cached copy is deleted and its item says
//! `remote` again. The item stays in `unit.list`, keeps its backlinks and
//! its listed size, and nothing is sent to the remote: no network call is
//! made and the remote file is never touched. Which items may go, and in
//! which order, is decided by [`tephra_cache::plan_eviction`]; this module
//! carries the plan out.
//!
//! When a pass runs: every [`DEFAULT_EVICT_TICK`] (30 s) for as long as the
//! daemon runs (so a file whose ten-minute window ends is dropped with no
//! new fetch, E5), and at once after `cache set-limit` and
//! `source.set_cache`. A pass that has nothing to drop only reads: it
//! writes nothing to `cache.db`.
//!
//! How one file goes, in this order (a fetch is never failed, never
//! blocked for long, and never handed a path that is about to vanish):
//!
//! 1. The copy is checked to be what Tephra wrote (a GitHub file by its
//!    git blob sha, a Drive file by the hash noted when it was downloaded:
//!    a Drive listing has no checksum, and a size alone would miss an edit
//!    of the same length). A copy that differs, or a Drive copy with no
//!    hash noted, is an edit Tephra has not been told of; it is left
//!    alone. Nothing is deleted on a doubt.
//! 2. The file's slot in [`crate::remote::fetch`]'s one-per-file map is
//!    taken. A fetch of the file that runs now owns it: the file is
//!    skipped. A fetch that arrives later waits for this eviction and then
//!    downloads the file afresh ([`EVICTED`]).
//! 3. `cached -> remote` in one conditional statement (still `cached`, idle
//!    for the window, no pin, no queued edit, and for a cache-off drop the
//!    flag still off). A fetch touches `last_used` before it reads the row,
//!    so one that got there first wins and this changes nothing.
//! 4. The file is deleted through the [`Disk`](crate::remote::fetch::Disk)
//!    trait, then the folders it leaves empty (never the cache root). If the
//!    delete fails the row is put back to `cached`.
//!
//! What counts toward the cap: the listed size of every item that has a
//! copy (`cached`, `pinned`, `pending_upload`, `not_pushed`), of every
//! source. The `.git` folder of a clone is NOT counted and is never
//! evicted; `cache status` reports its bytes on a line of its own. The cap
//! is soft: a file used in the last ten minutes is never dropped, and
//! neither is anything pinned or with an edit that is not sent, so the
//! cache can stand over its cap; `cache status` says so.
//!
//! Where the cap lives: `cache_limit` in `config.toml`. `cache set-limit`
//! writes it there (so it survives a restart) before it changes the live
//! value, and refuses when it cannot.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tephra_cache::{plan_eviction, Cache, EvictPick, EvictReason, ITEM_STATE_CACHED};

use crate::registry::{retry_cache_on_busy, SourceRegistry};
use crate::remote::fetch::{failure, hash_file, join_or_lead, Layout, Role};
use crate::remote_github::{now_secs, printable, RemoteHub, GITHUB_KIND};
use crate::rpc::backend::PromoteRpcError;
use crate::writer_gate::WriterGate;

/// How often the eviction pass looks at the clock.
pub const DEFAULT_EVICT_TICK: Duration = Duration::from_secs(30);

/// The word a fetch that waited on an eviction of its file gets, so it
/// reads the row again and downloads the file itself.
pub(crate) const EVICTED: &str = "evicted";

// ---- one pass ------------------------------------------------------------------------

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Files sent back to `remote`.
    pub dropped: usize,
    /// The bytes they took.
    pub freed_bytes: u64,
    /// Files the plan chose that were left (in use again, an edit not told
    /// of, a fetch running).
    pub skipped: usize,
}

/// Runs passes: one connection of its own, opened on first use, and one
/// pass at a time.
pub(crate) struct EvictRunner {
    cache_path: PathBuf,
    gate: Arc<WriterGate>,
    hub: Arc<RemoteHub>,
    conn: Mutex<Option<Cache>>,
    /// The last error a tick logged, so a lasting one is logged once.
    last_error: Mutex<Option<String>>,
    /// Test seam: called with `(source, path)` after a file was chosen and
    /// checked, and before the claim. A test parks the pass here to do what
    /// a person or a fetch would do at that moment.
    hook: Mutex<Option<EvictHook>>,
}

/// See [`EvictRunner::hook`].
pub type EvictHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

impl EvictRunner {
    pub(crate) fn new(cache_path: PathBuf, gate: Arc<WriterGate>, hub: Arc<RemoteHub>) -> Self {
        EvictRunner {
            cache_path,
            gate,
            hub,
            conn: Mutex::new(None),
            last_error: Mutex::new(None),
            hook: Mutex::new(None),
        }
    }

    /// Start-up, the two crashes a drop can leave behind:
    ///
    /// - A `cached` item whose file is not on the disk (a crash after the
    ///   delete and before the row, or a person who removed it) is `remote`
    ///   again, so its bytes stop counting and the next fetch downloads it.
    /// - A `remote` item whose file is still there (a crash after the claim
    ///   and before the delete) is `cached` again when the file is what
    ///   Tephra wrote, so it counts toward the cap and the next pass can
    ///   drop it. A file that differs from that is an edit: it is left as
    ///   it is, and not deleted.
    ///
    /// Returns `(made cached again, made remote again)`.
    pub(crate) fn reconcile(&self) -> Result<(usize, usize), String> {
        let mut slot = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(Cache::open(&self.cache_path).map_err(|e| e.to_string())?);
        }
        let cache = slot.as_ref().expect("opened above");
        let db_err = |e: tephra_cache::CacheError| e.to_string();
        let mut reset = 0;
        let mut readopted = 0;
        // The reverse crash: an eviction claimed the row (`remote`) and was
        // cut short before it deleted the file. A file that is there and is
        // still what Tephra wrote is `cached` again, so it counts toward
        // the cap and the next pass can drop it; a file that differs is
        // somebody's edit and is left exactly as it is.
        for (name, path) in cache.remote_items_used_before().map_err(db_err)? {
            let (Some(row), Some(source)) = (
                cache.remote_fetch_row(&name, &path).map_err(db_err)?,
                cache.remote_source(&name).map_err(db_err)?,
            ) else {
                continue;
            };
            let sibling = cache
                .remote_has_case_sibling(&name, &path)
                .map_err(db_err)?;
            let layout = Layout::of(&self.hub, &source, &row, sibling);
            let Ok(Some(found)) = layout.existing(false) else {
                continue;
            };
            let stamp = cache.remote_copy_stamp(&name, &path).map_err(db_err)?;
            if copy_is_unedited(
                &source.kind,
                row.sha.as_deref(),
                stamp.as_deref(),
                &found.path,
            ) && retry_cache_on_busy(&self.gate, || cache.readopt_remote_copy(&name, &path))
                .map_err(db_err)?
            {
                readopted += 1;
            }
        }
        for item in cache.remote_evict_items().map_err(db_err)? {
            if item.state != ITEM_STATE_CACHED {
                continue;
            }
            let (Some(row), Some(source)) = (
                cache
                    .remote_fetch_row(&item.source, &item.path)
                    .map_err(db_err)?,
                cache.remote_source(&item.source).map_err(db_err)?,
            ) else {
                continue;
            };
            let sibling = cache
                .remote_has_case_sibling(&item.source, &item.path)
                .map_err(db_err)?;
            let layout = Layout::of(&self.hub, &source, &row, sibling);
            // Only a clean "not there" counts: a symlink in the way or an
            // unreadable folder is not a missing file.
            if matches!(layout.existing(false), Ok(None))
                && retry_cache_on_busy(&self.gate, || {
                    cache.forget_remote_copy(&item.source, &item.path)
                })
                .map_err(db_err)?
            {
                reset += 1;
            }
        }
        Ok((readopted, reset))
    }

    /// One pass (blocking). See the module documentation.
    pub(crate) fn pass(&self) -> Result<PassReport, String> {
        let mut slot = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(Cache::open(&self.cache_path).map_err(|e| e.to_string())?);
        }
        let cache = slot.as_ref().expect("opened above");
        let hub = &self.hub;

        let now = now_secs(&*hub.clock);
        let idle = hub.idle_secs;
        let limit = hub.limit_bytes();
        let items = cache.remote_evict_items().map_err(|e| e.to_string())?;
        let plan = plan_eviction(&items, limit, now, idle);
        let mut report = PassReport::default();
        for pick in &plan.picks {
            let Some(item) = items
                .iter()
                .find(|i| i.source == pick.source && i.path == pick.path)
            else {
                continue;
            };
            match self.evict_one(cache, pick, now - idle) {
                Ok(true) => {
                    report.dropped += 1;
                    report.freed_bytes += item.bytes();
                }
                Ok(false) => report.skipped += 1,
                Err(e) => {
                    report.skipped += 1;
                    tracing::warn!(source = %pick.source, error = %e, "a cached file could not be dropped");
                }
            }
        }
        if report.dropped > 0 {
            tracing::info!(
                files = report.dropped,
                bytes = report.freed_bytes,
                limit_bytes = limit,
                "cache files dropped back to remote"
            );
        }
        Ok(report)
    }

    /// Runs a pass for the timer and logs what is new.
    fn tick(&self) {
        match self.pass() {
            Ok(_) => *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = None,
            Err(e) => {
                let mut last = self.last_error.lock().unwrap_or_else(|p| p.into_inner());
                if last.as_deref() != Some(e.as_str()) {
                    tracing::warn!(error = %e, "the cache eviction pass could not run");
                    *last = Some(e);
                }
            }
        }
    }

    /// Drops one file. `Ok(true)` when it went, `Ok(false)` when it was left
    /// as it is.
    fn evict_one(&self, cache: &Cache, pick: &EvictPick, cutoff: i64) -> Result<bool, String> {
        let hub = &self.hub;
        let db_err = |e: tephra_cache::CacheError| e.to_string();
        let source = &pick.source;
        let rel = &pick.path;

        let Some(row) = cache.remote_fetch_row(source, rel).map_err(db_err)? else {
            return Ok(false);
        };
        let Some(remote_source) = cache.remote_source(source).map_err(db_err)? else {
            return Ok(false);
        };
        if row.state != ITEM_STATE_CACHED {
            return Ok(false);
        }
        let case_sibling = cache.remote_has_case_sibling(source, rel).map_err(db_err)?;
        let layout = Layout::of(hub, &remote_source, &row, case_sibling);
        let found = layout.existing(false).map_err(|f| f.message)?;

        // A clone with git data of its own (`clone/.git`): its files are
        // tracked by git, and deleting one would show up as a change that
        // is not pushed. Nothing in it is dropped.
        if remote_source.kind == GITHUB_KIND
            && std::fs::symlink_metadata(layout.cache_dir.join("clone").join(".git")).is_ok()
        {
            tracing::info!(source = %source, "the clone has git data; its files are not dropped from the cache");
            return Ok(false);
        }

        // 1. A copy that is not what Tephra wrote is somebody's edit: the
        // watcher may not have reported it yet, so the row does not say.
        let stamp = cache.remote_copy_stamp(source, rel).map_err(db_err)?;
        let is_remote_bytes = |path: &std::path::Path| {
            copy_is_unedited(
                &remote_source.kind,
                row.sha.as_deref(),
                stamp.as_deref(),
                path,
            )
        };
        if let Some(found) = &found {
            if !is_remote_bytes(&found.path) {
                tracing::info!(source = %source, file = %printable(rel), "a cached file differs from the remote's and is kept");
                return Ok(false);
            }
        }
        let before = found
            .as_ref()
            .and_then(|f| std::fs::symlink_metadata(&f.path).ok())
            .map(|m| (m.len(), m.modified().ok()));

        let hook = self.hook.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(hook) = hook {
            hook(source, rel);
        }

        // 2. The file's slot.
        let Role::Lead(guard) = join_or_lead(&hub.fetches, source, rel) else {
            return Ok(false);
        };

        // 3. The claim.
        let needs_off = pick.reason == EvictReason::CacheOff;
        let claimed = retry_cache_on_busy(&self.gate, || {
            cache.drop_remote_copy(source, rel, cutoff, needs_off)
        })
        .map_err(db_err);
        let outcome = || {
            Err(Arc::new(failure(
                EVICTED,
                "the file was dropped from the cache",
            )))
        };
        let claimed = match claimed {
            Ok(c) => c,
            Err(e) => {
                guard.publish(outcome());
                return Err(e);
            }
        };
        if !claimed {
            guard.publish(outcome());
            return Ok(false);
        }

        // 4. The delete.
        let result = match &found {
            None => Ok(()),
            Some(found) => {
                let after = std::fs::symlink_metadata(&found.path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok()));
                // Checked again now the row is claimed: a save that landed
                // since the first look is somebody's edit, not ours to delete.
                if after != before || !is_remote_bytes(&found.path) {
                    Err("it changed while it was checked".to_string())
                } else {
                    match hub.fetches.disk.remove_file(&found.path) {
                        Ok(()) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(format!("the disk gave an error ({})", e.kind())),
                    }
                }
            }
        };
        match result {
            Ok(()) => {
                if let Some(found) = &found {
                    remove_empty_dirs(&found.path, &layout.cache_dir);
                }
                // The file is gone, and so is what is known of its bytes.
                let _ =
                    retry_cache_on_busy(&self.gate, || cache.clear_remote_copy_stamp(source, rel));
                guard.publish(outcome());
                Ok(true)
            }
            Err(why) => {
                // The copy stays on disk: the item says so again.
                let _ = retry_cache_on_busy(&self.gate, || cache.restore_remote_copy(source, rel));
                guard.publish(outcome());
                Err(format!("{}: {why}", printable(rel)))
            }
        }
    }
}

/// Whether the file at `path` is still what Tephra put there: a GitHub copy
/// by the blob sha of the listing, a Drive copy by the hash noted when it
/// was written (`stamp`; a Drive listing has no checksum). A copy with
/// nothing to compare it with is not unedited: it is kept.
fn copy_is_unedited(
    kind: &str,
    sha: Option<&str>,
    stamp: Option<&str>,
    path: &std::path::Path,
) -> bool {
    let want = if kind == GITHUB_KIND { sha } else { stamp };
    want.is_some_and(|want| hash_file(path).is_ok_and(|(_, got)| got == want))
}

/// Removes the folders `file` leaves empty, innermost first, stopping at
/// `root` (the source's cache directory, and the GitHub clone folder below
/// it, are never removed).
fn remove_empty_dirs(file: &std::path::Path, root: &std::path::Path) {
    let clone_root = root.join("clone");
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || d == clone_root || !d.starts_with(root) {
            break;
        }
        // Fails when the folder is not empty (or not a plain folder).
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

// ---- what the wire says ----------------------------------------------------------------

/// One source's line of `cache.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheSourceWire {
    pub name: String,
    pub cache: bool,
    pub cache_bytes: u64,
    pub git_bytes: u64,
}

/// `cache.status`: what the content cache takes, against its cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheStatusWire {
    /// Bytes of every cached copy of every source. `.git` is not in it.
    pub cache_bytes: u64,
    /// The cap in bytes, and as written in the config file.
    pub limit_bytes: u64,
    pub limit: String,
    /// `cache_bytes` is above the cap right now (a file used in the last
    /// ten minutes, or a pinned or edited one, is not dropped to fix it).
    pub over_limit: bool,
    /// What pinned files take. Never dropped.
    pub pinned_bytes: u64,
    /// Set when the pinned files alone are over the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// The `.git` folders of every clone, on a line of their own: not
    /// counted toward the cap, never evicted.
    pub git_bytes: u64,
    /// Seconds a file must be unused before it may be dropped.
    pub idle_seconds: i64,
    pub sources: Vec<CacheSourceWire>,
}

/// `cache.set_limit`: the status after the change and the pass it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheSetLimitWire {
    /// The limit before this call.
    pub previous: String,
    pub dropped: usize,
    pub freed_bytes: u64,
    #[serde(flatten)]
    pub status: CacheStatusWire,
}

/// `source.set_cache`: the flag after the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceCacheWire {
    pub name: String,
    pub cache: bool,
    /// The flag before this call.
    pub was: bool,
    /// Files the pass this call ran sent back to `remote`.
    pub dropped: usize,
}

fn refused(reason: &'static str, message: impl Into<String>) -> PromoteRpcError {
    PromoteRpcError::Refused {
        reason,
        message: message.into(),
    }
}

impl SourceRegistry {
    /// Starts the eviction timer. Called once, when the hub is set.
    pub(crate) fn start_eviction(&self, hub: Arc<RemoteHub>) {
        let tick = hub.evict_tick;
        let runner = Arc::new(EvictRunner::new(
            self.cache_path.clone(),
            self.gate.clone(),
            hub,
        ));
        if self.evict.set(runner.clone()).is_err() {
            return;
        }
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let runner = runner.clone();
                let _ = tokio::task::spawn_blocking(move || runner.tick()).await;
            }
        });
    }

    /// Test seam: see [`EvictHook`]. Not for the daemon's own use.
    #[doc(hidden)]
    pub fn set_evict_hook(&self, hook: Option<EvictHook>) {
        if let Some(runner) = self.evict.get() {
            *runner.hook.lock().unwrap_or_else(|p| p.into_inner()) = hook;
        }
    }

    /// Start-up (see [`EvictRunner::reconcile`]).
    pub(crate) fn reconcile_cache_copies(&self) {
        let Some(runner) = self.evict.get() else {
            return;
        };
        match runner.reconcile() {
            Ok((0, 0)) => {}
            Ok((cached, remote)) => tracing::info!(
                cached_again = cached,
                remote_again = remote,
                "cached items were reconciled with the files on disk"
            ),
            Err(e) => tracing::warn!(error = %e, "could not check the cached copies at start"),
        }
    }

    fn evictor(&self) -> Result<(&Arc<EvictRunner>, &Arc<RemoteHub>), PromoteRpcError> {
        match (self.evict.get(), self.remote.get()) {
            (Some(runner), Some(hub)) => Ok((runner, hub)),
            _ => Err(refused(
                "unavailable",
                "this daemon has no remote support, so there is no remote cache",
            )),
        }
    }

    /// `cache.status`. Blocking.
    pub fn cache_status(&self) -> Result<CacheStatusWire, PromoteRpcError> {
        let (_, hub) = self.evictor()?;
        let failed = |e: String| PromoteRpcError::Failed(format!("the index gave an error: {e}"));
        let (cache_bytes, pinned_bytes, sources) = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            let total = cache
                .remote_total_cache_bytes()
                .map_err(|e| failed(e.to_string()))?;
            let pinned = cache
                .remote_pinned_bytes()
                .map_err(|e| failed(e.to_string()))?;
            let rows = cache.remote_sources().map_err(|e| failed(e.to_string()))?;
            let mut sources = Vec::new();
            for row in rows {
                sources.push(CacheSourceWire {
                    cache_bytes: cache.remote_cache_bytes(&row.name).unwrap_or(0),
                    git_bytes: hub.git_bytes(&row.name),
                    name: row.name,
                    cache: row.cache,
                });
            }
            (total, pinned, sources)
        };
        let (limit, limit_bytes) = {
            let l = hub.cache_limit.lock().unwrap_or_else(|p| p.into_inner());
            (l.text.clone(), l.bytes)
        };
        let warning = (pinned_bytes > limit_bytes).then(|| {
            format!(
                "pinned files alone take {pinned_bytes} bytes, more than the limit of \
                 {limit_bytes}: pinned files are never dropped, so the cache stays over it \
                 (unpinned files are dropped first)"
            )
        });
        Ok(CacheStatusWire {
            cache_bytes,
            limit_bytes,
            limit,
            over_limit: cache_bytes > limit_bytes,
            pinned_bytes,
            warning,
            git_bytes: sources.iter().map(|s| s.git_bytes).sum(),
            idle_seconds: hub.idle_secs,
            sources,
        })
    }

    /// `cache.set_limit {limit}`: refuses a value that does not parse, or
    /// one it cannot save to the config file, with the old limit kept;
    /// otherwise saves it, changes it live and runs a pass. Blocking.
    pub fn set_cache_limit(&self, text: &str) -> Result<CacheSetLimitWire, PromoteRpcError> {
        let (runner, hub) = self.evictor()?;
        let text = text.trim();
        let (old_text, _) = {
            let l = hub.cache_limit.lock().unwrap_or_else(|p| p.into_inner());
            (l.text.clone(), l.bytes)
        };
        let bytes = crate::cache_limit::parse_cache_limit(text).map_err(|e| {
            refused(
                "bad_limit",
                format!(
                    "{} is not a usable cache limit: it {e}; the limit stays {old_text}",
                    printable(text)
                ),
            )
        })?;
        if let Some(save) = &hub.limit_saver {
            save(text).map_err(|e| {
                refused(
                    "not_saved",
                    format!("the limit could not be saved ({e}); it stays {old_text}"),
                )
            })?;
        }
        *hub.cache_limit.lock().unwrap_or_else(|p| p.into_inner()) =
            crate::remote_github::CacheLimit {
                text: text.to_string(),
                bytes,
            };
        let report = runner.pass().map_err(|e| {
            PromoteRpcError::Failed(format!(
                "the limit is set, but the cache pass could not run: {e}"
            ))
        })?;
        Ok(CacheSetLimitWire {
            previous: old_text,
            dropped: report.dropped,
            freed_bytes: report.freed_bytes,
            status: self.cache_status()?,
        })
    }

    /// `source.set_cache {name, cache}`: the per-source flag (G20). Turning
    /// it off runs a pass at once; a file is dropped once it has been idle
    /// for the window, not before. Blocking.
    pub fn set_source_cache(
        &self,
        name: &str,
        on: bool,
    ) -> Result<SourceCacheWire, PromoteRpcError> {
        let (runner, _) = self.evictor()?;
        let was = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            retry_cache_on_busy(&self.gate, || cache.set_remote_source_cache(name, on))
                .map_err(|e| PromoteRpcError::Failed(format!("the index gave an error: {e}")))?
        };
        let Some(was) = was else {
            let local = {
                let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                cache.get_source(name).ok().flatten().is_some()
            };
            return Err(if local {
                refused(
                    "not_remote",
                    format!(
                        "{} is a local source: the cache setting is only for remote sources",
                        printable(name)
                    ),
                )
            } else {
                refused(
                    "unknown_source",
                    format!("no source is named {}", printable(name)),
                )
            });
        };
        let dropped = if on {
            0
        } else {
            runner
                .pass()
                .map_err(|e| {
                    PromoteRpcError::Failed(format!(
                        "the flag is set, but the cache pass could not run: {e}"
                    ))
                })?
                .dropped
        };
        Ok(SourceCacheWire {
            name: name.to_string(),
            cache: on,
            was,
            dropped,
        })
    }
}
