//! The Drive half of the poller (PR-10): a Drive source is kept current from
//! the changes feed instead of a repository's ref.
//!
//! Every poll asks for the feed from the saved page token and drains EVERY
//! page (`nextPageToken`) before it keeps the answer's `newStartPageToken`.
//! Nothing is written until the whole feed is in hand and the text of every
//! added or edited file is read, and then it is written in ONE transaction
//! together with the new token. A failure at any point before that leaves
//! the old token and the index as they were, so the same pages come again
//! next poll. A page that comes twice changes nothing the second time: a
//! change is a file's state "now", and a file already in that state has no
//! difference to apply.
//!
//! A change says where a file is (its parents) and what it is called, never
//! its path. The path is made here, from the folders above it up to the
//! source's root folder ([`remote_drive::assign_paths`] gives the `name (2)`
//! of a duplicate and keeps one it already had). A file that is not below
//! the root is not the source's: it is ignored, and one that was and moved
//! out is removed. A FOLDER that changed (renamed, moved, trashed, new) can
//! change the path of everything under it, so the tree is listed again and
//! compared: metadata only, and only when the folder is not the one the last
//! listing saw.
//!
//! A token Drive no longer accepts (400 or 404, after a long time away) is a
//! lost cursor: a fresh start token is taken FIRST, the tree is listed again,
//! and every text file is read again from scratch.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tephra_cache::{
    is_kept_deleted, is_text_item_kind, pin_covers, Cache, CacheError, NewFixLog, PollState,
    RemoteItemRow, RemotePin, RemoteSourceRow, ITEM_STATE_NEEDS_ATTENTION, ITEM_STATE_NOT_PUSHED,
    ITEM_STATE_PENDING_UPLOAD, ITEM_STATE_REMOTE, STATUS_ACTIVE, STATUS_AUTH_REQUIRED,
    STATUS_OFFLINE, STATUS_SYNCING,
};
use tephra_remote::drive::{Change, DriveFile};
use tephra_remote::RemoteError;

use super::{clone_copy, CopyState, PollJob, Step, STAGE_CAP_BYTES};
use crate::file_kind::MAX_INDEXED_BYTES;
use crate::fixes::log::{FixDetail, KIND_RENAME, STATUS_APPLIED};
use crate::registry::DaemonEvent;
use crate::remote::safe_path::safe_segment;
use crate::remote_drive::{
    self, assign_paths, build_items, download_capped, list_tree, offline_reason, paths_by_file_id,
    rate_limit_reason, DriveItem, Listed, Tree, REASON_NOT_FOUND, REASON_PERMISSION_DENIED,
    REASON_TOKEN, REASON_TOO_LARGE,
};
use crate::remote_github::{now_secs, IndexJob};

/// Folders of the tree by id (the path of each below the root), as the last
/// listing saw them. `None` until a listing has been made since the daemon
/// started: every folder change is then looked at.
pub(super) type FolderMap = std::sync::Mutex<Option<HashMap<String, String>>>;

/// What one added, edited, renamed or moved file needs.
struct PlanItem {
    item: DriveItem,
    /// Its path before, when it moved or was renamed.
    old_path: Option<String>,
    /// Its text is read (again).
    read: bool,
    /// It is the same file under another path, with nothing read for it:
    /// its row, and what the index parsed out of it, move with it.
    in_place: bool,
}

#[derive(Default)]
struct DrivePlan {
    upserts: Vec<PlanItem>,
    /// The paths of files that are gone.
    removed: Vec<String>,
}

impl DrivePlan {
    fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removed.is_empty()
    }
}

/// What reading one file gave.
enum Read {
    Bytes(Vec<u8>),
    /// Not read, for good: the reason is shown on the item.
    Reason(&'static str),
    /// Not read now; the file stays listed and is read on a later poll.
    Unread,
}

/// Whether `e` from the feed says the page token is no longer good.
fn is_stale_token(e: &RemoteError) -> bool {
    matches!(
        e,
        RemoteError::Unexpected { status: 400 } | RemoteError::NotFound
    )
}

/// The last change of each file, in the order the files first appear.
fn latest_per_file(changes: Vec<Change>) -> Vec<Change> {
    let mut order: Vec<String> = Vec::new();
    let mut last: HashMap<String, Change> = HashMap::new();
    for c in changes {
        if !last.contains_key(&c.file_id) {
            order.push(c.file_id.clone());
        }
        last.insert(c.file_id.clone(), c);
    }
    order
        .into_iter()
        .filter_map(|id| last.remove(&id))
        .collect()
}

/// The path of a folder below the root, for a change that names where a
/// file is. Memoised for one poll; asks Drive about a folder only when the
/// last listing did not know it.
struct Resolver<'a> {
    adapter: &'a dyn tephra_remote::drive::DriveAdapter,
    root: &'a str,
    known: Option<&'a HashMap<String, String>>,
    memo: HashMap<String, Option<String>>,
    meta: HashMap<String, Option<DriveFile>>,
}

impl<'a> Resolver<'a> {
    /// The deepest chain of folders walked, so a cycle of parents ends.
    const MAX_DEPTH: usize = 64;

    async fn meta(&mut self, id: &str) -> Result<Option<DriveFile>, RemoteError> {
        if let Some(m) = self.meta.get(id) {
            return Ok(m.clone());
        }
        let got = match self.adapter.get_file(id).await {
            Ok(file) => Some(file),
            Err(RemoteError::NotFound | RemoteError::PermissionDenied) => None,
            Err(e) => return Err(e),
        };
        self.meta.insert(id.to_string(), got.clone());
        Ok(got)
    }

    /// The path of folder `id` below the root; `None` when it is not below
    /// it (or is trashed, or Drive will not say).
    async fn dir_of(&mut self, id: &str) -> Result<Option<String>, RemoteError> {
        if id == self.root {
            return Ok(Some(String::new()));
        }
        if let Some(dir) = self.known.and_then(|k| k.get(id)) {
            return Ok(Some(dir.clone()));
        }
        if let Some(done) = self.memo.get(id) {
            return Ok(done.clone());
        }
        // Walk up through each folder's first parent that leads to the root.
        let mut names: Vec<String> = Vec::new();
        let mut cur = id.to_string();
        let mut result = None;
        for _ in 0..Self::MAX_DEPTH {
            if cur == self.root {
                let mut dir = String::new();
                for n in names.iter().rev() {
                    dir.push_str(&safe_segment(n));
                    dir.push('/');
                }
                result = Some(dir);
                break;
            }
            if let Some(known) = self.known.and_then(|k| k.get(&cur)) {
                let mut dir = known.clone();
                for n in names.iter().rev() {
                    dir.push_str(&safe_segment(n));
                    dir.push('/');
                }
                result = Some(dir);
                break;
            }
            let Some(folder) = self.meta(&cur).await? else {
                break;
            };
            if !folder.is_folder() || folder.trashed {
                break;
            }
            let Some(parent) = folder.parents.first().cloned() else {
                break;
            };
            names.push(folder.name.clone());
            cur = parent;
        }
        self.memo.insert(id.to_string(), result.clone());
        Ok(result)
    }

    /// The directory of a file with these parents: the first parent that is
    /// below the root.
    async fn dir_for(&mut self, parents: &[String]) -> Result<Option<String>, RemoteError> {
        for parent in parents {
            if let Some(dir) = self.dir_of(parent).await? {
                return Ok(Some(dir));
            }
        }
        Ok(None)
    }
}

/// What the feed says, as files to bring in line and files that are gone.
struct Wanted {
    listed: Vec<Listed>,
    removed_ids: HashSet<String>,
    /// `listed` is the whole tree: whatever is not in it is gone.
    full: bool,
    /// The folders of the tree, when it was listed now.
    folders: Option<HashMap<String, String>>,
}

/// The difference between the rows and what is wanted. Pure.
fn diff(rows: &[RemoteItemRow], wanted: &Wanted) -> DrivePlan {
    let by_id: HashMap<&str, &RemoteItemRow> = rows
        .iter()
        .filter_map(|r| r.file_id.as_deref().map(|id| (id, r)))
        .collect();
    let in_listing: HashSet<&str> = wanted.listed.iter().map(|l| l.file.id.as_str()).collect();
    let mut plan = DrivePlan::default();
    let mut leaving: HashSet<&str> = HashSet::new();
    for r in rows {
        let Some(id) = r.file_id.as_deref() else {
            continue;
        };
        let gone = wanted.removed_ids.contains(id) || (wanted.full && !in_listing.contains(id));
        if gone {
            plan.removed.push(r.path.clone());
            leaving.insert(id);
        }
    }
    let except: HashSet<&str> = in_listing.iter().copied().chain(leaving).collect();
    let occupied = remote_drive::occupied_paths(rows, &except);
    let existing = paths_by_file_id(rows);
    let paths = assign_paths(&wanted.listed, &existing, &occupied);
    for item in build_items(&wanted.listed, paths) {
        let read_it = is_text_item_kind(item.kind)
            && !item.size.is_some_and(|s| s as u64 > MAX_INDEXED_BYTES);
        match by_id.get(item.file_id.as_str()) {
            None => plan.upserts.push(PlanItem {
                item,
                old_path: None,
                read: read_it,
                in_place: false,
            }),
            Some(row) => {
                let same_file = row.revision == item.revision
                    && row.kind.as_deref() == Some(item.kind)
                    && row.size == item.size;
                if same_file && row.path == item.path {
                    continue;
                }
                let old_path = (row.path != item.path).then(|| row.path.clone());
                // A rename or a move changes no byte: nothing is read, the
                // row just gets its new path. (A copy that is cached, or has
                // edits, is looked at by name and is not carried along.)
                let in_place = same_file && row.state == ITEM_STATE_REMOTE;
                plan.upserts.push(PlanItem {
                    item,
                    old_path,
                    read: read_it && !in_place,
                    in_place,
                });
            }
        }
    }
    plan
}

/// The pins that follow what they name when Drive renamed or moved it, as
/// `(pin, new path)`. A file pin follows its file. A folder pin follows its
/// folder: Drive says nothing of a folder but the move of each file in it,
/// so the folder counts as renamed when every file that was under it moved
/// to one other folder, keeping its place below it, and that folder held
/// nothing else before. A folder with a file that stayed, left alone or
/// was removed is not renamed (a file moved out of it is just moved).
fn moved_pins(
    pins: &[RemotePin],
    rows: &HashMap<String, RemoteItemRow>,
    plan: &DrivePlan,
) -> Vec<(RemotePin, String)> {
    let moves: HashMap<&str, &str> = plan
        .upserts
        .iter()
        .filter_map(|u| Some((u.old_path.as_deref()?, u.item.path.as_str())))
        .collect();
    let removed: HashSet<&str> = plan.removed.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for pin in pins {
        if !pin.is_folder {
            if let Some(new) = moves.get(pin.path.as_str()) {
                out.push((pin.clone(), (*new).to_string()));
            }
            continue;
        }
        if pin.path.is_empty() {
            continue;
        }
        let members: Vec<&RemoteItemRow> = rows
            .values()
            .filter(|r| {
                !is_kept_deleted(r) && r.path != pin.path && pin_covers(&pin.path, true, &r.path)
            })
            .collect();
        let mut target: Option<String> = None;
        let mut whole = !members.is_empty();
        for r in &members {
            let renamed = moves.get(r.path.as_str()).and_then(|new| {
                let suffix = r.path.strip_prefix(&pin.path)?;
                new.strip_suffix(suffix).filter(|q| !q.is_empty())
            });
            match renamed {
                Some(q) if target.as_deref().is_none_or(|t| t == q) => {
                    target = Some(q.to_string());
                }
                _ => {
                    whole = false;
                    break;
                }
            }
        }
        let Some(target) = target.filter(|t| whole && *t != pin.path) else {
            continue;
        };
        let was_there = |path: &str| {
            path == target
                || path
                    .strip_prefix(target.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let taken = rows.values().any(|r| {
            was_there(&r.path)
                && !is_kept_deleted(r)
                && !moves.contains_key(r.path.as_str())
                && !removed.contains(r.path.as_str())
        });
        if !taken {
            out.push((pin.clone(), target));
        }
    }
    out
}

/// Applies `plan` in the caller's transaction: files gone, files moved,
/// files added or edited (with the text read for them), the rename record,
/// and the pins that follow a renamed or moved file or folder.
/// `None` when the source is gone; otherwise the cached copies (below
/// `cache_dir`) of files that left, for the caller to delete once the
/// transaction has committed.
///
/// A file that left keeps its row, as a kept-deleted item, when its copy has
/// edits to send (an upload is queued, or the item says so): nothing a
/// person wrote is dropped because Drive moved on.
fn apply_plan(
    cache: &Cache,
    name: &str,
    plan: &DrivePlan,
    reads: &HashMap<String, Read>,
    now: i64,
    cache_dir: &Path,
) -> Result<Option<Vec<PathBuf>>, CacheError> {
    let Some(source) = cache.get_source(name)? else {
        return Ok(None);
    };
    let rows: HashMap<String, RemoteItemRow> = cache
        .remote_items(name)?
        .into_iter()
        .map(|r| (r.path.clone(), r))
        .collect();
    let mut stale_copies: Vec<PathBuf> = Vec::new();
    // Pins that follow a rename or a move (see `moved_pins`), found before
    // any row changes.
    let pins_moved = moved_pins(&cache.remote_pins(name)?, &rows, plan);

    // Renames and moves that carry their row along go first, by a name
    // nobody has, so two files that swap names do not collide.
    let carried: Vec<(String, String)> = plan
        .upserts
        .iter()
        .filter(|u| u.in_place)
        .filter_map(|u| Some((u.old_path.clone()?, u.item.path.clone())))
        .collect();

    let vacated = plan.removed.iter().map(String::as_str).chain(
        plan.upserts
            .iter()
            .filter(|u| !u.in_place)
            .filter_map(|u| u.old_path.as_deref()),
    );
    for path in vacated {
        if let Some(unit) = cache.get_unit_by_rel_path(source.id, path)? {
            cache.delete_unit(unit.id)?;
        }
        let state = rows.get(path).map(|r| r.state.as_str());
        let unsent = cache.has_queued_upload(name, path)?
            || matches!(
                state,
                Some(
                    ITEM_STATE_PENDING_UPLOAD | ITEM_STATE_NOT_PUSHED | ITEM_STATE_NEEDS_ATTENTION
                )
            );
        if unsent {
            cache.keep_deleted_remote_item(name, path)?;
        } else {
            cache.delete_remote_item(name, path)?;
            if !matches!(state, None | Some(ITEM_STATE_REMOTE)) {
                if let CopyState::File(copy) = clone_copy(cache_dir, path) {
                    stale_copies.push(copy);
                }
            }
        }
    }
    cache.rename_remote_items(name, source.id, &carried)?;
    for u in &plan.upserts {
        let new = u.item.as_new();
        if u.in_place {
            // The row moved above; it only learns what else about it changed.
            cache.upsert_remote_item(name, &new)?;
        } else {
            cache.upsert_remote_item(name, &new)?;
            match reads.get(&u.item.file_id) {
                Some(Read::Bytes(bytes)) => {
                    crate::scan::write_remote_unit(cache, source.id, &u.item.path, bytes)
                        .map(|_| ())
                        .map_err(crate::scan::cache_error_of)?;
                }
                Some(Read::Reason(reason)) => {
                    cache.replace_remote_unit_listing(source.id, &new, MAX_INDEXED_BYTES)?;
                    cache.set_remote_item_reason(name, &u.item.path, Some(reason))?;
                }
                _ => cache.replace_remote_unit_listing(source.id, &new, MAX_INDEXED_BYTES)?,
            }
        }
        if let Some(old) = &u.old_path {
            let detail = FixDetail {
                message: Some(
                    "renamed or moved in Google Drive; links to it are not rewritten".to_string(),
                ),
                ..FixDetail::default()
            };
            cache.insert_fix_log(&NewFixLog {
                created_at: now,
                kind: KIND_RENAME.to_string(),
                status: STATUS_APPLIED.to_string(),
                old_source: Some(name.to_string()),
                old_path: Some(old.clone()),
                new_source: Some(name.to_string()),
                new_path: Some(u.item.path.clone()),
                files_changed: 0,
                detail_json: Some(detail.to_json()),
            })?;
        }
    }
    if !pins_moved.is_empty() {
        // Last, with every row in place: a file pin on a moved file went
        // with its old row above and is written back at the new path. Then
        // the states follow the pins: a moved file whose copy was dropped is
        // `remote` at its new path and the pin job fetches it after this
        // sync; a pin that covers nothing any more frees what it held.
        cache.move_remote_pins(name, &pins_moved)?;
        cache.reconcile_remote_pins(name)?;
    }
    Ok(Some(stale_copies))
}

impl PollJob {
    /// One poll of a Drive source whose first index is finished.
    pub(super) async fn poll_drive(&self, source: &RemoteSourceRow, state: &PollState) -> Step {
        let Some(token) = state.commit.clone() else {
            // A lost cursor: nothing is trusted, read it all again.
            return self.recover_drive(source).await;
        };

        // 1. Every page of the feed, before anything is kept.
        let mut changes: Vec<Change> = Vec::new();
        let mut page_token = token.clone();
        let new_start = loop {
            match self.hub.drive.list_changes(&page_token).await {
                Ok(page) => {
                    changes.extend(page.changes);
                    match (page.next_page_token, page.new_start_page_token) {
                        (Some(next), _) => page_token = next,
                        (None, Some(start)) => break start,
                        (None, None) => {
                            let e = RemoteError::BadResponse("the changes feed has no next token");
                            return self.fail_drive(source, e).await;
                        }
                    }
                }
                Err(e) if is_stale_token(&e) => return self.recover_drive(source).await,
                Err(e) => return self.fail_drive(source, e).await,
            }
        };

        // 2. What the changes mean for this source's folder.
        let wanted = match self.wanted_from(source, changes).await {
            Ok(wanted) => wanted,
            Err(e) => return self.fail_drive(source, e).await,
        };
        let loaded = self.db(|c, n| c.remote_items(n)).await;
        let rows = match loaded {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "could not read the index before a poll");
                return Step::Next(None);
            }
        };
        let plan = diff(&rows, &wanted);

        // 3. The text of every added or edited file. Nothing is written
        // yet: a failure here leaves the token and the index as they were.
        let mut reads: HashMap<String, Read> = HashMap::new();
        let mut staged_bytes = 0u64;
        for u in plan.upserts.iter().filter(|u| u.read) {
            let native = u.item.kind == tephra_cache::ITEM_KIND_GDOC;
            if staged_bytes >= STAGE_CAP_BYTES {
                reads.insert(u.item.file_id.clone(), Read::Unread);
                continue;
            }
            match self.read_one(&u.item.file_id, native).await {
                Ok(Read::Bytes(bytes)) => {
                    staged_bytes += bytes.len() as u64;
                    reads.insert(u.item.file_id.clone(), Read::Bytes(bytes));
                }
                Ok(other) => {
                    reads.insert(u.item.file_id.clone(), other);
                }
                Err(e) => return self.fail_drive(source, e).await,
            }
        }

        // 4. The one transaction.
        let now = now_secs(&*self.hub.clock);
        let status_changed = source.status != STATUS_ACTIVE || source.status_reason.is_some();
        let plan = Arc::new(plan);
        let reads = Arc::new(reads);
        let cache_dir = self.hub.cache_dir(&self.name);
        let committed = {
            let (plan, reads, new_start) = (plan.clone(), reads.clone(), new_start.clone());
            self.db(move |c, n| {
                let tx = c.connection().unchecked_transaction()?;
                let Some(copies) = apply_plan(c, n, &plan, &reads, now, &cache_dir)? else {
                    return Ok(None);
                };
                c.set_remote_poll_state(n, &new_start, None, now)?;
                c.set_remote_status(n, STATUS_ACTIVE, None, Some(now))?;
                tx.commit()?;
                Ok(Some(copies))
            })
            .await
        };
        drop(reads);
        match committed {
            Ok(Some(copies)) => {
                for copy in copies {
                    if let Err(e) = std::fs::remove_file(&copy) {
                        tracing::warn!(source = %self.name, error = %e, "could not remove the cached copy of a file Drive no longer has");
                    }
                }
            }
            Ok(None) => return Step::Gone,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "a poll could not be applied; nothing was changed");
                return Step::Next(None);
            }
        }
        if let Some(folders) = wanted.folders {
            *self.folders.lock().unwrap_or_else(|p| p.into_inner()) = Some(folders);
        }
        tracing::info!(
            source = %self.name,
            changed = plan.upserts.len(),
            removed = plan.removed.len(),
            "drive source updated"
        );
        if !plan.is_empty() || status_changed {
            self.announce();
        }

        // 5. A file that could not be read before (a server error) is read
        // again now.
        let pending = self
            .db(|c, n| c.remote_pending_items(n, MAX_INDEXED_BYTES))
            .await
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if pending {
            self.new_index_job().run().await;
        }
        Step::Next(None)
    }

    fn new_index_job(&self) -> IndexJob {
        IndexJob::new(
            &self.name,
            self.cache_path.clone(),
            self.gate.clone(),
            self.events_tx.clone(),
            self.hub.clone(),
        )
    }

    /// Reads one file for the index. A rate limit, a rejected token and a
    /// connection that is away are errors (the poll stops and the old token
    /// stays); a server error on this file, a file that is gone or one the
    /// token may not read are not.
    async fn read_one(&self, file_id: &str, native: bool) -> Result<Read, RemoteError> {
        let answer = if native {
            self.hub.drive.export_markdown(file_id).await
        } else {
            download_capped(&*self.hub.drive, file_id).await
        };
        match answer {
            Ok(bytes) if bytes.len() as u64 > MAX_INDEXED_BYTES => {
                Ok(Read::Reason(REASON_TOO_LARGE))
            }
            Ok(bytes) => Ok(Read::Bytes(bytes)),
            Err(RemoteError::NotFound) => Ok(Read::Reason(REASON_NOT_FOUND)),
            Err(RemoteError::PermissionDenied) => Ok(Read::Reason(REASON_PERMISSION_DENIED)),
            Err(RemoteError::Server { status }) => {
                tracing::warn!(source = %self.name, status, "a Drive read failed; it is tried again on a later poll");
                Ok(Read::Unread)
            }
            Err(e) => Err(e),
        }
    }

    /// The files to bring in line, and the files that are gone, from a feed.
    async fn wanted_from(
        &self,
        source: &RemoteSourceRow,
        changes: Vec<Change>,
    ) -> Result<Wanted, RemoteError> {
        let root = source.target.clone();
        let known_folders = self
            .folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let rows = self
            .db(|c, n| c.remote_items(n))
            .await
            .map_err(|_| RemoteError::BadResponse("the index could not be read"))?;
        let row_ids: HashSet<&str> = rows.iter().filter_map(|r| r.file_id.as_deref()).collect();

        let mut resolver = Resolver {
            adapter: &*self.hub.drive,
            root: &root,
            known: known_folders.as_ref(),
            memo: HashMap::new(),
            meta: HashMap::new(),
        };
        let mut listed: Vec<Listed> = Vec::new();
        let mut removed_ids: HashSet<String> = HashSet::new();
        let mut relist = false;

        for change in latest_per_file(changes) {
            if change.file_id == root {
                continue;
            }
            let file = match change.file {
                Some(file) if !change.removed => file,
                _ => {
                    // Gone for good. A file the index has is removed. A
                    // folder is not in the index: it matters only when the
                    // last listing had it (or none was made yet).
                    if row_ids.contains(change.file_id.as_str()) {
                        removed_ids.insert(change.file_id.clone());
                    } else if known_folders
                        .as_ref()
                        .is_none_or(|k| k.contains_key(&change.file_id))
                    {
                        relist = true;
                    }
                    continue;
                }
            };
            if file.is_folder() {
                let now_dir = if file.trashed {
                    None
                } else {
                    resolver
                        .dir_for(&file.parents)
                        .await?
                        .map(|d| format!("{d}{}/", safe_segment(&file.name)))
                };
                let before = known_folders.as_ref().and_then(|k| k.get(&file.id));
                if known_folders.is_none() && now_dir.is_none() && !row_ids.is_empty() {
                    // No listing since start: it might have been ours.
                    relist = true;
                } else if before != now_dir.as_ref() {
                    relist = true;
                }
                continue;
            }
            let dir = if file.trashed {
                None
            } else {
                resolver.dir_for(&file.parents).await?
            };
            match dir {
                Some(dir) => listed.push(Listed { file, dir }),
                // Trashed, or not (or no longer) below the root.
                None => {
                    if row_ids.contains(file.id.as_str()) {
                        removed_ids.insert(file.id.clone());
                    }
                }
            }
        }
        if relist {
            let Tree { files, folders } = list_tree(&*self.hub.drive, &root).await?;
            return Ok(Wanted {
                listed: files,
                removed_ids: HashSet::new(),
                full: true,
                folders: Some(folders),
            });
        }
        Ok(Wanted {
            listed,
            removed_ids,
            full: false,
            folders: None,
        })
    }

    /// The saved token is no longer good, or there is none, or the source
    /// was cut off while its folder was being listed. A fresh start
    /// token is taken first, the tree is listed again, the rows are brought
    /// in line with it and every text file is read again from scratch.
    pub(super) async fn recover_drive(&self, source: &RemoteSourceRow) -> Step {
        let token = match self.hub.drive.start_page_token().await {
            Ok(token) => token,
            Err(e) => return self.fail_drive(source, e).await,
        };
        let Tree { files, folders } = match list_tree(&*self.hub.drive, &source.target).await {
            Ok(tree) => tree,
            Err(e) => return self.fail_drive(source, e).await,
        };
        let wanted = Arc::new(Wanted {
            listed: files,
            removed_ids: HashSet::new(),
            full: true,
            folders: None,
        });
        let now = now_secs(&*self.hub.clock);
        let cache_dir = self.hub.cache_dir(&self.name);
        let done = {
            let wanted = wanted.clone();
            let token = token.clone();
            self.db(move |c, n| {
                let rows = c.remote_items(n)?;
                let plan = diff(&rows, &wanted);
                let tx = c.connection().unchecked_transaction()?;
                let Some(copies) = apply_plan(c, n, &plan, &HashMap::new(), now, &cache_dir)?
                else {
                    return Ok(false);
                };
                tx.commit()?;
                for copy in copies {
                    let _ = std::fs::remove_file(copy);
                }
                // Every unit is made again with no content and the index
                // starts over; then the fresh token is kept.
                if !c.reset_remote_index(n, now, MAX_INDEXED_BYTES)? {
                    return Ok(false);
                }
                c.set_remote_poll_state(n, &token, None, now)?;
                Ok(true)
            })
            .await
        };
        match done {
            Ok(true) => {}
            Ok(false) => return Step::Gone,
            Err(e) => {
                tracing::warn!(source = %self.name, error = %e, "could not start the index over");
                return Step::Next(None);
            }
        }
        *self.folders.lock().unwrap_or_else(|p| p.into_inner()) = Some(folders);
        tracing::warn!(source = %self.name, "the Drive feed position was lost; indexing from scratch");
        self.announce();
        self.new_index_job().run().await;
        Step::Next(None)
    }

    /// Records what a failed request means for the source. Never removes an
    /// item, and never writes anything an unchanged status would not.
    async fn fail_drive(&self, source: &RemoteSourceRow, error: RemoteError) -> Step {
        let mut retry_at = None;
        let (status, reason) = match error {
            e @ (RemoteError::Unreachable(_) | RemoteError::Server { .. }) => {
                (STATUS_OFFLINE, offline_reason(&e))
            }
            RemoteError::RateLimited { retry_at: at } => {
                retry_at = Some(at);
                (STATUS_SYNCING, rate_limit_reason("polling", at))
            }
            RemoteError::AuthRejected => (STATUS_AUTH_REQUIRED, REASON_TOKEN.to_string()),
            other => (
                STATUS_SYNCING,
                format!("a poll did not finish ({other}); trying again"),
            ),
        };
        let unchanged =
            source.status == status && source.status_reason.as_deref() == Some(reason.as_str());
        if !unchanged {
            let why = reason.clone();
            let wrote = self
                .db(move |c, n| c.set_remote_status(n, status, Some(&why), None))
                .await;
            match wrote {
                Ok(()) => {
                    tracing::warn!(source = %self.name, status, "drive source is not being kept current");
                    let _ = self.events_tx.send(DaemonEvent::SourceChanged {
                        source: self.name.clone(),
                        paths: Vec::new(),
                        fixes: Vec::new(),
                    });
                }
                Err(e) => {
                    tracing::warn!(source = %self.name, error = %e, "could not record a failed poll")
                }
            }
        }
        Step::Next(retry_at)
    }
}
