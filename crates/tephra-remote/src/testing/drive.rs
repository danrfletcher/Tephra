//! The fake Google Drive server: the routes [`crate::drive::DriveClient`]
//! uses, over an in-memory Drive. Routes live under `/drive/v3`, so the
//! base URL the adapter gets ends the way the real one does.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use super::{etagged, json_response, serve, with_header, Common, Flavor, Served};
use crate::clock::{Clock, SystemClock};
use crate::drive::{FOLDER_MIME, GOOGLE_DOC_MIME};

/// How the fake rate-limits, in Google's styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveLimit {
    /// 429, with `Retry-After` when `retry_after`.
    Status429 { retry_after: bool },
    /// 403 with this `error.errors[].reason` (`rateLimitExceeded`,
    /// `userRateLimitExceeded`, `sharingRateLimitExceeded`), with
    /// `Retry-After` when `retry_after`.
    Reason403 {
        reason: &'static str,
        retry_after: bool,
    },
}

fn google_error(status: u16, reason: &str, message: &str) -> Response {
    json_response(
        status,
        &json!({
            "error": {
                "code": status,
                "message": message,
                "errors": [{ "domain": "global", "reason": reason, "message": message }],
            }
        }),
    )
}

fn error_body(status: u16) -> Response {
    match status {
        401 => google_error(401, "authError", "Invalid Credentials"),
        403 => google_error(403, "insufficientPermissions", "Insufficient Permission"),
        404 => google_error(404, "notFound", "File not found"),
        429 => google_error(429, "rateLimitExceeded", "Rate Limit Exceeded"),
        s => google_error(s, "backendError", "Backend Error"),
    }
}

fn limited(style: &DriveLimit, secs: u64, _now: u64) -> Response {
    let (response, retry_after) = match style {
        DriveLimit::Status429 { retry_after } => (
            google_error(429, "rateLimitExceeded", "Rate Limit Exceeded"),
            *retry_after,
        ),
        DriveLimit::Reason403 {
            reason,
            retry_after,
        } => (
            google_error(403, reason, "Rate Limit Exceeded"),
            *retry_after,
        ),
    };
    if retry_after {
        with_header(response, "retry-after", &secs.to_string())
    } else {
        response
    }
}

#[derive(Clone)]
struct Entry {
    id: String,
    name: String,
    mime: String,
    parents: Vec<String>,
    content: Vec<u8>,
    revision: u64,
    /// RFC 3339, as Drive sends it. Later insertions get later times
    /// unless a test sets one.
    created: String,
    trashed: bool,
    drive_id: Option<String>,
    /// Pretend the file has this size whatever its bytes are (a file past
    /// the index cap, without holding 20 MB in memory).
    claimed_size: Option<u64>,
}

impl Entry {
    fn native(&self) -> bool {
        self.mime.starts_with("application/vnd.google-apps.")
    }

    fn metadata(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "name": self.name,
            "mimeType": self.mime,
            "parents": self.parents,
            "createdTime": self.created,
            "trashed": self.trashed,
            "version": self.revision.to_string(),
        });
        if let Some(drive) = &self.drive_id {
            v["driveId"] = json!(drive);
        }
        if !self.native() {
            v["headRevisionId"] = json!(format!("rev-{}-{}", self.id, self.revision));
            v["md5Checksum"] = json!(format!("md5-{}-{}", self.id, self.revision));
            v["size"] = json!(self
                .claimed_size
                .unwrap_or(self.content.len() as u64)
                .to_string());
        }
        v
    }
}

/// Holds the content reads (`alt=media` and export) of the fake once a
/// budget is spent, until [`FakeDrive::release_content`]: a test looks at
/// a source part-way through its first index.
struct Gate {
    /// Reads still let through; `None` lets every read through.
    budget: Mutex<Option<usize>>,
    /// The same for the listing pages (`files.list`); `None` lets every
    /// page through.
    listing_budget: Mutex<Option<usize>>,
    released: tokio::sync::Notify,
    /// `(reads still to fail, status)`: the next content reads are answered
    /// with `status` (in Google's error format) instead of the file.
    failing: Mutex<Option<(u32, u16)>>,
}

impl Gate {
    /// The error answer a failing content read gets, once per read.
    fn failure(&self) -> Option<Response> {
        let mut failing = self.failing.lock().unwrap_or_else(|e| e.into_inner());
        let (left, status) = (*failing)?;
        *failing = (left > 1).then_some((left - 1, status));
        Some(error_body(status))
    }

    async fn pass(&self) {
        self.pass_budget(&self.budget).await
    }

    /// [`Gate::pass`] for one listing page.
    async fn pass_listing(&self) {
        self.pass_budget(&self.listing_budget).await
    }

    async fn pass_budget(&self, spent: &Mutex<Option<usize>>) {
        loop {
            let notified = self.released.notified();
            tokio::pin!(notified);
            // Registered before the budget is looked at, so a release in
            // between is not missed.
            notified.as_mut().enable();
            {
                let mut budget = spent.lock().unwrap_or_else(|e| e.into_inner());
                match budget.as_mut() {
                    None => return,
                    Some(n) if *n > 0 => {
                        *n -= 1;
                        return;
                    }
                    Some(_) => {}
                }
            }
            notified.await;
        }
    }
}

struct ChangeRecord {
    seq: u64,
    file_id: String,
    removed: bool,
}

struct State_ {
    files: BTreeMap<String, Entry>,
    changes: Vec<ChangeRecord>,
    seq: u64,
    page_size: Option<usize>,
    user_name: String,
    /// Change page tokens below this are answered 400 (Drive forgets old
    /// ones).
    reject_tokens_below: u64,
    /// `(pages still answered, status)`: the changes page after that many
    /// is answered with `status` once.
    fail_changes_page: Option<(u32, u16)>,
    created_counter: u64,
}

type Shared = (Arc<Mutex<State_>>, Arc<Gate>);

impl State_ {
    fn record(&mut self, id: &str, removed: bool) {
        self.seq += 1;
        let seq = self.seq;
        self.changes.push(ChangeRecord {
            seq,
            file_id: id.to_string(),
            removed,
        });
    }
}

/// The fake Drive. Dropping it closes the port.
pub struct FakeDrive {
    base_url: String,
    state: Arc<Mutex<State_>>,
    gate: Arc<Gate>,
    common: Arc<Common<DriveLimit>>,
    _served: Served,
}

impl FakeDrive {
    /// Starts on `127.0.0.1:0` with the wall clock.
    pub async fn start() -> FakeDrive {
        Self::start_with_clock(Arc::new(SystemClock)).await
    }

    /// Starts with `clock` as the time source for rate-limit windows.
    pub async fn start_with_clock(clock: Arc<dyn Clock>) -> FakeDrive {
        let state = Arc::new(Mutex::new(State_ {
            files: BTreeMap::new(),
            changes: Vec::new(),
            seq: 100,
            page_size: None,
            user_name: "Fake User".to_string(),
            reject_tokens_below: 0,
            fail_changes_page: None,
            created_counter: 0,
        }));
        let gate = Arc::new(Gate {
            budget: Mutex::new(None),
            listing_budget: Mutex::new(None),
            released: tokio::sync::Notify::new(),
            failing: Mutex::new(None),
        });
        let common = Arc::new(Common::new(
            clock,
            Flavor {
                unauthorized: || error_body(401),
                error: error_body,
                limited,
            },
        ));
        let router = Router::new()
            .route("/drive/v3/about", get(about))
            .route("/drive/v3/files", get(list_files))
            .route("/drive/v3/files/{id}", get(file))
            .route("/drive/v3/files/{id}/export", get(export))
            .route("/drive/v3/changes/startPageToken", get(start_token))
            .route("/drive/v3/changes", get(list_changes))
            .with_state((state.clone(), gate.clone()));
        let served = serve(router, common.clone()).await;
        FakeDrive {
            base_url: format!("http://{}/drive/v3", served.addr),
            state,
            gate,
            common,
            _served: served,
        }
    }

    super::scripting_methods!(DriveLimit);

    fn state(&self) -> MutexGuard<'_, State_> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The display name `GET /about` answers with.
    pub fn set_user_name(&self, name: &str) {
        self.state().user_name = name.to_string();
    }

    /// At most this many items per page, whatever the request asks for
    /// (children and changes both).
    pub fn set_page_size(&self, n: usize) {
        self.state().page_size = Some(n.max(1));
    }

    /// Adds a folder (`parent` `None` puts it in `root`).
    pub fn add_folder(&self, id: &str, name: &str, parent: Option<&str>) {
        self.insert(id, name, FOLDER_MIME, parent, Vec::new());
    }

    /// Adds a binary or text file.
    pub fn add_file(&self, id: &str, name: &str, mime: &str, parent: Option<&str>, bytes: &[u8]) {
        self.insert(id, name, mime, parent, bytes.to_vec());
    }

    /// Adds a native Google Doc; `markdown` is what its export returns.
    pub fn add_doc(&self, id: &str, name: &str, parent: Option<&str>, markdown: &str) {
        self.insert(
            id,
            name,
            GOOGLE_DOC_MIME,
            parent,
            markdown.as_bytes().to_vec(),
        );
    }

    /// Adds a native Google file of another kind (`mime` is
    /// `application/vnd.google-apps.spreadsheet`, `.presentation`, `.form`,
    /// `.drawing` or `.shortcut`). It has no content and no export.
    pub fn add_native(&self, id: &str, name: &str, mime: &str, parent: Option<&str>) {
        self.insert(id, name, mime, parent, Vec::new());
    }

    fn insert(&self, id: &str, name: &str, mime: &str, parent: Option<&str>, content: Vec<u8>) {
        let mut state = self.state();
        state.created_counter += 1;
        let created = created_time(state.created_counter);
        state.files.insert(
            id.to_string(),
            Entry {
                id: id.to_string(),
                name: name.to_string(),
                mime: mime.to_string(),
                parents: vec![parent.unwrap_or("root").to_string()],
                content,
                revision: 1,
                created,
                trashed: false,
                drive_id: None,
                claimed_size: None,
            },
        );
        state.record(id, false);
    }

    /// New content, a new head revision and a change.
    pub fn update_file(&self, id: &str, bytes: &[u8]) {
        let mut state = self.state();
        let e = state.files.get_mut(id).expect("file was added");
        e.content = bytes.to_vec();
        e.revision += 1;
        state.record(id, false);
    }

    /// A new name for the same file id, and a change.
    pub fn rename_file(&self, id: &str, name: &str) {
        let mut state = self.state();
        state.files.get_mut(id).expect("file was added").name = name.to_string();
        state.record(id, false);
    }

    /// Moves a file (or folder) to another parent, and a change.
    pub fn move_file(&self, id: &str, parent: &str) {
        let mut state = self.state();
        state.files.get_mut(id).expect("file was added").parents = vec![parent.to_string()];
        state.record(id, false);
    }

    /// Gives a file a second parent (a file with several parents lists
    /// under each; the first one is the file's own), and a change.
    pub fn add_parent(&self, id: &str, parent: &str) {
        let mut state = self.state();
        state
            .files
            .get_mut(id)
            .expect("file was added")
            .parents
            .push(parent.to_string());
        state.record(id, false);
    }

    /// Puts a file in the bin: it stays, with `trashed` true, and a change.
    pub fn trash_file(&self, id: &str) {
        let mut state = self.state();
        state.files.get_mut(id).expect("file was added").trashed = true;
        state.record(id, false);
    }

    /// Removes a file and records a `removed` change.
    pub fn remove_file(&self, id: &str) {
        let mut state = self.state();
        state.files.remove(id);
        state.record(id, true);
    }

    /// Says the file is in a Shared Drive (its `driveId`). No change is
    /// recorded: the file was always there.
    pub fn set_drive_id(&self, id: &str, drive_id: &str) {
        let mut state = self.state();
        state.files.get_mut(id).expect("file was added").drive_id = Some(drive_id.to_string());
    }

    /// Sets the created time (RFC 3339) of a file, to order duplicates.
    pub fn set_created_time(&self, id: &str, created: &str) {
        let mut state = self.state();
        state.files.get_mut(id).expect("file was added").created = created.to_string();
    }

    /// The listing says the file is this big (a file past the index cap,
    /// without its bytes in memory). A read of it still returns what it
    /// holds.
    pub fn claim_size(&self, id: &str, size: u64) {
        let mut state = self.state();
        state
            .files
            .get_mut(id)
            .expect("file was added")
            .claimed_size = Some(size);
    }

    /// Drive forgets every change page token older than now: a poll that
    /// sends one gets a 400, as when a stored token has expired. A token
    /// from `changes/startPageToken` taken now still works.
    pub fn forget_old_tokens(&self) {
        let mut state = self.state();
        state.reject_tokens_below = state.seq;
    }

    /// Answers `pages` more changes pages, then fails the next one once
    /// with `status`: a failure in the middle of a long feed.
    pub fn fail_changes_after(&self, pages: u32, status: u16) {
        self.state().fail_changes_page = Some((pages, status));
    }

    /// Lets `n` more content reads through (`alt=media` or export) and
    /// holds every one after them until [`FakeDrive::release_content`].
    /// Listings and the changes feed are never held.
    pub fn hold_content_after(&self, n: usize) {
        *self.gate.budget.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    /// Lets `n` more listing pages (`files.list`) through and holds every
    /// one after them until [`FakeDrive::release_listing`]: a test looks at
    /// a source part-way through its listing. Content reads and the
    /// changes feed are not held.
    pub fn hold_listing_after(&self, n: usize) {
        *self
            .gate
            .listing_budget
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    /// Lets the held listing pages, and every later one, through.
    pub fn release_listing(&self) {
        *self
            .gate
            .listing_budget
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.gate.released.notify_waiters();
    }

    /// Answers the next `n` content reads (`alt=media` or export) with
    /// `status`; listings and the feed are not touched.
    pub fn fail_content_reads(&self, n: u32, status: u16) {
        *self.gate.failing.lock().unwrap_or_else(|e| e.into_inner()) = Some((n, status));
    }

    /// Lets the held content reads, and every later one, through.
    pub fn release_content(&self) {
        *self.gate.budget.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.gate.released.notify_waiters();
    }

    /// The head revision id the fake reports for a file.
    pub fn head_revision(&self, id: &str) -> String {
        format!("rev-{id}-{}", self.state().files[id].revision)
    }
}

/// A created time that rises with `n` (a fixed day, `n` seconds in).
fn created_time(n: u64) -> String {
    format!(
        "2026-01-01T{:02}:{:02}:{:02}.000Z",
        (n / 3600) % 24,
        (n / 60) % 60,
        n % 60
    )
}

fn not_found() -> Response {
    error_body(404)
}

fn lock(shared: &Shared) -> MutexGuard<'_, State_> {
    shared.0.lock().unwrap_or_else(|e| e.into_inner())
}

async fn about(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let name = lock(&shared).user_name.clone();
    let body = json!({
        "user": { "displayName": name, "emailAddress": "fake@example.test", "permissionId": "perm-1" }
    });
    etagged(&headers, "application/json", body.to_string().into_bytes())
}

/// The id inside `'ID' in parents and trashed = false`.
fn parent_from_query(q: &str) -> Option<String> {
    let start = q.find('\'')? + 1;
    let end = start + q[start..].find('\'')?;
    Some(q[start..end].to_string())
}

async fn list_files(
    State(shared): State<Shared>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    shared.1.pass_listing().await;
    let Some(parent) = query.get("q").and_then(|q| parent_from_query(q)) else {
        return google_error(400, "invalidQuery", "Invalid Value");
    };
    let state = lock(&shared);
    let children: Vec<&Entry> = state
        .files
        .values()
        .filter(|e| e.parents.contains(&parent) && !e.trashed)
        .collect();
    let requested = query
        .get("pageSize")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(100);
    let size = state
        .page_size
        .map_or(requested, |p| p.min(requested))
        .max(1);
    let offset = match query.get("pageToken") {
        None => 0,
        Some(t) => match t
            .strip_prefix("page-")
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) => n,
            None => return google_error(400, "invalid", "Invalid Value"),
        },
    };
    let page: Vec<Value> = children
        .iter()
        .skip(offset)
        .take(size)
        .map(|e| e.metadata())
        .collect();
    let mut body = json!({ "files": page });
    if offset + size < children.len() {
        body["nextPageToken"] = json!(format!("page-{}", offset + size));
    }
    etagged(&headers, "application/json", body.to_string().into_bytes())
}

async fn file(
    State(shared): State<Shared>,
    Path(id): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if query.get("alt").map(String::as_str) == Some("media") {
        shared.1.pass().await;
        if let Some(failed) = shared.1.failure() {
            return failed;
        }
    }
    let state = lock(&shared);
    let Some(e) = state.files.get(&id) else {
        return not_found();
    };
    if query.get("alt").map(String::as_str) == Some("media") {
        if e.native() || e.mime == FOLDER_MIME {
            return google_error(
                403,
                "fileNotDownloadable",
                "Only files with binary content can be downloaded. Use Export with Docs Editors files.",
            );
        }
        return etagged(&headers, "application/octet-stream", e.content.clone());
    }
    etagged(
        &headers,
        "application/json",
        e.metadata().to_string().into_bytes(),
    )
}

async fn export(
    State(shared): State<Shared>,
    Path(id): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    shared.1.pass().await;
    if let Some(failed) = shared.1.failure() {
        return failed;
    }
    let state = lock(&shared);
    let Some(e) = state.files.get(&id) else {
        return not_found();
    };
    if !e.native() || e.mime == FOLDER_MIME {
        return google_error(
            403,
            "exportNotSupported",
            "Export only supports Docs Editors files.",
        );
    }
    if query.get("mimeType").map(String::as_str) != Some("text/markdown") {
        return google_error(400, "badRequest", "Unsupported export format");
    }
    etagged(&headers, "text/markdown", e.content.clone())
}

async fn start_token(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let seq = lock(&shared).seq;
    let body = json!({ "startPageToken": seq.to_string() });
    etagged(&headers, "application/json", body.to_string().into_bytes())
}

async fn list_changes(
    State(shared): State<Shared>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(since) = query.get("pageToken").and_then(|t| t.parse::<u64>().ok()) else {
        return google_error(400, "invalid", "Invalid Value");
    };
    let mut state = lock(&shared);
    if let Some((left, status)) = state.fail_changes_page {
        if left == 0 {
            state.fail_changes_page = None;
            return error_body(status);
        }
        state.fail_changes_page = Some((left - 1, status));
    }
    if since < state.reject_tokens_below {
        return google_error(400, "invalid", "Invalid Value");
    }
    let requested = query
        .get("pageSize")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(100);
    let size = state
        .page_size
        .map_or(requested, |p| p.min(requested))
        .max(1);
    let pending: Vec<&ChangeRecord> = state.changes.iter().filter(|c| c.seq > since).collect();
    let page: Vec<&&ChangeRecord> = pending.iter().take(size).collect();
    let changes: Vec<Value> = page
        .iter()
        .map(|c| {
            let mut v = json!({
                "fileId": c.file_id,
                "removed": c.removed,
                "time": "2026-11-20T00:00:00.000Z",
            });
            if !c.removed {
                if let Some(e) = state.files.get(&c.file_id) {
                    v["file"] = e.metadata();
                }
            }
            v
        })
        .collect();
    let mut body = json!({ "changes": changes });
    if pending.len() > size {
        let last = page.last().map_or(since, |c| c.seq);
        body["nextPageToken"] = json!(last.to_string());
    } else {
        body["newStartPageToken"] = json!(state.seq.to_string());
    }
    etagged(&headers, "application/json", body.to_string().into_bytes())
}
