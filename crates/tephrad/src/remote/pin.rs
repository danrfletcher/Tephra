//! Pins (atlas-27 PR-12): `unit.pin` and `unit.unpin`, the planner that says
//! what a pin covers, and the background download that fills a pin.
//!
//! A pin marks a remote file or folder "keep downloaded". Pinning stores one
//! `remote_pins` row and returns; the files come down in the background and
//! are then never evicted. A folder pin is a path prefix, so a file that
//! appears under the folder later is covered with no new row and is fetched
//! after the next sync.
//!
//! The planner works from the index alone (no request to the remote): it
//! counts the files a pin covers and their listed bytes, and counts as
//! "skipped, not fetchable" what a fetch refuses (native Google Docs, Sheets,
//! Slides, Forms and Drawings, shortcuts, symlinks, submodules and Git LFS
//! pointers): those are excluded from the total and never requested. A pin
//! whose total is over a fifth of the cache limit is not stored until the
//! caller says `confirm: true`. A pin never fails because of the limit: pins
//! that together exceed it only warn (the files are never evicted, and
//! `cache status` says so).
//!
//! The download is the `unit.fetch` path ([`SourceRegistry::fetch_unit`]),
//! file by file, so a pin and a fetch of one file are one download and a
//! failed file goes back to `remote` with its reason like any failed fetch.
//! One job per source runs at a time. It stops when the remote is away
//! (offline, a rejected token, a rate limit) and is started again after the
//! source's next sync, after a pin, and at start-up; so the files of a pin
//! that could not download, the files that failed, and the files added to a
//! pinned folder all arrive without another `unit.pin`.
//!
//! Pin and unpin write nothing on the remote and run no git command.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tephra_cache::{is_kept_deleted, pin_covers, PinWrite, RemoteItemRow, REASON_LFS_POINTER};

use crate::registry::SourceRegistry;
use crate::remote::fetch::{not_fetchable_item, FetchRpcError};
use crate::remote_github::{printable, RemoteHub};

/// A pin whose total is more than this fraction of the cache limit (one
/// part in `CONFIRM_DIVISOR`) asks for a confirmation first.
pub const CONFIRM_DIVISOR: u64 = 5;

/// Whether a pin of `total_bytes` needs a confirmation under a cache limit
/// of `cap_bytes`: strictly more than a fifth of the cap. Exactly a fifth
/// does not; one byte over does. (Worked in 128 bits: no total overflows.)
pub fn needs_confirm(total_bytes: u64, cap_bytes: u64) -> bool {
    u128::from(total_bytes) * u128::from(CONFIRM_DIVISOR) > u128::from(cap_bytes)
}

/// What a pin names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    File,
    Folder,
}

impl PinKind {
    pub fn is_folder(self) -> bool {
        self == PinKind::Folder
    }
}

/// What a pin covers, counted from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinPlan {
    pub kind: PinKind,
    /// The path as stored: no trailing slash.
    pub path: String,
    /// Files a pin would download or keep: the count of fetchable files
    /// under it, whether or not they are on disk already.
    pub files: usize,
    /// Their listed bytes (a file with no listed size counts 0).
    pub total_bytes: u64,
    /// What is listed under the pin and never fetched, by what it is
    /// (`gdoc`, `gsheet`, `submodule`, `lfs_pointer`, ...).
    pub skipped: BTreeMap<String, usize>,
}

impl PinPlan {
    pub fn skipped_count(&self) -> usize {
        self.skipped.values().sum()
    }
}

/// Why nothing can be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No file or folder of that name is in the index.
    NotFound,
    /// A single file that is never fetched (a native Doc, ...).
    NotFetchable(FetchRpcError),
}

/// Reads the path of a pin request: no leading slash, no empty, `.` or `..`
/// part, no control character. A trailing slash says "a folder" and is
/// returned as the second value (the path itself is returned without it).
pub fn normalize_pin_path(raw: &str) -> Result<(String, bool), String> {
    let (body, folder) = match raw.strip_suffix('/') {
        Some(body) => (body, true),
        None => (raw, false),
    };
    if body.is_empty() {
        return Err("name a file or a folder, like @source:docs/setup.md".to_string());
    }
    if raw.chars().any(char::is_control) {
        return Err("a path cannot hold a control character".to_string());
    }
    for part in body.split('/') {
        match part {
            "" => return Err("a path cannot have an empty part (a leading or doubled /)".into()),
            "." | ".." => return Err("a path cannot contain . or .. parts".into()),
            _ => {}
        }
    }
    Ok((body.to_string(), folder))
}

/// Plans a pin on `path` (already normalized) over the source's index rows.
/// `folder_only` is a request that ended in a slash: only a folder will do.
/// A file of that exact name is taken first otherwise.
pub fn plan_pin(
    items: &[RemoteItemRow],
    path: &str,
    folder_only: bool,
) -> Result<PinPlan, PlanError> {
    let live = |r: &&RemoteItemRow| !is_kept_deleted(r);
    if !folder_only {
        if let Some(row) = items.iter().filter(live).find(|r| r.path == path) {
            if let Some(refusal) = not_fetchable_item(
                row.kind.as_deref(),
                row.file_id.as_deref(),
                row.reason.as_deref(),
            ) {
                return Err(PlanError::NotFetchable(refusal));
            }
            return Ok(PinPlan {
                kind: PinKind::File,
                path: path.to_string(),
                files: 1,
                total_bytes: row.size.unwrap_or(0).max(0) as u64,
                skipped: BTreeMap::new(),
            });
        }
    }
    let mut plan = PinPlan {
        kind: PinKind::Folder,
        path: path.to_string(),
        files: 0,
        total_bytes: 0,
        skipped: BTreeMap::new(),
    };
    let mut any = false;
    for row in items.iter().filter(live) {
        if row.path == path || !pin_covers(path, true, &row.path) {
            continue;
        }
        any = true;
        match not_fetchable_item(
            row.kind.as_deref(),
            row.file_id.as_deref(),
            row.reason.as_deref(),
        ) {
            Some(refusal) => {
                // A Git LFS pointer is stored as a `binary` file with a
                // reason; it is named by the reason.
                let what = if row.reason.as_deref() == Some(REASON_LFS_POINTER) {
                    "lfs_pointer".to_string()
                } else {
                    refusal.kind.unwrap_or_else(|| "other".to_string())
                };
                *plan.skipped.entry(what).or_default() += 1;
            }
            None => {
                plan.files += 1;
                plan.total_bytes += row.size.unwrap_or(0).max(0) as u64;
            }
        }
    }
    if any {
        Ok(plan)
    } else {
        Err(PlanError::NotFound)
    }
}

// ---- what a call answers -----------------------------------------------------------

/// What `unit.pin` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinWire {
    pub unit: String,
    /// `pinned` (stored now), `already_pinned` (nothing changed) or
    /// `needs_confirm` (nothing was written: ask again with `confirm`).
    pub status: &'static str,
    pub kind: PinKind,
    /// Fetchable files the pin covers.
    pub files: usize,
    /// Their listed bytes.
    pub total_bytes: u64,
    /// Listed under the pin and never fetched, by kind.
    pub skipped: BTreeMap<String, usize>,
    /// The cache limit in bytes.
    pub cap_bytes: u64,
    /// Set when the pins together will take more than the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// What `unit.unpin` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnpinWire {
    pub unit: String,
    /// `unpinned` or `not_pinned` (nothing changed).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PinKind>,
    /// For `not_pinned`: the pin on a folder above that still keeps the
    /// file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<String>,
}

fn refusal(reason: &'static str, message: impl Into<String>) -> FetchRpcError {
    FetchRpcError {
        reason,
        message: message.into(),
        url: None,
        kind: None,
    }
}

/// Cuts `@source:path`.
fn split_unit(unit: &str) -> Result<(&str, &str), FetchRpcError> {
    unit.strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
        .filter(|(s, p)| !s.is_empty() && !p.is_empty())
        .ok_or_else(|| {
            refusal(
                "unknown_unit",
                format!(
                    "no such unit: {} (a unit id is @source:path)",
                    printable(unit)
                ),
            )
        })
}

fn db_refusal(e: impl std::fmt::Display) -> FetchRpcError {
    refusal("cache_error", format!("Tephra's index gave an error: {e}"))
}

fn the_pin_warning(footprint: u64, cap: u64) -> String {
    format!(
        "pinned files will take {footprint} bytes, more than the cache limit of {cap}: \
         pinned files are never dropped, so the cache stays over it \
         (unpinned files are dropped first)"
    )
}

impl SourceRegistry {
    /// Checks the unit names a file or folder of a remote source and gives
    /// the hub, the stored path, and whether a trailing slash asked for a
    /// folder.
    fn pin_target<'a>(
        &self,
        unit: &'a str,
    ) -> Result<(&'a str, String, bool, Arc<RemoteHub>), FetchRpcError> {
        let (source, raw) = split_unit(unit)?;
        let found = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            crate::registry::retry_cache_on_busy(&self.gate, || {
                let remote = cache.remote_source(source)?;
                let local = matches!((&remote, cache.get_source(source)?), (None, Some(_)));
                Ok((remote.is_some(), local))
            })
            .map_err(db_refusal)?
        };
        match found {
            (true, _) => {}
            (false, true) => {
                return Err(refusal(
                    "not_remote",
                    format!(
                        "{} is in a local source: there is nothing to pin",
                        printable(unit)
                    ),
                ))
            }
            (false, false) => {
                return Err(refusal(
                    "unknown_unit",
                    format!("no such unit: {}", printable(unit)),
                ))
            }
        }
        let Some(hub) = self.remote.get().cloned() else {
            return Err(refusal(
                "pin_unavailable",
                "this daemon has no remote support, so nothing can be pinned",
            ));
        };
        let (path, folder_only) = normalize_pin_path(raw).map_err(|why| {
            refusal(
                "invalid_path",
                format!("cannot pin {}: {why}", printable(unit)),
            )
        })?;
        Ok((source, path, folder_only, hub))
    }

    /// `unit.pin`: see the module documentation. Stores the pin (when it
    /// needs no confirmation or has it), marks the copies it covers, and
    /// starts the download; it does not wait for the files.
    pub fn pin_unit(&self, unit: &str, confirm: bool) -> Result<PinWire, FetchRpcError> {
        let (source, path, folder_only, hub) = self.pin_target(unit)?;
        let items = self
            .fetch_db(|c| c.remote_items(source))
            .map_err(|f| refusal(f.code, f.message))?;
        let plan = plan_pin(&items, &path, folder_only).map_err(|e| match e {
            PlanError::NotFetchable(refusal) => refusal,
            PlanError::NotFound => refusal(
                "not_found",
                format!(
                    "{} is not a file or folder in {source}; nothing was pinned",
                    printable(unit)
                ),
            ),
        })?;
        let cap_bytes = hub
            .cache_limit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bytes;
        let wire = |status: &'static str, warning: Option<String>| PinWire {
            unit: unit.to_string(),
            status,
            kind: plan.kind,
            files: plan.files,
            total_bytes: plan.total_bytes,
            skipped: plan.skipped.clone(),
            cap_bytes,
            warning,
        };

        let existing = self
            .fetch_db(|c| c.remote_pins(source))
            .map_err(|f| refusal(f.code, f.message))?;
        if existing
            .iter()
            .any(|p| p.path == plan.path && p.is_folder == plan.kind.is_folder())
        {
            return Ok(wire("already_pinned", None));
        }
        if !confirm && needs_confirm(plan.total_bytes, cap_bytes) {
            return Ok(wire("needs_confirm", None));
        }

        let now = crate::remote_github::now_secs(&*hub.clock);
        let written = self
            .fetch_db(|c| {
                let written = c.add_remote_pin(source, &plan.path, plan.kind.is_folder(), now)?;
                if written == PinWrite::New {
                    c.reconcile_remote_pins(source)?;
                }
                Ok(written)
            })
            .map_err(|f| refusal(f.code, f.message))?;
        match written {
            PinWrite::New => {}
            PinWrite::Existing => return Ok(wire("already_pinned", None)),
            PinWrite::OtherKind => {
                return Err(refusal(
                    "pin_conflict",
                    format!(
                        "{} is already pinned as a {}: unpin it first",
                        printable(unit),
                        if plan.kind.is_folder() {
                            "file"
                        } else {
                            "folder"
                        }
                    ),
                ))
            }
        }
        tracing::info!(source, kind = ?plan.kind, files = plan.files, "pinned");
        wake(&hub, source);

        let footprint = self
            .fetch_db(|c| c.remote_pin_footprint())
            .map_err(|f| refusal(f.code, f.message))?;
        let warning = (footprint > cap_bytes).then(|| the_pin_warning(footprint, cap_bytes));
        Ok(wire("pinned", warning))
    }

    /// `unit.unpin`: removes the pin the unit names (a file's, or a
    /// folder's when it ends in a slash or no file pin has the name). The
    /// copies stay on disk and become evictable, unless another pin still
    /// covers them. Nothing to remove is not an error.
    pub fn unpin_unit(&self, unit: &str) -> Result<UnpinWire, FetchRpcError> {
        let (source, path, folder_only, _hub) = self.pin_target(unit)?;
        let (removed, covered_by) = self
            .fetch_db(|c| {
                let pins = c.remote_pins(source)?;
                let mine = pins
                    .iter()
                    .filter(|p| p.path == path && (p.is_folder || !folder_only))
                    // A file pin of that name before a folder pin of it.
                    .min_by_key(|p| p.is_folder);
                if let Some(pin) = mine {
                    c.remove_remote_pin(source, &pin.path, pin.is_folder)?;
                    c.reconcile_remote_pins(source)?;
                    return Ok((
                        Some(if pin.is_folder {
                            PinKind::Folder
                        } else {
                            PinKind::File
                        }),
                        None,
                    ));
                }
                let above = pins
                    .iter()
                    .filter(|p| pin_covers(&p.path, p.is_folder, &path))
                    .map(|p| p.path.clone())
                    .min_by_key(String::len);
                Ok((None, above))
            })
            .map_err(|f| refusal(f.code, f.message))?;
        match removed {
            Some(kind) => {
                tracing::info!(source, ?kind, "unpinned");
                Ok(UnpinWire {
                    unit: unit.to_string(),
                    status: "unpinned",
                    kind: Some(kind),
                    covered_by: None,
                })
            }
            None => Ok(UnpinWire {
                unit: unit.to_string(),
                status: "not_pinned",
                kind: None,
                covered_by,
            }),
        }
    }

    /// One pass of the pin download of `source`: fetches every covered file
    /// that has no copy, once each, until none is left to try. Returns at
    /// once when the remote is away (offline, a rejected token, a rate
    /// limit): the next sync starts it again.
    fn pin_pass(&self, source: &str) {
        let mut tried: HashSet<String> = HashSet::new();
        loop {
            let rows = match self.fetch_db(|c| c.remote_pin_downloads(source)) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(source, error = %e.message, "could not read the files a pin still needs");
                    return;
                }
            };
            let todo: Vec<RemoteItemRow> = rows
                .into_iter()
                .filter(|r| !tried.contains(&r.path))
                .collect();
            if todo.is_empty() {
                return;
            }
            for row in todo {
                tried.insert(row.path.clone());
                if not_fetchable_item(
                    row.kind.as_deref(),
                    row.file_id.as_deref(),
                    row.reason.as_deref(),
                )
                .is_some()
                {
                    continue;
                }
                // The pin may have been dropped since the list was read.
                if !self
                    .fetch_db(|c| c.remote_pin_covers(source, &row.path))
                    .unwrap_or(false)
                {
                    continue;
                }
                match self.fetch_unit(&format!("@{source}:{}", row.path)) {
                    Ok(_) => {}
                    Err(e) if matches!(e.reason, "offline" | "auth_required" | "rate_limited") => {
                        tracing::info!(
                            source,
                            reason = e.reason,
                            "the pin download waits for the next sync"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::info!(
                            source,
                            reason = e.reason,
                            "a pinned file did not download; the next sync tries again"
                        );
                    }
                }
            }
        }
    }

    /// Start-up: the pins of every source with one are filled (or filled
    /// again after a restart that cut a download).
    pub(crate) fn resume_pin_downloads(&self) {
        let Some(hub) = self.remote.get().cloned() else {
            return;
        };
        let sources = match self.fetch_db(|c| c.remote_pinned_sources()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e.message, "could not list the pinned sources at start");
                return;
            }
        };
        for source in sources {
            wake(&hub, &source);
        }
    }
}

// ---- the job -------------------------------------------------------------------------

/// The pin jobs running now, one per source. `true` says another pass was
/// asked for while one ran.
pub(crate) type PinJobs = Mutex<HashMap<String, bool>>;

/// Removes a source's job entry however the job ends.
struct JobGuard {
    hub: Arc<RemoteHub>,
    source: String,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.hub
            .pin_jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.source);
    }
}

/// Starts the pin download of `source`, or asks the one running to go round
/// again. Does nothing while the hub has no registry (a daemon that did not
/// set one). Never blocks: the work is on its own thread.
pub(crate) fn wake(hub: &Arc<RemoteHub>, source: &str) {
    let Some(registry) = hub.registry.get().and_then(std::sync::Weak::upgrade) else {
        return;
    };
    {
        let mut jobs = hub.pin_jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(again) = jobs.get_mut(source) {
            *again = true;
            return;
        }
        jobs.insert(source.to_string(), false);
    }
    let guard = JobGuard {
        hub: hub.clone(),
        source: source.to_string(),
    };
    let spawned = std::thread::Builder::new()
        .name("tephra-pin".to_string())
        .spawn(move || {
            let guard = guard;
            loop {
                registry.pin_pass(&guard.source);
                let mut jobs = guard.hub.pin_jobs.lock().unwrap_or_else(|p| p.into_inner());
                match jobs.get_mut(&guard.source) {
                    Some(again) if *again => *again = false,
                    _ => break,
                }
            }
        });
    if let Err(e) = spawned {
        // The closure (and its guard) was dropped: the entry is gone.
        tracing::warn!(source, error = %e, "could not start the pin download");
    }
}
