//! Google Drive, read side: the [`DriveAdapter`] trait and
//! [`DriveClient`], its HTTP implementation.
//!
//! The trait lists the children of one named folder and follows the
//! changes feed; it has no method that lists the Drives or all the files
//! of an account, and none that writes. The secret read from the
//! credential store for provider `Google` is sent as the bearer token as
//! it is; turning a stored refresh token into an access token belongs to
//! the sign-in work (PR-4), not here.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use crate::base_url::{BaseUrlError, DRIVE_DEFAULT_BASE};
use crate::clock::Clock;
use crate::credentials::{CredentialStore, Provider};
use crate::error::RemoteError;
use crate::rate_limit::Api;
use crate::transport::{AdapterOptions, BodyStream, Core, Request};
use crate::{BoxFut, RemoteResult};

/// The `mimeType` of a native Google Doc.
pub const GOOGLE_DOC_MIME: &str = "application/vnd.google-apps.document";
/// The `mimeType` of a Drive folder.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// What a native Doc is exported as.
pub const MARKDOWN_MIME: &str = "text/markdown";

const FILE_FIELDS: &str = "id,name,mimeType,parents,headRevisionId,size";

/// Who the token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveUser {
    #[serde(default)]
    pub display_name: String,
    pub email_address: Option<String>,
    pub permission_id: Option<String>,
}

fn size_from_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    // Drive sends `size` as a decimal string.
    let text: Option<String> = Option::deserialize(d)?;
    match text {
        None => Ok(None),
        Some(t) => t.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
    }
}

/// One file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    /// Absent for native Google files and folders.
    pub head_revision_id: Option<String>,
    #[serde(default, deserialize_with = "size_from_text")]
    pub size: Option<u64>,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    pub fn is_native_doc(&self) -> bool {
        self.mime_type == GOOGLE_DOC_MIME
    }
}

/// One page of a folder's children.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePage {
    #[serde(default)]
    pub files: Vec<DriveFile>,
    /// Pass it back to get the next page; `None` on the last one.
    pub next_page_token: Option<String>,
}

/// One entry of the changes feed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub time: Option<String>,
    /// The file as it is now; absent when `removed`.
    pub file: Option<DriveFile>,
}

/// One page of the changes feed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesPage {
    #[serde(default)]
    pub changes: Vec<Change>,
    /// More pages: pass this to the next call.
    pub next_page_token: Option<String>,
    /// The last page: store this and use it for the next poll.
    pub new_start_page_token: Option<String>,
}

#[derive(Deserialize)]
struct AboutWire {
    user: DriveUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartTokenWire {
    start_page_token: String,
}

/// Read-side Drive access, one folder per call.
pub trait DriveAdapter: Send + Sync {
    /// Who the token is. A 401 is `AuthRejected`.
    fn whoami(&self) -> BoxFut<'_, RemoteResult<DriveUser>>;

    /// Metadata of one file or folder.
    fn get_file<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<DriveFile>>;

    /// One page of the direct children of `folder_id`. Metadata only: no
    /// file content is read.
    fn list_children<'a>(
        &'a self,
        folder_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFut<'a, RemoteResult<FilePage>>;

    /// The bytes of a binary or text file.
    fn download<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<Vec<u8>>>;

    /// The bytes of a binary or text file as a body read piece by piece:
    /// a file of any size is written to disk without being held whole.
    /// The same request, and the same failures, as [`DriveAdapter::download`].
    fn stream_download<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<BodyStream>> {
        Box::pin(async move { Ok(BodyStream::from_bytes(self.download(file_id).await?)) })
    }

    /// A native Google Doc exported as `text/markdown`.
    fn export_markdown<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<Vec<u8>>>;

    /// The token to start following the changes feed from now.
    fn start_page_token(&self) -> BoxFut<'_, RemoteResult<String>>;

    /// The changes since `page_token`: follow `next_page_token` until
    /// `new_start_page_token` appears.
    fn list_changes<'a>(&'a self, page_token: &'a str) -> BoxFut<'a, RemoteResult<ChangesPage>>;
}

/// The real Drive adapter. Never sleeps, never follows a redirect, never
/// opens a listener.
pub struct DriveClient {
    core: Core,
}

impl fmt::Debug for DriveClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriveClient")
            .field("base_url", &self.core.base())
            .finish_non_exhaustive()
    }
}

/// A Drive id is letters, digits, `-` and `_`: it goes into a path and
/// into a query expression, so nothing else is let through.
fn check_id(id: &str) -> RemoteResult<&str> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(id)
    } else {
        Err(RemoteError::InvalidInput("file id"))
    }
}

fn check_token_text(token: &str) -> RemoteResult<()> {
    if token.is_empty() || token.len() > 2048 || token.chars().any(|c| c.is_control()) {
        Err(RemoteError::InvalidInput("page token"))
    } else {
        Ok(())
    }
}

impl DriveClient {
    /// `base_url` is the `drive_api_url` config value: `None` is
    /// `https://www.googleapis.com/drive/v3`; anything else must have a
    /// loopback host. The token comes from `store` (provider `Google`) on
    /// every call.
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
        Ok(DriveClient {
            core: Core::new(
                Api::Drive,
                Provider::Google,
                base_url,
                DRIVE_DEFAULT_BASE,
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
        request: Request,
        what: &'static str,
    ) -> RemoteResult<T> {
        let reply = self.core.get(request).await?;
        serde_json::from_slice(&reply.body).map_err(|_| RemoteError::BadResponse(what))
    }
}

impl DriveAdapter for DriveClient {
    fn whoami(&self) -> BoxFut<'_, RemoteResult<DriveUser>> {
        Box::pin(async move {
            let about: AboutWire = self
                .json(
                    Request {
                        path: "/about".to_string(),
                        query: vec![(
                            "fields",
                            "user(displayName,emailAddress,permissionId)".to_string(),
                        )],
                        ..Request::default()
                    },
                    "about",
                )
                .await?;
            Ok(about.user)
        })
    }

    fn get_file<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<DriveFile>> {
        Box::pin(async move {
            let id = check_id(file_id)?;
            self.json(
                Request {
                    path: format!("/files/{id}"),
                    query: vec![("fields", FILE_FIELDS.to_string())],
                    ..Request::default()
                },
                "file",
            )
            .await
        })
    }

    fn list_children<'a>(
        &'a self,
        folder_id: &'a str,
        page_token: Option<&'a str>,
    ) -> BoxFut<'a, RemoteResult<FilePage>> {
        Box::pin(async move {
            let id = check_id(folder_id)?;
            let mut query = vec![
                ("q", format!("'{id}' in parents and trashed = false")),
                ("fields", format!("nextPageToken,files({FILE_FIELDS})")),
                ("pageSize", "100".to_string()),
            ];
            if let Some(token) = page_token {
                check_token_text(token)?;
                query.push(("pageToken", token.to_string()));
            }
            self.json(
                Request {
                    path: "/files".to_string(),
                    query,
                    ..Request::default()
                },
                "file list",
            )
            .await
        })
    }

    fn download<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<Vec<u8>>> {
        Box::pin(async move {
            let id = check_id(file_id)?;
            let reply = self
                .core
                .get(Request {
                    path: format!("/files/{id}"),
                    query: vec![("alt", "media".to_string())],
                    ..Request::default()
                })
                .await?;
            Ok(reply.body)
        })
    }

    fn stream_download<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<BodyStream>> {
        Box::pin(async move {
            let id = check_id(file_id)?;
            self.core
                .stream(Request {
                    path: format!("/files/{id}"),
                    query: vec![("alt", "media".to_string())],
                    ..Request::default()
                })
                .await
        })
    }

    fn export_markdown<'a>(&'a self, file_id: &'a str) -> BoxFut<'a, RemoteResult<Vec<u8>>> {
        Box::pin(async move {
            let id = check_id(file_id)?;
            let reply = self
                .core
                .get(Request {
                    path: format!("/files/{id}/export"),
                    query: vec![("mimeType", MARKDOWN_MIME.to_string())],
                    ..Request::default()
                })
                .await?;
            Ok(reply.body)
        })
    }

    fn start_page_token(&self) -> BoxFut<'_, RemoteResult<String>> {
        Box::pin(async move {
            let wire: StartTokenWire = self
                .json(
                    Request {
                        path: "/changes/startPageToken".to_string(),
                        ..Request::default()
                    },
                    "start page token",
                )
                .await?;
            Ok(wire.start_page_token)
        })
    }

    fn list_changes<'a>(&'a self, page_token: &'a str) -> BoxFut<'a, RemoteResult<ChangesPage>> {
        Box::pin(async move {
            check_token_text(page_token)?;
            self.json(
                Request {
                    path: "/changes".to_string(),
                    query: vec![
                        ("pageToken", page_token.to_string()),
                        ("pageSize", "100".to_string()),
                        (
                            "fields",
                            format!(
                                "nextPageToken,newStartPageToken,changes(fileId,removed,time,file({FILE_FIELDS}))"
                            ),
                        ),
                    ],
                    ..Request::default()
                },
                "changes",
            )
            .await
        })
    }
}
