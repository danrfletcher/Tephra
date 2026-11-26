//! Dan's edits to cached Google Drive files go back to Drive (atlas-27
//! PR-11).
//!
//! One job runs per Drive source (started with its poller). It does two
//! things:
//!
//! 1. **Sees edits.** A watcher on the source's content cache (and a scan
//!    when the job starts, so an edit made while the daemon was off is
//!    found) looks for a cached copy whose bytes are no longer the ones
//!    Tephra put there: its hash differs from the stamp noted at fetch time
//!    (or at the last upload). A file that is back in the cache after the
//!    cache dropped it (state `remote`, no stamp, a file on disk: Dan saved
//!    a file the cache had evicted, E24) is an edit too. Either way the item
//!    becomes `pending_upload` and a row is added to `upload_queue`, based on
//!    the last recorded revision. A second save changes nothing: the bytes
//!    are read when they are sent.
//! 2. **Sends them.** For every due row the worker reads Drive's current
//!    head revision first:
//!    - the same bytes are already there (a save that changed nothing, an
//!      upload that landed before a kill): the row ends and nothing is sent;
//!    - the head is the revision the edit was based on: the bytes are
//!      uploaded as a new revision, and that revision becomes the item's;
//!    - the head moved: NO bytes are sent. The item is held as
//!      `needs_attention: changed on Drive` until `resolve` (below).
//!      A kill after the bytes landed leaves the head moved too: the MD5 of
//!      the bytes being sent is noted in the row before they go, so a retry
//!      tells its own upload from somebody else's change and finishes
//!      without a second revision (E14).
//!
//!    A failure that may pass (offline, 5xx, 429, no sign-in) retries with a
//!    doubling wait, 1 s up to 60 s and for ever; the wait is stored in the
//!    row and measured by the hub's clock. Drive saying no for good (the file
//!    is gone, Tephra may not write it) holds the item with Drive's reason.
//!    The local bytes are never dropped.
//!
//! `resolve` clears a conflict: `mine` queues the edit again on the head the
//! conflict saw (a head that moved again is refused again), `theirs` keeps
//! the edit as `<name>.mine` and fetches the file from Drive.
//!
//! Nothing here deletes, trashes, moves or renames anything on Drive: the
//! only call that writes is `DriveUploader::replace_media`, new content for
//! a file that exists. Upload, retry and `resolve --mine` read no file body
//! from Drive; only `--theirs` downloads, and only the one file.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use tephra_cache::{
    Cache, CacheError, DriveConflict, QueuedEdit, RemoteFetchRow, RemoteSourceRow, UploadEntry,
    UploadLanded, ITEM_STATE_CACHED, ITEM_STATE_NEEDS_ATTENTION, ITEM_STATE_PENDING_UPLOAD,
    ITEM_STATE_PINNED, ITEM_STATE_REMOTE, REASON_CHANGED_ON_DRIVE, UPLOAD_REFUSED_PREFIX,
};
use tephra_remote::drive::{Md5Hasher, UploadBody};
use tephra_remote::github::GitBlobHasher;
use tephra_remote::RemoteError;

use crate::ignore::IgnoreRules;
use crate::registry::{retry_cache_on_busy, DaemonEvent, SourceRegistry};
use crate::remote::fetch::Layout;
use crate::remote::safe_path;
use crate::remote_github::{now_secs, printable, RemoteHub, DRIVE_KIND};
use crate::watcher::{self, WatcherHandle};
use crate::writer_gate::WriterGate;

/// How long `resolve --mine` waits for the upload it queued, in real time.
/// After that the answer says the edit is still queued.
pub(crate) const RESOLVE_WAIT: Duration = Duration::from_secs(10);

/// A look at everything the cache holds even when no watcher event came: a
/// safety net for an event the OS dropped. Real time, whatever the clock.
const SAFETY_SCAN_EVERY: Duration = Duration::from_secs(5);

/// The bound on the one metadata read `resolve --theirs` makes.
const HEAD_READ_BOUND: Duration = Duration::from_secs(30);

/// What the worker last saw of each file it has nothing to send for: its
/// length and modified time.
type CleanMarks = HashMap<String, (u64, Option<SystemTime>)>;

/// What a Drive source's job shares with the watcher and with `resolve`.
pub(crate) struct UploadSlot {
    /// The job should look at the cache for edits.
    dirty: AtomicBool,
    /// The job should look at the queue now.
    wake: tokio::sync::Notify,
    /// How many passes over the queue have finished. A test waits on it.
    pub(crate) passes: AtomicUsize,
    /// The watcher of the cache is running, or is being started.
    watching: AtomicBool,
}

impl UploadSlot {
    fn new() -> Self {
        UploadSlot {
            dirty: AtomicBool::new(true),
            wake: tokio::sync::Notify::new(),
            passes: AtomicUsize::new(0),
            watching: AtomicBool::new(false),
        }
    }

    pub(crate) fn poke(&self, scan: bool) {
        if scan {
            self.dirty.store(true, Ordering::SeqCst);
        }
        self.wake.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Looking at a file.
// ---------------------------------------------------------------------------

/// What a file's bytes are, in one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Digest {
    pub(crate) len: u64,
    /// Drive's `md5Checksum` of those bytes.
    pub(crate) md5: String,
    /// The git blob sha, which is what the fetch stamp holds.
    pub(crate) sha: String,
}

const READ_BUFFER: usize = 64 * 1024;

/// Reads `path` once, in pieces. A file that changes length while it is read
/// is an error (the caller looks again).
pub(crate) fn digest_file(path: &Path) -> io::Result<Digest> {
    let len = fs::metadata(path)?.len();
    let mut file = fs::File::open(path)?;
    let mut md5 = Md5Hasher::new();
    let mut sha = GitBlobHasher::new(len);
    let mut buf = vec![0u8; READ_BUFFER];
    let mut seen = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        seen += n as u64;
        md5.update(&buf[..n]);
        sha.update(&buf[..n]);
    }
    if seen != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file changed while it was read",
        ));
    }
    Ok(Digest {
        len,
        md5: md5.finish(),
        sha: sha.finish(),
    })
}

fn busy<T>(gate: &WriterGate, f: impl Fn() -> Result<T, CacheError>) -> Result<T, String> {
    retry_cache_on_busy(gate, f).map_err(|e| e.to_string())
}

/// Whether a file or a link is at `path` (nothing is followed).
fn something_at(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Where the file of `row` is in the cache of `source`, if there is a
/// regular file there.
fn locate(
    hub: &RemoteHub,
    source: &RemoteSourceRow,
    row: &RemoteFetchRow,
    cache: &Cache,
) -> Result<Option<PathBuf>, String> {
    let sibling = cache
        .remote_has_case_sibling(&row.source, &row.path)
        .map_err(|e| e.to_string())?;
    let layout = Layout::of(hub, source, row, sibling);
    match layout.existing(false) {
        Ok(found) => Ok(found.map(|f| f.path)),
        Err(e) => Err(e.message),
    }
}

/// Cheap test, before anything is hashed or asked of the database: could a
/// file of this item be on disk at all?
fn maybe_on_disk(cache_dir: &Path, rel: &str) -> bool {
    let mut plain = cache_dir.to_path_buf();
    plain.extend(safe_path::safe_segments(rel));
    let mut distinct = cache_dir.to_path_buf();
    distinct.extend(safe_path::case_distinct(rel));
    something_at(&plain) || something_at(&distinct)
}

// ---------------------------------------------------------------------------
// The job.
// ---------------------------------------------------------------------------

struct UploadJob {
    name: String,
    cache_path: PathBuf,
    gate: Arc<WriterGate>,
    events_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
    hub: Arc<RemoteHub>,
    slot: Arc<UploadSlot>,
    conn: Arc<Mutex<Option<Cache>>>,
    /// Files seen to be what Tephra put there, by path: their length and
    /// modification time then. A file that still has them is not hashed
    /// again.
    clean: Arc<Mutex<CleanMarks>>,
    /// The source's generation when this job started: a removal moves it and
    /// the job ends without touching what a source of the same name added
    /// later has.
    generation: u64,
}

/// What came of one try at a queue row.
enum Attempt {
    /// The row is done, or was not ours any more.
    Settled,
    /// The row went on a different footing and the pass goes on.
    Failed,
}

impl SourceRegistry {
    /// Starts the upload job of the Drive source `name`, once per source.
    pub(crate) fn spawn_drive_upload(&self, name: &str) {
        let Some(hub) = self.remote.get().cloned() else {
            return;
        };
        let slot = {
            let mut jobs = hub.uploaders.lock().unwrap_or_else(|p| p.into_inner());
            if jobs.contains_key(name) {
                return;
            }
            let slot = Arc::new(UploadSlot::new());
            jobs.insert(name.to_string(), slot.clone());
            slot
        };
        let generation = hub.generation(name);
        let job = UploadJob {
            name: name.to_string(),
            cache_path: self.cache_path.clone(),
            gate: self.gate.clone(),
            events_tx: self.events_tx.clone(),
            hub,
            slot,
            conn: Arc::new(Mutex::new(None)),
            clean: Arc::new(Mutex::new(HashMap::new())),
            generation,
        };
        self.runtime.spawn(job.run());
    }
}

impl SourceRegistry {
    /// Looks at the cache of the Drive source `name` now and queues every
    /// edit it finds: the upload job's own scan, run on the caller's thread
    /// so a removal does not miss a save the watcher has not reported yet.
    /// Makes no request. Returns how many edits were queued.
    pub(crate) fn scan_drive_edits_now(&self, name: &str) -> Result<usize, String> {
        let hub = self
            .remote
            .get()
            .cloned()
            .ok_or_else(|| "no remote support".to_string())?;
        let generation = hub.generation(name);
        let job = UploadJob {
            name: name.to_string(),
            cache_path: self.cache_path.clone(),
            gate: self.gate.clone(),
            events_tx: self.events_tx.clone(),
            hub,
            slot: Arc::new(UploadSlot::new()),
            conn: Arc::new(Mutex::new(None)),
            clean: Arc::new(Mutex::new(HashMap::new())),
            generation,
        };
        let cache = Cache::open(&self.cache_path).map_err(|e| e.to_string())?;
        job.scan(&cache)
    }
}

impl UploadJob {
    async fn run(self) {
        let started = tokio::time::Instant::now();
        let mut last_safety = started;
        loop {
            // The source was removed (and may be added again): this job is
            // not its job any more, and leaves its maps to the new one.
            if self.superseded() {
                return;
            }
            // Edits first: what a save left is in the queue before the
            // queue is looked at.
            if self.slot.dirty.swap(false, Ordering::SeqCst) && !self.removing() {
                let queued = self.blocking(|job, cache| job.scan(cache)).await;
                if let Ok(Ok(n)) = queued {
                    if n > 0 {
                        self.announce();
                    }
                }
            }
            match self.pass().await {
                Pass::Gone => break,
                Pass::Done => {}
            }
            if self.superseded() {
                return;
            }
            self.slot.passes.fetch_add(1, Ordering::SeqCst);
            self.start_watch();
            if last_safety.elapsed() >= SAFETY_SCAN_EVERY {
                last_safety = tokio::time::Instant::now();
                self.slot.dirty.store(true, Ordering::SeqCst);
            }
            tokio::select! {
                _ = self.slot.wake.notified() => {}
                _ = tokio::time::sleep(self.hub.poll_tick) => {}
            }
        }
        if self.superseded() {
            return;
        }
        stop_watch(&self.hub, &self.name);
        self.hub
            .uploaders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.name);
    }

    fn announce(&self) {
        let _ = self.events_tx.send(DaemonEvent::SourceChanged {
            source: self.name.clone(),
            paths: Vec::new(),
            fixes: Vec::new(),
        });
    }

    /// The source was removed since this job started.
    fn superseded(&self) -> bool {
        self.hub.generation(&self.name) != self.generation
    }

    /// The source's removal is under way: nothing is sent or queued.
    fn removing(&self) -> bool {
        self.hub.is_removing(&self.name)
    }

    /// Runs `f` on the blocking pool with the job's own connection.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&UploadJob, &Cache) -> Result<T, String> + Send + 'static,
    ) -> Result<Result<T, String>, String> {
        let job = UploadJob {
            name: self.name.clone(),
            cache_path: self.cache_path.clone(),
            gate: self.gate.clone(),
            events_tx: self.events_tx.clone(),
            hub: self.hub.clone(),
            slot: self.slot.clone(),
            conn: self.conn.clone(),
            clean: self.clean.clone(),
            generation: self.generation,
        };
        tokio::task::spawn_blocking(move || {
            let conn = job.conn.clone();
            let mut slot = conn.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(Cache::open(&job.cache_path).map_err(|e| e.to_string())?);
            }
            let cache = slot.as_ref().expect("opened above");
            Ok(f(&job, cache))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn now(&self) -> i64 {
        now_secs(&*self.hub.clock)
    }

    /// Starts the watcher of the source's content cache, once, off the
    /// job's thread: the OS can take a long while to set a watch up when
    /// many start together, and the queue must go on meanwhile. A save before
    /// it is up is found by the next safety scan.
    fn start_watch(&self) {
        if self.slot.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let job = UploadJob {
            name: self.name.clone(),
            cache_path: self.cache_path.clone(),
            gate: self.gate.clone(),
            events_tx: self.events_tx.clone(),
            hub: self.hub.clone(),
            slot: self.slot.clone(),
            conn: self.conn.clone(),
            clean: self.clean.clone(),
            generation: self.generation,
        };
        tokio::task::spawn_blocking(move || {
            if !job.ensure_watch() {
                job.slot.watching.store(false, Ordering::SeqCst);
            }
        });
    }

    /// The source still has its row. Reads through the job's connection.
    fn source_exists(&self) -> bool {
        let mut slot = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            match Cache::open(&self.cache_path) {
                Ok(cache) => *slot = Some(cache),
                Err(_) => return false,
            }
        }
        let cache = slot.as_ref().expect("opened above");
        matches!(
            busy(&self.gate, || cache.remote_source(&self.name)),
            Ok(Some(_))
        )
    }

    /// True when the watcher is running afterwards. A save in the cache
    /// marks the job dirty after the debounce.
    fn ensure_watch(&self) -> bool {
        let hub = &self.hub;
        if hub
            .drive_watchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&self.name)
        {
            return true;
        }
        // A source that is gone (or going) has no directory to make: this
        // would bring back the one its removal deleted.
        if self.removing() || self.superseded() || !self.source_exists() {
            return false;
        }
        let dir = hub.cache_dir(&self.name);
        // The directory is made now, so the watcher is already running when
        // the first file is fetched into it: a save right after a fetch is
        // then seen by an event, not only by a later scan.
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!(source = %self.name, error = %e.kind(), "could not make the cache directory of a Drive source");
            return false;
        }
        // The watcher reports canonical paths (a macOS temp dir is a link).
        let Ok(root) = fs::canonicalize(&dir) else {
            return false;
        };
        let Ok(rules) = IgnoreRules::load(&root) else {
            return false;
        };
        // No lock is held while the OS sets the watch up: that is the slow
        // part.
        let tap = match watcher::start(root.clone(), Arc::new(RwLock::new(rules))) {
            Ok(tap) => tap,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "could not watch the cache of a Drive source");
                return false;
            }
        };
        let slot = self.slot.clone();
        let on_batch = move |_batch: HashSet<PathBuf>| slot.poke(true);
        let on_removed = {
            let hub = hub.clone();
            let name = self.name.clone();
            let slot = self.slot.clone();
            move || {
                hub.drive_watchers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&name);
                // The job starts a new one on its next turn.
                slot.watching.store(false, Ordering::SeqCst);
            }
        };
        let handle = watcher::spawn_debounce(
            tap,
            root,
            tokio::runtime::Handle::current(),
            on_batch,
            on_removed,
        );
        hub.drive_watchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(self.name.clone(), handle);
        // A save between the first scan and the watcher is caught by this.
        self.slot.dirty.store(true, Ordering::SeqCst);
        self.slot.wake.notify_one();
        true
    }

    // ---- 1. seeing edits ------------------------------------------------

    /// Looks at every file of the source the cache could hold an edit of.
    /// Returns how many edits were queued.
    fn scan(&self, cache: &Cache) -> Result<usize, String> {
        let Some(source) = busy(&self.gate, || cache.remote_source(&self.name))? else {
            return Ok(0);
        };
        if source.kind != DRIVE_KIND {
            return Ok(0);
        }
        let cache_dir = self.hub.cache_dir(&self.name);
        let candidates = busy(&self.gate, || cache.drive_edit_candidates(&self.name))?;
        let mut queued = 0;
        for (path, state, stamp) in candidates {
            // A held or queued item needs nothing: the bytes are read when
            // they are sent.
            if state == ITEM_STATE_PENDING_UPLOAD || state == ITEM_STATE_NEEDS_ATTENTION {
                continue;
            }
            if !maybe_on_disk(&cache_dir, &path) {
                self.clean
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&path);
                continue;
            }
            let Some(row) = busy(&self.gate, || cache.remote_fetch_row(&self.name, &path))? else {
                continue;
            };
            let Ok(Some(file)) = locate(&self.hub, &source, &row, cache) else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(&file) else {
                continue;
            };
            let seen = (meta.len(), meta.modified().ok());

            let edited = match (state.as_str(), stamp.as_deref()) {
                // A cached copy is an edit once its bytes are not the
                // stamp's. No stamp, nothing to compare with: left alone.
                (ITEM_STATE_CACHED | ITEM_STATE_PINNED, Some(stamp)) => {
                    let known_clean = self
                        .clean
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&path)
                        == Some(&seen);
                    if known_clean {
                        false
                    } else {
                        match digest_file(&file) {
                            Ok(d) if d.sha == stamp => {
                                self.clean
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .insert(path.clone(), seen);
                                false
                            }
                            Ok(_) => true,
                            // Changing under the read: the next event looks again.
                            Err(_) => false,
                        }
                    }
                }
                // A file the cache dropped, back on disk with no stamp:
                // Dan saved it (E24). A stamp that is still there means the
                // eviction is half done and the file is about to go.
                (ITEM_STATE_REMOTE, None) => true,
                _ => false,
            };
            if !edited {
                continue;
            }
            let now = self.now();
            match busy(&self.gate, || {
                cache.queue_drive_edit(&self.name, &path, now)
            })? {
                QueuedEdit::Queued => {
                    queued += 1;
                    self.clean
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&path);
                    // A saved write counts as a use of the file.
                    let _ = busy(&self.gate, || {
                        cache.touch_remote_last_used(&self.name, &path, now)
                    });
                    tracing::info!(source = %self.name, file = %printable(&path), "an edit of a cached Drive file is queued to upload");
                }
                QueuedEdit::AlreadyQueued | QueuedEdit::Held | QueuedEdit::Ignored => {}
            }
        }
        if queued > 0 {
            self.slot.wake.notify_one();
        }
        Ok(queued)
    }

    // ---- 2. sending them ------------------------------------------------

    async fn pass(&self) -> Pass {
        if self.removing() {
            return Pass::Done;
        }
        let now = self.now();
        let name = self.name.clone();
        let loaded = self
            .blocking(move |job, cache| {
                let source = busy(&job.gate, || cache.remote_source(&name))?;
                let due = busy(&job.gate, || cache.due_uploads(&name, now))?;
                Ok((source, due))
            })
            .await;
        let (source, due) = match loaded {
            Ok(Ok((Some(source), due))) => (source, due),
            Ok(Ok((None, _))) => return Pass::Gone,
            Ok(Err(why)) | Err(why) => {
                tracing::warn!(source = %self.name, why, "the upload queue could not be read");
                return Pass::Done;
            }
        };
        for entry in due {
            if self.removing() {
                break;
            }
            match self.attempt(&source, entry).await {
                Attempt::Settled | Attempt::Failed => {}
            }
        }
        Pass::Done
    }

    async fn attempt(&self, source: &RemoteSourceRow, entry: UploadEntry) -> Attempt {
        let (src, ent) = (source.clone(), entry.clone());
        let prepared = self
            .blocking(move |job, cache| job.prepare(cache, &src, &ent))
            .await;
        let prep = match prepared {
            Ok(Ok(prep)) => prep,
            Ok(Err(why)) | Err(why) => {
                self.fail(&entry, None, &format!("could not read the file: {why}"))
                    .await;
                return Attempt::Failed;
            }
        };
        let (file_id, path, digest) = match prep {
            Prep::Skip => return Attempt::Settled,
            Prep::Missing => {
                let (name, rel) = (self.name.clone(), entry.path.clone());
                let _ = self
                    .blocking(move |job, cache| {
                        busy(&job.gate, || cache.abandon_queued_edit(&name, &rel))
                    })
                    .await;
                tracing::info!(source = %self.name, file = %printable(&entry.path), "the file of a queued edit is gone from the cache; nothing is sent");
                self.announce();
                return Attempt::Settled;
            }
            Prep::Refuse(why) => {
                self.refuse(&entry, &why).await;
                return Attempt::Settled;
            }
            Prep::Ready {
                file_id,
                path,
                digest,
            } => (file_id, path, digest),
        };

        // The check, right before the bytes go.
        let head = match self.hub.drive.get_file(&file_id).await {
            Ok(head) => head,
            Err(e) => return self.on_error(&entry, &e).await,
        };
        if head.trashed {
            self.refuse(&entry, "the file is in the Drive trash").await;
            return Attempt::Settled;
        }
        let head_rev = head.revision().map(str::to_string);
        let head_md5 = head.md5_checksum.clone();
        let same_bytes = head_md5.as_deref() == Some(digest.md5.as_str());
        let based_on_head = entry.base_revision.is_some() && head_rev == entry.base_revision;
        let our_send_landed = entry
            .sending_md5()
            .is_some_and(|m| head_md5.as_deref() == Some(m));

        if same_bytes || (our_send_landed && head_rev.is_some()) {
            // Drive already has these bytes (or the ones an earlier try
            // sent): no second revision. A head with no revision to record
            // cannot be finished this way.
            let Some(revision) = head_rev.clone() else {
                self.conflict(&entry, None).await;
                return Attempt::Settled;
            };
            let landed = Landed {
                revision,
                drive_md5: head_md5.unwrap_or_default(),
                sent: same_bytes.then(|| digest.clone()),
            };
            self.finish(&entry, &path, &digest, landed).await;
            return Attempt::Settled;
        }
        if !based_on_head {
            self.conflict(&entry, head_rev).await;
            return Attempt::Settled;
        }

        // A removal began since the check above: nothing is sent.
        if self.removing() {
            return Attempt::Settled;
        }

        // Upload.
        let (id, seen) = (entry.id, entry.attempts);
        let md5 = digest.md5.clone();
        let marked = self
            .blocking(move |job, cache| {
                busy(&job.gate, || cache.mark_upload_sending(id, seen, &md5))
            })
            .await;
        if !matches!(marked, Ok(Ok(true))) {
            return Attempt::Settled;
        }
        let body = UploadBody {
            path: path.clone(),
            content_type: head.mime_type.clone(),
        };
        let sent = match self.hub.drive_uploader.replace_media(&file_id, body).await {
            Ok(file) => file,
            Err(e) => return self.on_error(&entry, &e).await,
        };
        let Some(revision) = sent.revision().map(str::to_string) else {
            self.fail(&entry, None, "Drive's answer named no revision")
                .await;
            return Attempt::Failed;
        };
        let landed = Landed {
            drive_md5: sent
                .md5_checksum
                .clone()
                .unwrap_or_else(|| digest.md5.clone()),
            revision,
            sent: Some(digest.clone()),
        };
        self.finish(&entry, &path, &digest, landed).await;
        Attempt::Settled
    }

    /// The bytes are on Drive: records the revision and ends the row (or
    /// rebases it when the file was saved again meanwhile).
    async fn finish(&self, entry: &UploadEntry, path: &Path, digest: &Digest, landed: Landed) {
        let (entry, path, digest) = (entry.clone(), path.to_path_buf(), digest.clone());
        let now = self.now();
        let result = self
            .blocking(move |job, cache| {
                // Is the file on disk still what Drive has?
                let current = digest_file(&path).ok();
                let unchanged = current.as_ref().is_none_or(|c| c.md5 == landed.drive_md5);
                let stamp = landed.sent.as_ref().map(|d| d.sha.as_str());
                let ok = busy(&job.gate, || {
                    cache.complete_upload(
                        &UploadLanded {
                            source: &entry.source,
                            path: &entry.path,
                            base_seen: entry.base_revision.as_deref(),
                            new_revision: &landed.revision,
                            size: i64::try_from(digest.len).unwrap_or(i64::MAX),
                            stamp,
                            file_unchanged: unchanged,
                        },
                        now,
                    )
                })?;
                if ok && unchanged {
                    // A save in the moment between the look and the commit
                    // is an edit of a `cached` file now: caught here.
                    if let Ok(after) = digest_file(&path) {
                        if after.md5 != landed.drive_md5 {
                            busy(&job.gate, || {
                                cache.queue_drive_edit(&entry.source, &entry.path, now)
                            })?;
                        }
                    }
                }
                Ok(ok)
            })
            .await;
        match result {
            Ok(Ok(true)) => {
                tracing::info!(source = %self.name, "an edit of a Drive file is on Drive");
                self.announce();
            }
            Ok(Ok(false)) => {}
            Ok(Err(why)) | Err(why) => {
                tracing::warn!(source = %self.name, why, "an upload landed but could not be recorded; the check on the next try finishes it");
            }
        }
    }

    /// Reads what the row and the file are, before anything is asked of Drive.
    fn prepare(
        &self,
        cache: &Cache,
        source: &RemoteSourceRow,
        entry: &UploadEntry,
    ) -> Result<Prep, String> {
        let Some(row) = busy(&self.gate, || {
            cache.remote_fetch_row(&entry.source, &entry.path)
        })?
        else {
            return Ok(Prep::Skip);
        };
        if row.state != ITEM_STATE_PENDING_UPLOAD {
            return Ok(Prep::Skip);
        }
        let Some(file_id) = row.file_id.clone() else {
            return Ok(Prep::Refuse("the file has no Drive id".to_string()));
        };
        if tephra_cache::is_drive_item_kind(row.kind.as_deref().unwrap_or("")) {
            return Ok(Prep::Refuse(
                "a Google document is edited in Google Docs, not uploaded".to_string(),
            ));
        }
        let Some(path) = locate(&self.hub, source, &row, cache)? else {
            return Ok(Prep::Missing);
        };
        let digest = digest_file(&path).map_err(|e| format!("{:?}", e.kind()))?;
        Ok(Prep::Ready {
            file_id,
            path,
            digest,
        })
    }

    async fn on_error(&self, entry: &UploadEntry, error: &RemoteError) -> Attempt {
        match classify(error, self.now()) {
            Failure::Transient { text, not_before } => {
                self.fail(entry, not_before, &text).await;
                Attempt::Failed
            }
            Failure::Refused(text) => {
                self.refuse(entry, &text).await;
                Attempt::Settled
            }
        }
    }

    /// A try that may pass: the wait doubles.
    async fn fail(&self, entry: &UploadEntry, not_before: Option<i64>, text: &str) {
        let (id, seen, text) = (entry.id, entry.attempts, text.to_string());
        let now = self.now();
        let _ = self
            .blocking(move |job, cache| {
                busy(&job.gate, || {
                    cache.fail_upload_attempt(id, seen, now, not_before, &text)
                })
            })
            .await;
    }

    /// Drive said no for good: nothing is retried, the bytes stay.
    async fn refuse(&self, entry: &UploadEntry, why: &str) {
        let reason = format!("{UPLOAD_REFUSED_PREFIX}{why}");
        let (e, reason) = (entry.clone(), reason);
        let held = self
            .blocking(move |job, cache| {
                busy(&job.gate, || {
                    cache.hold_upload(
                        &e.source,
                        &e.path,
                        e.base_revision.as_deref(),
                        &reason,
                        None,
                    )
                })
            })
            .await;
        if matches!(held, Ok(Ok(true))) {
            tracing::warn!(source = %self.name, file = %printable(&entry.path), why, "Drive refused an upload; the edit is kept and the item needs attention");
            self.announce();
        }
    }

    /// Drive's head is not the revision the edit was made on: no bytes go.
    async fn conflict(&self, entry: &UploadEntry, head: Option<String>) {
        let e = entry.clone();
        let held = self
            .blocking(move |job, cache| {
                busy(&job.gate, || {
                    cache.hold_upload(
                        &e.source,
                        &e.path,
                        e.base_revision.as_deref(),
                        REASON_CHANGED_ON_DRIVE,
                        head.as_deref(),
                    )
                })
            })
            .await;
        if matches!(held, Ok(Ok(true))) {
            tracing::info!(source = %self.name, file = %printable(&entry.path), "the file changed on Drive under an edit; the upload is held");
            self.announce();
        }
    }
}

enum Pass {
    Gone,
    Done,
}

enum Prep {
    /// Not pending any more (a resolve, a delete): nothing to do.
    Skip,
    /// The file of the edit is no longer in the cache.
    Missing,
    Refuse(String),
    Ready {
        file_id: String,
        path: PathBuf,
        digest: Digest,
    },
}

/// What Drive has after an upload, or already had.
struct Landed {
    revision: String,
    /// The MD5 Drive reports for those bytes.
    drive_md5: String,
    /// The local bytes, when they are the ones Drive has: their sha is the
    /// new stamp.
    sent: Option<Digest>,
}

enum Failure {
    Transient {
        text: String,
        not_before: Option<i64>,
    },
    Refused(String),
}

/// Sorts what Drive (or the road to it) answered: a failure that may pass is
/// retried with a doubling wait; the rest is held. The texts hold no token
/// (an adapter error never carries one).
fn classify(error: &RemoteError, now: i64) -> Failure {
    let transient = |not_before| Failure::Transient {
        text: error.to_string(),
        not_before,
    };
    match error {
        RemoteError::Unreachable(_)
        | RemoteError::Server { .. }
        | RemoteError::AuthRejected
        | RemoteError::Moved { .. }
        | RemoteError::BadResponse(_) => transient(None),
        RemoteError::RateLimited { retry_at } => {
            let at = retry_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(now, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            transient(Some(at))
        }
        RemoteError::NotFound => Failure::Refused("the file was not found".to_string()),
        RemoteError::PermissionDenied => {
            Failure::Refused("Tephra may not write this file".to_string())
        }
        RemoteError::Unexpected { status } => {
            Failure::Refused(format!("Google Drive answered {status}"))
        }
        RemoteError::InvalidInput(what) => Failure::Refused(format!("invalid {what}")),
    }
}

/// Stops the watcher of `source`'s cache, if it has one.
pub(crate) fn stop_watch(hub: &RemoteHub, source: &str) {
    let handle: Option<WatcherHandle> = hub
        .drive_watchers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(source);
    if let Some(handle) = handle {
        handle.stop();
    }
}

// ---------------------------------------------------------------------------
// resolve.
// ---------------------------------------------------------------------------

/// Which side of a conflict wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// Dan's edit is uploaded as a new revision.
    Mine,
    /// Dan's edit is kept as `<name>.mine`; Drive's file is fetched.
    Theirs,
}

/// What `unit.resolve` answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolveWire {
    pub unit: String,
    /// `mine` or `theirs`.
    pub keep: &'static str,
    /// The item's state after: `cached` (uploaded, or refreshed),
    /// `pending_upload` (still waiting to go), `needs_attention` (held
    /// again), `remote` (`theirs` could not fetch the file yet).
    pub state: String,
    /// Why it needs attention, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `theirs`: where Dan's edits were kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mine_path: Option<String>,
    /// `theirs`: where the refreshed file is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `theirs`: why the file could not be fetched just now (the conflict is
    /// cleared all the same; `fetch` gets it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_failed: Option<String>,
}

/// A resolve that did nothing. `reason` is the stable word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRpcError {
    pub reason: &'static str,
    pub message: String,
}

impl ResolveRpcError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        ResolveRpcError {
            reason,
            message: message.into(),
        }
    }

    pub fn data(&self) -> serde_json::Value {
        serde_json::json!({ "reason": self.reason })
    }
}

/// `<file>.mine`, or `<file>.mine.2`, `.3`, ... when that name is taken: an
/// earlier `.mine` is never overwritten.
fn free_mine_path(file: &Path) -> PathBuf {
    let base = file.as_os_str().to_os_string();
    let with = |suffix: &str| {
        let mut name = base.clone();
        name.push(suffix);
        PathBuf::from(name)
    };
    let first = with(".mine");
    if !something_at(&first) {
        return first;
    }
    (2u32..)
        .map(|n| with(&format!(".mine.{n}")))
        .find(|p| !something_at(p))
        .expect("a free name exists")
}

impl SourceRegistry {
    /// `unit.resolve`: clears the Drive conflict of `unit` (`@source:path`).
    /// Blocks (up to [`RESOLVE_WAIT`] for `mine`): call it from the blocking
    /// pool. Any unit that is not a held Drive conflict is refused and
    /// nothing changes.
    pub fn resolve_conflict(&self, unit: &str, keep: Keep) -> Result<ResolveWire, ResolveRpcError> {
        let unknown = || {
            ResolveRpcError::new(
                "unknown_unit",
                format!(
                    "no such unit: {} (a unit id is @source:path)",
                    printable(unit)
                ),
            )
        };
        let (source, rel) = unit
            .strip_prefix('@')
            .and_then(|rest| rest.split_once(':'))
            .filter(|(s, p)| !s.is_empty() && !p.is_empty())
            .ok_or_else(unknown)?;
        let Some(hub) = self.remote.get().cloned() else {
            return Err(ResolveRpcError::new(
                "resolve_unavailable",
                "this daemon has no remote support, so there is nothing to resolve",
            ));
        };
        let db =
            |e: String| ResolveRpcError::new("db", format!("the cache could not be read: {e}"));
        let nothing = || {
            ResolveRpcError::new(
                "nothing_to_resolve",
                format!(
                    "{} is not held for a change on Drive: there is nothing to resolve",
                    printable(unit)
                ),
            )
        };

        // One resolve at a time; the second of two at once finds nothing.
        let _one = hub.resolve_lock.lock().unwrap_or_else(|p| p.into_inner());
        let (source_row, item, conflict) = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            let row = busy(&self.gate, || cache.remote_source(source)).map_err(db)?;
            let item = busy(&self.gate, || cache.remote_fetch_row(source, rel)).map_err(db)?;
            let conflict = busy(&self.gate, || cache.drive_conflict(source, rel)).map_err(db)?;
            (row, item, conflict)
        };
        let Some(source_row) = source_row else {
            // A local source: its units are files on the person's own disk.
            let local = {
                let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                busy(&self.gate, || {
                    Ok(match cache.get_source(source)? {
                        Some(s) => cache.get_unit_by_rel_path(s.id, rel)?.is_some(),
                        None => false,
                    })
                })
                .map_err(db)?
            };
            return Err(if local {
                ResolveRpcError::new(
                    "not_drive",
                    format!(
                        "{} is a file of a local source: only a Drive conflict is resolved here",
                        printable(unit)
                    ),
                )
            } else {
                unknown()
            });
        };
        let Some(item) = item else {
            return Err(unknown());
        };
        if source_row.kind != DRIVE_KIND {
            return Err(ResolveRpcError::new(
                "not_drive",
                format!(
                    "{} is not a file of a Drive source: only a Drive conflict is resolved here",
                    printable(unit)
                ),
            ));
        }
        let Some(conflict) = conflict else {
            return Err(nothing());
        };
        match keep {
            Keep::Mine => {
                let won = {
                    let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                    busy(&self.gate, || cache.claim_conflict_for_mine(&conflict)).map_err(db)?
                };
                if !won {
                    return Err(nothing());
                }
                tracing::info!(source, file = %printable(rel), "resolve --mine: the edit is queued again on the head Drive had");
                self.spawn_drive_upload(source);
                if let Some(slot) = hub
                    .uploaders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(source)
                {
                    slot.poke(false);
                }
                drop(_one);
                let (state, reason) = self.wait_for_upload(source, rel);
                Ok(ResolveWire {
                    unit: unit.to_string(),
                    keep: "mine",
                    state,
                    reason,
                    mine_path: None,
                    path: None,
                    fetch_failed: None,
                })
            }
            Keep::Theirs => {
                let wire = self.resolve_theirs(&hub, unit, &source_row, &item, conflict)?;
                drop(_one);
                Ok(wire)
            }
        }
    }

    /// Waits (real time, up to [`RESOLVE_WAIT`]) for the queued edit to
    /// leave `pending_upload`, and says where the item is.
    fn wait_for_upload(&self, source: &str, rel: &str) -> (String, Option<String>) {
        let until = std::time::Instant::now() + RESOLVE_WAIT;
        loop {
            let row = {
                let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                busy(&self.gate, || cache.remote_fetch_row(source, rel))
                    .ok()
                    .flatten()
            };
            if let Some(row) = row {
                if row.state != ITEM_STATE_PENDING_UPLOAD || std::time::Instant::now() >= until {
                    return (row.state, row.reason);
                }
            } else {
                return (ITEM_STATE_REMOTE.to_string(), None);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn resolve_theirs(
        &self,
        hub: &Arc<RemoteHub>,
        unit: &str,
        source: &RemoteSourceRow,
        item: &RemoteFetchRow,
        conflict: DriveConflict,
    ) -> Result<ResolveWire, ResolveRpcError> {
        let db =
            |e: String| ResolveRpcError::new("db", format!("the cache could not be read: {e}"));
        let located = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            locate(hub, source, item, &cache)
        }
        .map_err(|e| ResolveRpcError::new("cache_file", e))?;

        // 1. Dan's bytes are copied to `<name>.mine` first: whatever happens
        //    after this, they are on disk twice.
        let mine = match &located {
            Some(file) => {
                let mine = free_mine_path(file);
                if let Err(e) = fs::copy(file, &mine) {
                    let _ = fs::remove_file(&mine);
                    return Err(ResolveRpcError::new(
                        "keep_failed",
                        format!(
                            "your edits of {} could not be kept as {}: {}; nothing was changed",
                            printable(unit),
                            printable(&mine.display().to_string()),
                            e.kind()
                        ),
                    ));
                }
                Some(mine)
            }
            None => None,
        };

        // 2. Drive's head, for the item's revision and size. A failure here
        //    is not fatal: the next poll brings both.
        let head = self
            .runtime
            .block_on(async {
                tokio::time::timeout(
                    HEAD_READ_BOUND,
                    hub.drive.get_file(item.file_id.as_deref().unwrap_or("")),
                )
                .await
            })
            .ok()
            .and_then(Result::ok);
        let revision = head.as_ref().and_then(|h| h.revision().map(str::to_string));
        let size = head
            .as_ref()
            .and_then(|h| h.size)
            .and_then(|n| i64::try_from(n).ok());

        // 3. The conflict is cleared, conditional on it being the one read.
        let won = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            busy(&self.gate, || {
                cache.claim_conflict_for_theirs(&conflict, revision.as_deref(), size)
            })
        };
        match won {
            Ok(true) => {}
            Ok(false) => {
                if let Some(mine) = &mine {
                    let _ = fs::remove_file(mine);
                }
                return Err(ResolveRpcError::new(
                    "nothing_to_resolve",
                    format!(
                        "{} is not held for a change on Drive: there is nothing to resolve",
                        printable(unit)
                    ),
                ));
            }
            Err(e) => {
                if let Some(mine) = &mine {
                    let _ = fs::remove_file(mine);
                }
                return Err(db(e));
            }
        }
        tracing::info!(source = %source.name, file = %printable(&item.path), "resolve --theirs: the edit is kept beside the file and the file is fetched from Drive");

        // 4. The edited file goes (its bytes are in `.mine`), and Drive's
        //    are fetched.
        if let (Some(file), Some(_)) = (&located, &mine) {
            if let Err(e) = fs::remove_file(file) {
                tracing::warn!(error = %e.kind(), "could not remove the edited copy after keeping it");
            }
        }
        if let Some(slot) = hub
            .uploaders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&source.name)
        {
            slot.poke(true);
        }
        let mine_path = mine.map(|p| p.display().to_string());
        match self.fetch_unit(unit) {
            Ok(done) => Ok(ResolveWire {
                unit: unit.to_string(),
                keep: "theirs",
                state: done.state,
                reason: None,
                mine_path,
                path: Some(done.path),
                fetch_failed: None,
            }),
            Err(e) => Ok(ResolveWire {
                unit: unit.to_string(),
                keep: "theirs",
                state: ITEM_STATE_REMOTE.to_string(),
                reason: None,
                mine_path,
                path: None,
                fetch_failed: Some(e.message),
            }),
        }
    }
}
