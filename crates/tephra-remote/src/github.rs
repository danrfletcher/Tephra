//! GitHub, read side: the [`GithubAdapter`] trait and [`GithubClient`],
//! its HTTP implementation.
//!
//! The trait names one repo per call. There is no method that lists the
//! repos of an account or an organisation, and none that writes: the
//! adapter answers "what is in this repo at this ref" and nothing else.
//! Every call reads the token from the credential store (see
//! [`crate::transport`] for the order of a call and the failure mapping).

use std::fmt;
use std::sync::Arc;

use reqwest::header::ETAG;
use serde::Deserialize;

use crate::base_url::{BaseUrlError, GITHUB_DEFAULT_BASE};
use crate::clock::Clock;
use crate::credentials::{CredentialStore, Provider};
use crate::error::{Conditional, RemoteError};
use crate::rate_limit::Api;
use crate::transport::{encode_path, AdapterOptions, Core, Request};
use crate::{BoxFut, RemoteResult};

/// One repository, `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRef {
    owner: String,
    name: String,
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl RepoRef {
    /// `owner` and `name` must each be 1 to 100 of letters, digits, `-`,
    /// `_` and `.`, and not `.` or `..`: nothing that could change the
    /// request path.
    pub fn new(owner: &str, name: &str) -> RemoteResult<RepoRef> {
        if valid_segment(owner) && valid_segment(name) {
            Ok(RepoRef {
                owner: owner.to_string(),
                name: name.to_string(),
            })
        } else {
            Err(RemoteError::InvalidInput("repository name"))
        }
    }

    /// Parses `owner/name`.
    pub fn parse(text: &str) -> RemoteResult<RepoRef> {
        match text.split_once('/') {
            Some((owner, name)) => RepoRef::new(owner, name),
            None => Err(RemoteError::InvalidInput("repository name")),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    fn path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Who the token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubUser {
    pub login: String,
    pub id: u64,
}

/// A repo that exists and is readable with the token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoInfo {
    pub id: u64,
    pub full_name: String,
    pub default_branch: String,
    #[serde(default)]
    pub private: bool,
}

/// What a tree entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    /// A file.
    Blob,
    /// A directory.
    Tree,
    /// A submodule (a commit in another repo). Listed by name only.
    Commit,
}

/// One entry of a recursive tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub mode: String,
    pub kind: TreeEntryKind,
    pub sha: String,
    /// Blobs only.
    pub size: Option<u64>,
}

/// A repo's whole tree at one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub sha: String,
    pub entries: Vec<TreeEntry>,
    /// GitHub cut the listing (too many entries); the caller must not take
    /// it as complete.
    pub truncated: bool,
}

/// The commit a ref points at, and the ETag to poll it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefHead {
    pub sha: String,
    pub etag: Option<String>,
}

/// Read-side GitHub access for one repo per call. Each method sends the
/// stored token; see [`GithubClient`] for the failure mapping.
pub trait GithubAdapter: Send + Sync {
    /// Who the token is. A 401 is `AuthRejected`.
    fn whoami(&self) -> BoxFut<'_, RemoteResult<GithubUser>>;

    /// Whether the stored token is accepted: `Ok(())`, or the error
    /// (`AuthRejected` for a missing or rejected token).
    fn check_token(&self) -> BoxFut<'_, RemoteResult<()>> {
        Box::pin(async move { self.whoami().await.map(|_| ()) })
    }

    /// The repo, with its default branch. `NotFound` when it does not
    /// exist or the token cannot see it.
    fn get_repo<'a>(&'a self, repo: &'a RepoRef) -> BoxFut<'a, RemoteResult<RepoInfo>>;

    /// Every entry of the repo's tree at `git_ref` (a branch, tag or
    /// commit). One request: no blob is read.
    fn list_tree<'a>(
        &'a self,
        repo: &'a RepoRef,
        git_ref: &'a str,
    ) -> BoxFut<'a, RemoteResult<Tree>>;

    /// The bytes of the blob with this sha.
    fn get_blob<'a>(&'a self, repo: &'a RepoRef, sha: &'a str)
        -> BoxFut<'a, RemoteResult<Vec<u8>>>;

    /// The bytes of the file at `path` on `git_ref`.
    fn get_file<'a>(
        &'a self,
        repo: &'a RepoRef,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFut<'a, RemoteResult<Vec<u8>>>;

    /// The commit `git_ref` points at, as a conditional request: with the
    /// `etag` of an earlier answer, an unchanged ref is `NotModified`.
    fn poll_ref<'a>(
        &'a self,
        repo: &'a RepoRef,
        git_ref: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFut<'a, RemoteResult<Conditional<RefHead>>>;
}

/// The real GitHub adapter. Never sleeps, never follows a redirect, never
/// opens a listener.
pub struct GithubClient {
    core: Core,
}

impl fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubClient")
            .field("base_url", &self.core.base())
            .finish_non_exhaustive()
    }
}

const RAW: &str = "application/vnd.github.raw+json";
const JSON: &str = "application/vnd.github+json";
const SHA_ONLY: &str = "application/vnd.github.sha";

impl GithubClient {
    /// `base_url` is the `github_api_url` config value: `None` is
    /// `https://api.github.com`; anything else must have a loopback host.
    /// The token comes from `store` (provider `Github`) on every call.
    pub fn new(
        base_url: Option<&str>,
        store: Arc<dyn CredentialStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BaseUrlError> {
        Self::with_options(base_url, store, clock, AdapterOptions::default())
    }

    pub fn with_options(
        base_url: Option<&str>,
        store: Arc<dyn CredentialStore>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
    ) -> Result<Self, BaseUrlError> {
        Ok(GithubClient {
            core: Core::new(
                Api::Github,
                Provider::Github,
                base_url,
                GITHUB_DEFAULT_BASE,
                store,
                clock,
                options,
            )?,
        })
    }

    /// The base URL in use, normalised.
    pub fn base_url(&self) -> &str {
        self.core.base()
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        path: String,
        what: &'static str,
    ) -> RemoteResult<T> {
        let reply = self
            .core
            .get(Request {
                path,
                accept: Some(JSON),
                ..Request::default()
            })
            .await?;
        serde_json::from_slice(&reply.body).map_err(|_| RemoteError::BadResponse(what))
    }
}

/// A ref or a path with `/` kept and each segment encoded.
fn ref_path(git_ref: &str) -> RemoteResult<String> {
    encode_path(git_ref).map_err(|_| RemoteError::InvalidInput("ref"))
}

#[derive(Deserialize)]
struct TreeWire {
    sha: String,
    #[serde(default)]
    tree: Vec<TreeEntryWire>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntryWire {
    path: String,
    #[serde(default)]
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<u64>,
}

fn is_hex_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl GithubAdapter for GithubClient {
    fn whoami(&self) -> BoxFut<'_, RemoteResult<GithubUser>> {
        Box::pin(self.json("/user".to_string(), "user"))
    }

    fn get_repo<'a>(&'a self, repo: &'a RepoRef) -> BoxFut<'a, RemoteResult<RepoInfo>> {
        Box::pin(self.json(repo.path(), "repository"))
    }

    fn list_tree<'a>(
        &'a self,
        repo: &'a RepoRef,
        git_ref: &'a str,
    ) -> BoxFut<'a, RemoteResult<Tree>> {
        Box::pin(async move {
            let path = format!("{}/git/trees/{}", repo.path(), ref_path(git_ref)?);
            let reply = self
                .core
                .get(Request {
                    path,
                    query: vec![("recursive", "1".to_string())],
                    accept: Some(JSON),
                    ..Request::default()
                })
                .await?;
            let wire: TreeWire = serde_json::from_slice(&reply.body)
                .map_err(|_| RemoteError::BadResponse("tree"))?;
            let entries = wire
                .tree
                .into_iter()
                .map(|e| {
                    let kind = match e.kind.as_str() {
                        "blob" => TreeEntryKind::Blob,
                        "tree" => TreeEntryKind::Tree,
                        "commit" => TreeEntryKind::Commit,
                        _ => return Err(RemoteError::BadResponse("tree entry type")),
                    };
                    Ok(TreeEntry {
                        path: e.path,
                        mode: e.mode,
                        kind,
                        sha: e.sha,
                        size: e.size,
                    })
                })
                .collect::<RemoteResult<Vec<_>>>()?;
            Ok(Tree {
                sha: wire.sha,
                entries,
                truncated: wire.truncated,
            })
        })
    }

    fn get_blob<'a>(
        &'a self,
        repo: &'a RepoRef,
        sha: &'a str,
    ) -> BoxFut<'a, RemoteResult<Vec<u8>>> {
        Box::pin(async move {
            if !is_hex_sha(sha) {
                return Err(RemoteError::InvalidInput("blob sha"));
            }
            let reply = self
                .core
                .get(Request {
                    path: format!("{}/git/blobs/{sha}", repo.path()),
                    accept: Some(RAW),
                    ..Request::default()
                })
                .await?;
            Ok(reply.body)
        })
    }

    fn get_file<'a>(
        &'a self,
        repo: &'a RepoRef,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFut<'a, RemoteResult<Vec<u8>>> {
        Box::pin(async move {
            if git_ref.is_empty() || git_ref.chars().any(|c| c.is_control()) {
                return Err(RemoteError::InvalidInput("ref"));
            }
            let reply = self
                .core
                .get(Request {
                    path: format!("{}/contents/{}", repo.path(), encode_path(path)?),
                    query: vec![("ref", git_ref.to_string())],
                    accept: Some(RAW),
                    ..Request::default()
                })
                .await?;
            Ok(reply.body)
        })
    }

    fn poll_ref<'a>(
        &'a self,
        repo: &'a RepoRef,
        git_ref: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFut<'a, RemoteResult<Conditional<RefHead>>> {
        Box::pin(async move {
            let reply = self
                .core
                .get(Request {
                    path: format!("{}/commits/{}", repo.path(), ref_path(git_ref)?),
                    accept: Some(SHA_ONLY),
                    if_none_match: etag.map(str::to_string),
                    ..Request::default()
                })
                .await?;
            if reply.status == 304 {
                return Ok(Conditional::NotModified);
            }
            let sha = String::from_utf8(reply.body)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| is_hex_sha(s))
                .ok_or(RemoteError::BadResponse("commit sha"))?;
            let etag = reply
                .headers
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            Ok(Conditional::Modified(RefHead { sha, etag }))
        })
    }
}
