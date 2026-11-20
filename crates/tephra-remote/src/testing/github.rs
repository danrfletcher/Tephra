//! The fake GitHub server: the REST routes [`crate::github::GithubClient`]
//! uses, over an in-memory set of repos.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::json;

use super::{etagged, fake_sha, json_response, serve, with_header, Common, Flavor, Served};
use crate::clock::{Clock, SystemClock};

/// How the fake rate-limits, in GitHub's styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubLimit {
    /// 403 with `Retry-After`.
    RetryAfter403,
    /// 429 with `Retry-After`.
    RetryAfter429,
    /// 403 with `x-ratelimit-remaining: 0` and `x-ratelimit-reset`.
    Reset403,
    /// 403 whose body names a secondary rate limit, with no rate headers.
    Secondary403,
}

const SECONDARY_TEXT: &str =
    "You have exceeded a secondary rate limit. Please wait a few minutes before you try again.";

fn error_body(status: u16) -> Response {
    let message = match status {
        401 => "Bad credentials",
        403 => "Resource not accessible by integration",
        404 => "Not Found",
        429 => "Too Many Requests",
        500..=599 => "Server Error",
        _ => "Error",
    };
    json_response(status, &json!({ "message": message }))
}

fn limited(style: &GithubLimit, secs: u64, now: u64) -> Response {
    match style {
        GithubLimit::RetryAfter403 => with_header(
            json_response(403, &json!({ "message": SECONDARY_TEXT })),
            "retry-after",
            &secs.to_string(),
        ),
        GithubLimit::RetryAfter429 => with_header(
            json_response(429, &json!({ "message": "Too Many Requests" })),
            "retry-after",
            &secs.to_string(),
        ),
        GithubLimit::Reset403 => {
            let r = json_response(403, &json!({ "message": "API rate limit exceeded" }));
            let r = with_header(r, "x-ratelimit-limit", "60");
            let r = with_header(r, "x-ratelimit-remaining", "0");
            with_header(r, "x-ratelimit-reset", &(now + secs).to_string())
        }
        GithubLimit::Secondary403 => json_response(403, &json!({ "message": SECONDARY_TEXT })),
    }
}

struct Branch {
    files: BTreeMap<String, Vec<u8>>,
    /// Counts commits, so every change gives a new head sha.
    commits: u64,
}

struct Repo {
    id: u64,
    default_branch: String,
    branches: BTreeMap<String, Branch>,
    /// The token cannot see it: every route is a 404.
    hidden: bool,
    /// Every route is a 301 to this location.
    moved_to: Option<String>,
}

struct State_ {
    login: String,
    next_id: u64,
    repos: BTreeMap<String, Repo>,
}

impl Branch {
    fn tree_sha(&self) -> String {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        for (path, bytes) in &self.files {
            parts.push(path.as_bytes().to_vec());
            parts.push(fake_sha(&[bytes]).into_bytes());
        }
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        fake_sha(&refs)
    }

    fn head_sha(&self) -> String {
        fake_sha(&[
            b"commit",
            self.tree_sha().as_bytes(),
            self.commits.to_string().as_bytes(),
        ])
    }
}

/// The fake GitHub. Dropping it closes the port.
pub struct FakeGithub {
    base_url: String,
    state: Arc<Mutex<State_>>,
    common: Arc<Common<GithubLimit>>,
    _served: Served,
}

type Shared = (Arc<Mutex<State_>>,);

impl FakeGithub {
    /// Starts on `127.0.0.1:0` with the wall clock.
    pub async fn start() -> FakeGithub {
        Self::start_with_clock(Arc::new(SystemClock)).await
    }

    /// Starts with `clock` as the time source for rate-limit windows.
    pub async fn start_with_clock(clock: Arc<dyn Clock>) -> FakeGithub {
        let state = Arc::new(Mutex::new(State_ {
            login: "fake-user".to_string(),
            next_id: 1000,
            repos: BTreeMap::new(),
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
            .route("/user", get(user))
            .route("/repos/{owner}/{repo}", get(repo_info))
            .route("/repos/{owner}/{repo}/git/trees/{*git_ref}", get(tree))
            .route("/repos/{owner}/{repo}/git/blobs/{sha}", get(blob))
            .route("/repos/{owner}/{repo}/contents/{*path}", get(contents))
            .route("/repos/{owner}/{repo}/commits/{*git_ref}", get(commit))
            .with_state((state.clone(),));
        let served = serve(router, common.clone()).await;
        FakeGithub {
            base_url: format!("http://{}", served.addr),
            state,
            common,
            _served: served,
        }
    }

    super::scripting_methods!(GithubLimit);

    fn state(&self) -> MutexGuard<'_, State_> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The login `GET /user` answers with.
    pub fn set_login(&self, login: &str) {
        self.state().login = login.to_string();
    }

    /// Adds an empty repo `owner/name` with this default branch.
    pub fn add_repo(&self, full_name: &str, default_branch: &str) {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id += 1;
        let mut branches = BTreeMap::new();
        branches.insert(
            default_branch.to_string(),
            Branch {
                files: BTreeMap::new(),
                commits: 1,
            },
        );
        state.repos.insert(
            full_name.to_string(),
            Repo {
                id,
                default_branch: default_branch.to_string(),
                branches,
                hidden: false,
                moved_to: None,
            },
        );
    }

    /// Writes a file on a branch (creating the branch if need be) as a
    /// new commit; returns the new head sha.
    pub fn put_file(&self, full_name: &str, branch: &str, path: &str, bytes: &[u8]) -> String {
        let mut state = self.state();
        let repo = state.repos.get_mut(full_name).expect("repo was added");
        let b = repo.branches.entry(branch.to_string()).or_insert(Branch {
            files: BTreeMap::new(),
            commits: 0,
        });
        b.files.insert(path.to_string(), bytes.to_vec());
        b.commits += 1;
        b.head_sha()
    }

    /// Removes a file from a branch as a new commit; returns the head sha.
    pub fn remove_file(&self, full_name: &str, branch: &str, path: &str) -> String {
        let mut state = self.state();
        let repo = state.repos.get_mut(full_name).expect("repo was added");
        let b = repo.branches.get_mut(branch).expect("branch exists");
        b.files.remove(path);
        b.commits += 1;
        b.head_sha()
    }

    /// The head sha of a branch.
    pub fn head_sha(&self, full_name: &str, branch: &str) -> String {
        let state = self.state();
        state.repos[full_name].branches[branch].head_sha()
    }

    /// The blob sha the fake gives to this content.
    pub fn blob_sha(bytes: &[u8]) -> String {
        fake_sha(&[bytes])
    }

    /// A repo the token cannot see: GitHub answers 404 for a private repo
    /// without access, exactly as for one that does not exist.
    pub fn set_hidden(&self, full_name: &str, hidden: bool) {
        self.state()
            .repos
            .get_mut(full_name)
            .expect("repo was added")
            .hidden = hidden;
    }

    /// Every route of this repo answers 301 with this `Location`.
    pub fn move_repo(&self, full_name: &str, location: &str) {
        self.state()
            .repos
            .get_mut(full_name)
            .expect("repo was added")
            .moved_to = Some(location.to_string());
    }

    /// Forgets a scripted move.
    pub fn clear_move(&self, full_name: &str) {
        self.state()
            .repos
            .get_mut(full_name)
            .expect("repo was added")
            .moved_to = None;
    }
}

/// A response that ends a handler early; boxed because a `Response` is
/// large for an `Err`.
type Early = Box<Response>;

fn not_found() -> Early {
    Box::new(error_body(404))
}

/// Finds the repo, or the 404 / 301 a caller should get instead.
fn with_repo<T>(
    shared: &Shared,
    owner: &str,
    repo: &str,
    f: impl FnOnce(&Repo) -> Result<T, Early>,
) -> Result<T, Early> {
    let state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = state.repos.get(&format!("{owner}/{repo}")) else {
        return Err(not_found());
    };
    if let Some(location) = &r.moved_to {
        let mut response = json_response(301, &json!({ "message": "Moved Permanently" }));
        response = with_header(response, "location", location);
        return Err(Box::new(response));
    }
    if r.hidden {
        return Err(not_found());
    }
    f(r)
}

fn resolve<'a>(repo: &'a Repo, git_ref: &str) -> Option<&'a Branch> {
    if git_ref == "HEAD" {
        return repo.branches.get(&repo.default_branch);
    }
    if let Some(b) = repo.branches.get(git_ref) {
        return Some(b);
    }
    repo.branches.values().find(|b| b.head_sha() == git_ref)
}

fn unwrap(result: Result<Response, Early>) -> Response {
    match result {
        Ok(r) => r,
        Err(r) => *r,
    }
}

async fn user(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let login = shared
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .login
        .clone();
    let body = json!({ "login": login, "id": 1 }).to_string().into_bytes();
    etagged(&headers, "application/json", body)
}

async fn repo_info(
    State(shared): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    unwrap(with_repo(&shared, &owner, &repo, |r| {
        let body = json!({
            "id": r.id,
            "full_name": format!("{owner}/{repo}"),
            "default_branch": r.default_branch,
            "private": r.hidden,
        });
        Ok(etagged(
            &headers,
            "application/json",
            body.to_string().into_bytes(),
        ))
    }))
}

async fn tree(
    State(shared): State<Shared>,
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    unwrap(with_repo(&shared, &owner, &repo, |r| {
        let Some(branch) = resolve(r, &git_ref) else {
            return Err(not_found());
        };
        let mut dirs = BTreeSet::new();
        for path in branch.files.keys() {
            let mut at = 0;
            while let Some(i) = path[at..].find('/') {
                dirs.insert(path[..at + i].to_string());
                at += i + 1;
            }
        }
        let mut entries = Vec::new();
        for dir in &dirs {
            entries.push(json!({
                "path": dir, "mode": "040000", "type": "tree",
                "sha": fake_sha(&[b"tree", dir.as_bytes()]),
            }));
        }
        for (path, bytes) in &branch.files {
            entries.push(json!({
                "path": path, "mode": "100644", "type": "blob",
                "sha": fake_sha(&[bytes]), "size": bytes.len(),
            }));
        }
        entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        let body = json!({
            "sha": branch.tree_sha(),
            "tree": entries,
            "truncated": false,
        });
        Ok(etagged(
            &headers,
            "application/json",
            body.to_string().into_bytes(),
        ))
    }))
}

async fn blob(
    State(shared): State<Shared>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    unwrap(with_repo(&shared, &owner, &repo, |r| {
        let found = r
            .branches
            .values()
            .flat_map(|b| b.files.values())
            .find(|bytes| fake_sha(&[bytes]) == sha);
        match found {
            Some(bytes) => Ok(etagged(&headers, "application/octet-stream", bytes.clone())),
            None => Err(not_found()),
        }
    }))
}

async fn contents(
    State(shared): State<Shared>,
    Path((owner, repo, path)): Path<(String, String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    unwrap(with_repo(&shared, &owner, &repo, |r| {
        let git_ref = query.get("ref").map(String::as_str).unwrap_or("HEAD");
        let Some(branch) = resolve(r, git_ref) else {
            return Err(not_found());
        };
        match branch.files.get(&path) {
            Some(bytes) => Ok(etagged(
                &headers,
                "application/vnd.github.raw+json",
                bytes.clone(),
            )),
            None => Err(not_found()),
        }
    }))
}

async fn commit(
    State(shared): State<Shared>,
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    unwrap(with_repo(&shared, &owner, &repo, |r| {
        let Some(branch) = resolve(r, &git_ref) else {
            return Err(not_found());
        };
        let accepts_sha = headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("vnd.github.sha"));
        let sha = branch.head_sha();
        if accepts_sha {
            Ok(etagged(&headers, "text/plain", sha.into_bytes()))
        } else {
            let body = json!({ "sha": sha });
            Ok(etagged(
                &headers,
                "application/json",
                body.to_string().into_bytes(),
            ))
        }
    }))
}
