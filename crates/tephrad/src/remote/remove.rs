//! Removing a remote source (atlas-27 PR-13): `source.unregister` for a GitHub
//! or Drive source.
//!
//! The decision is the daemon's. Without `force` a source that holds work the
//! remote does not have is refused, with the counts, and nothing changes.
//! With `force` that work is discarded with everything else Tephra keeps for
//! the source. Either way a removal makes no request to GitHub or Drive,
//! never touches the credential store, and never writes to a git clone
//! except by deleting its directory.
//!
//! The order, under the one removal lock (two removals never interleave):
//!
//! 1. A look at the clone or the cache as it is on the disk now (no
//!    network), and the unsent work from the cache's own count. Unsent work
//!    is refused here, with nothing paused.
//! 2. The source's workers are paused ([`Removal::is_removing`]): the poller
//!    makes no request, the upload job sends nothing, a download stops at its
//!    next chunk, and a pass over the clone waits on the clone's lock, which
//!    the removal holds. The downloads in flight are waited for.
//! 3. The test pause point, then the same look and count again. An edit
//!    saved since step 1 is refused here; the workers resume at once.
//! 4. Only now does anything go: the directory `remote/<name>/` (a link in
//!    it is unlinked, never followed), then the watchers, then every row of
//!    the source in ONE transaction, then the in-memory state, then ONE
//!    `removed` event. A failure to delete the directory is reported with
//!    the source and its rows untouched, so a retry finishes the job.
//!
//! A job that outlives its source (a poller asleep until its next turn) ends
//! when it next looks: the source row is gone, or its generation moved.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tephra_cache::UnsentWork;

use crate::registry::{
    retry_cache_on_busy, RegistryError, SourceChange, SourceChangeKind, SourceRegistry,
};
use crate::remote::clone_sync::{CloneCtx, Pass};
use crate::remote_github::{refused, RemoteHub, DRIVE_KIND, GITHUB_KIND};

/// How long a removal waits for the downloads of the source to stop. They
/// stop at their next chunk; one stuck on a silent network is not waited for
/// (the leader removes its own file when it ends, see `fetch.rs`).
const DOWNLOADS_STOP_WITHIN: Duration = Duration::from_secs(5);

/// How many times the directory is deleted again when something wrote into
/// it while it was being deleted.
const DELETE_TRIES: usize = 5;

type Hook = Arc<dyn Fn() + Send + Sync>;

/// What the hub knows of removals.
#[derive(Default)]
pub(crate) struct Removal {
    /// One removal at a time.
    lock: Mutex<()>,
    /// The sources whose removal is under way: their workers hold still.
    pausing: Mutex<HashSet<String>>,
    /// A number per source name that moves when the source is removed. A
    /// job holds the number it started with and ends when it moved, so a
    /// source added again under the name is never served by the old job.
    generations: Mutex<HashMap<String, u64>>,
    /// Test-only: called between the workers being paused and the second
    /// look at the unsent work.
    hook: Mutex<Option<Hook>>,
}

impl RemoteHub {
    /// True while the removal of `source` is under way. The workers of the
    /// source make no request and write nothing then.
    pub(crate) fn is_removing(&self, source: &str) -> bool {
        self.removal
            .pausing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(source)
    }

    /// The generation of the source named `source`. A job reads it when it
    /// starts and ends when it is not the same any more.
    pub(crate) fn generation(&self, source: &str) -> u64 {
        self.removal
            .generations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(source)
            .copied()
            .unwrap_or(0)
    }

    /// Test-only: `f` runs in every removal that is not forced, after the
    /// source's workers are paused and before the unsent work is looked at
    /// for the last time. A test saves an edit there to prove the second
    /// look refuses the removal.
    #[doc(hidden)]
    pub fn set_removal_pause_hook(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.removal.hook.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(f));
    }

    fn run_removal_hook(&self) {
        let hook = self
            .removal
            .hook
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Marks the source as being removed until it is dropped, however the
/// removal ends.
struct Paused<'a> {
    hub: &'a RemoteHub,
    name: &'a str,
}

impl<'a> Paused<'a> {
    fn new(hub: &'a RemoteHub, name: &'a str) -> Self {
        hub.removal
            .pausing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string());
        Paused { hub, name }
    }
}

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        self.hub
            .removal
            .pausing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.name);
    }
}

/// `true` for a name that is one plain path segment: what a source's
/// directory under `remote/` is called. Anything else (`..`, a separator, a
/// drive prefix, an empty name, a NUL) could name a place outside `remote/`.
pub(crate) fn is_plain_dir_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return false;
    }
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(n)), None) if n == name
    )
}

/// Deletes `dir` and everything in it. A link inside it is unlinked, never
/// followed; if `dir` is itself a link only the link goes. A directory that
/// is not there is not an error. Tries again a few times when something
/// writes into it meanwhile.
pub(crate) fn remove_dir_safely(dir: &Path) -> io::Result<()> {
    let mut last = None;
    for _ in 0..DELETE_TRIES {
        match std::fs::symlink_metadata(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
            Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => {
                return match std::fs::remove_file(dir) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                };
            }
            Ok(_) => {}
        }
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("the directory could not be deleted")))
}

/// What the person is told about a directory that could not be deleted:
/// the kind of failure, never a path (the message may reach a client).
fn io_reason(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        other => other.to_string(),
    }
}

impl SourceRegistry {
    /// `source.unregister` of the remote source `name` (see the module
    /// documentation). Returns `NotFound` when the source is gone by the
    /// time this removal holds the lock (another removal got there first).
    pub(crate) fn remove_remote_source(
        &self,
        hub: &Arc<RemoteHub>,
        name: &str,
        force: bool,
    ) -> Result<(), RegistryError> {
        if !is_plain_dir_name(name) {
            return Err(refused(
                "invalid_name",
                format!("\"{name}\" is not a name a source can have"),
            ));
        }
        let _one = hub.removal.lock.lock().unwrap_or_else(|p| p.into_inner());
        let source = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            cache.remote_source(name)?
        };
        let Some(source) = source else {
            return Err(RegistryError::NotFound(name.to_string()));
        };

        // 1. The look before anything is paused.
        if !force {
            self.refuse_unsent_work(hub, name, &source.kind, false)?;
        }

        // 2. Hold the workers still, and wait for the downloads.
        let _paused = Paused::new(hub, name);
        hub.fetches.wait_idle(name, DOWNLOADS_STOP_WITHIN);
        let clone_lock = hub.clone_lock(name);
        let _clone = clone_lock.lock().unwrap_or_else(|p| p.into_inner());

        // 3. The look again, just before the delete.
        if !force {
            hub.run_removal_hook();
            self.refuse_unsent_work(hub, name, &source.kind, true)?;
        }

        // 4. Delete: the directory first (so a failure leaves the source
        // whole), then the rows.
        if let Err(e) = remove_dir_safely(&hub.cache_dir(name)) {
            tracing::warn!(source = %name, kind = ?e.kind(), "the directory of a source being removed could not be deleted");
            return Err(RegistryError::Remote(
                crate::remote_github::RemoteAddError::Failed(format!(
                    "could not delete the files Tephra keeps for \"{name}\" ({}); the source is still registered, try again",
                    io_reason(&e)
                )),
            ));
        }
        crate::remote::upload::stop_watch(hub, name);
        crate::remote::clone_sync::stop_watch(hub, name);
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            retry_cache_on_busy(&self.gate, || cache.delete_remote_source(name))?;
        }
        self.forget_remote_source_state(hub, name);
        self.forget_runtime_state(name);
        self.announce_sources_changed(vec![SourceChange::new(SourceChangeKind::Removed, name)]);
        Ok(())
    }

    /// What the hub holds in memory for a source that is gone, so a source
    /// added again under the name starts with nothing of the old one.
    fn forget_remote_source_state(&self, hub: &RemoteHub, name: &str) {
        *hub.removal
            .generations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(name.to_string())
            .or_insert(0) += 1;
        hub.polling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        let slot = hub
            .uploaders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        if let Some(slot) = slot {
            // The job looks at its generation and ends.
            slot.poke(false);
        }
        hub.clone_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
    }

    /// Looks at the source as it is on the disk now (no network) and refuses
    /// when it holds unsent work. `locked` says the caller holds the clone's
    /// lock.
    fn refuse_unsent_work(
        &self,
        hub: &Arc<RemoteHub>,
        name: &str,
        kind: &str,
        locked: bool,
    ) -> Result<(), RegistryError> {
        if kind == GITHUB_KIND {
            let ctx = CloneCtx {
                name: name.to_string(),
                cache_path: self.cache_path.clone(),
                gate: self.gate.clone(),
                events_tx: self.events_tx.clone(),
                hub: hub.clone(),
            };
            if locked {
                ctx.recount_locked();
            } else {
                ctx.run(Pass::Recount);
            }
        } else if kind == DRIVE_KIND {
            if let Err(why) = self.scan_drive_edits_now(name) {
                tracing::warn!(source = %name, why, "the cache of a Drive source could not be looked at before its removal");
            }
        }
        let work = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            retry_cache_on_busy(&self.gate, || cache.remote_unsent_work(name))?
        };
        if work.is_empty() {
            Ok(())
        } else {
            Err(RegistryError::UnsentWork {
                name: name.to_string(),
                work,
            })
        }
    }

    /// Start-up: deletes every directory under `remote/` that no remote
    /// source owns. A crash between a removal's two deletes leaves one; a
    /// source added later under its name must begin empty.
    pub(crate) fn remove_orphan_remote_dirs(&self, hub: &RemoteHub) {
        let owned: HashSet<String> = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            match cache.remote_sources() {
                Ok(rows) => rows.into_iter().map(|r| r.name).collect(),
                // Not knowing who owns what, nothing is deleted.
                Err(_) => return,
            }
        };
        let root = hub.cache_dir("");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if owned.contains(&name) || !is_plain_dir_name(&name) {
                continue;
            }
            match remove_dir_safely(&entry.path()) {
                Ok(()) => {
                    tracing::info!(source = %name, "a directory no source owns was removed (a removal was cut short)")
                }
                Err(e) => {
                    tracing::warn!(source = %name, kind = ?e.kind(), "a directory no source owns could not be removed")
                }
            }
        }
    }
}

/// The unsent work as the wire says it (`data` of the `unsent_work` error).
pub(crate) fn unsent_work_data(work: &UnsentWork) -> serde_json::Value {
    serde_json::json!({
        "reason": "unsent_work",
        "not_pushed": work.not_pushed,
        "pending_upload": work.pending_upload,
        "needs_attention": work.needs_attention,
    })
}

/// The sentence of the refusal: what is held, and that `force` discards it.
pub(crate) fn unsent_work_message(name: &str, work: &UnsentWork) -> String {
    let mut held = Vec::new();
    if work.not_pushed > 0 {
        held.push(format!("{} not pushed", work.not_pushed));
    }
    if work.pending_upload > 0 {
        held.push(format!("{} pending upload", work.pending_upload));
    }
    if work.needs_attention > 0 {
        held.push(format!("{} needs attention", work.needs_attention));
    }
    format!(
        "\"{name}\" holds work that has not reached the remote ({}); \
         removing it with force discards your local edits and the cache and never touches the remote",
        held.join(", ")
    )
}
