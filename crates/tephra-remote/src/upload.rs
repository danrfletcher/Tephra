//! The one write Tephra makes to Google Drive (atlas-27 PR-11): the bytes of
//! a file of the content cache become new content of a Drive file that
//! exists, which Drive records as a new revision.
//!
//! It is its own module on purpose. The read transport (`transport.rs`)
//! builds GETs and nothing else, and `tests/fence_adapter_surface.rs` keeps
//! it so; the read traits name nothing that writes. This module builds the
//! one write request and hands it to the shared path
//! ([`Core::send_built`]) so it gets the same token read, E2 latch, E1 gate,
//! no-redirect rule and error mapping as every read. It can only produce
//! `PATCH /upload/drive/v3/files/{id}?uploadType=media`: nothing that
//! creates, deletes, trashes, moves or renames, and no way to name another
//! route.

use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};

use crate::error::RemoteError;
use crate::transport::{encode_component, Core};

/// An upload is read from disk as it goes out, so its total time is bounded
/// by this and not by the whole-answer timeout.
const UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The bytes of a media upload: a file of the content cache, read piece by
/// piece as they are sent, so no upload holds a file whole in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadBody {
    pub path: PathBuf,
    /// The `Content-Type` of the media: the Drive file's own type, so an
    /// upload never changes it.
    pub content_type: String,
}

/// Sends `body` as the new content of the Drive file `id` and returns the
/// answer's body (the file's metadata, with the `fields` asked for).
pub(crate) async fn send_media(
    core: &Core,
    id: &str,
    fields: &str,
    body: UploadBody,
) -> Result<Vec<u8>, RemoteError> {
    let content_type = HeaderValue::from_str(&body.content_type)
        .map_err(|_| RemoteError::InvalidInput("content type"))?;
    let file = tokio::fs::File::open(&body.path).await.map_err(|e| {
        tracing::debug!(kind = ?e.kind(), "the file to upload could not be opened");
        RemoteError::InvalidInput("file to upload")
    })?;
    let id = id.to_string();
    let fields = fields.to_string();
    let reply = core
        .send_built(move |client, base, bearer: HeaderValue| {
            let root = base
                .strip_suffix("/drive/v3")
                .ok_or(RemoteError::InvalidInput("upload base url"))?;
            let url = format!(
                "{root}/upload/drive/v3/files/{id}?uploadType=media&fields={}",
                encode_component(&fields)
            );
            Ok(client
                .patch(url)
                .header(AUTHORIZATION, bearer)
                .header(CONTENT_TYPE, content_type)
                .body(reqwest::Body::from(file))
                .timeout(UPLOAD_TOTAL_TIMEOUT))
        })
        .await?;
    Ok(reply.body)
}
