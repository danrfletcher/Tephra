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
        });
        if !self.native() {
            v["headRevisionId"] = json!(format!("rev-{}-{}", self.id, self.revision));
            v["size"] = json!(self.content.len().to_string());
        }
        v
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
}

type Shared = (Arc<Mutex<State_>>,);

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
        }));
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
            .with_state((state.clone(),));
        let served = serve(router, common.clone()).await;
        FakeDrive {
            base_url: format!("http://{}/drive/v3", served.addr),
            state,
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

    fn insert(&self, id: &str, name: &str, mime: &str, parent: Option<&str>, content: Vec<u8>) {
        let mut state = self.state();
        state.files.insert(
            id.to_string(),
            Entry {
                id: id.to_string(),
                name: name.to_string(),
                mime: mime.to_string(),
                parents: vec![parent.unwrap_or("root").to_string()],
                content,
                revision: 1,
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

    /// Removes a file and records a `removed` change.
    pub fn remove_file(&self, id: &str) {
        let mut state = self.state();
        state.files.remove(id);
        state.record(id, true);
    }

    /// The head revision id the fake reports for a file.
    pub fn head_revision(&self, id: &str) -> String {
        format!("rev-{id}-{}", self.state().files[id].revision)
    }
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
    let Some(parent) = query.get("q").and_then(|q| parent_from_query(q)) else {
        return google_error(400, "invalidQuery", "Invalid Value");
    };
    let state = lock(&shared);
    let children: Vec<&Entry> = state
        .files
        .values()
        .filter(|e| e.parents.contains(&parent))
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
    let state = lock(&shared);
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
