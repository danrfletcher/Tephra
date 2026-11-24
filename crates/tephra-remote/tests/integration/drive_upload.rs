//! The one Drive write (PR-11): `DriveUploader::replace_media` against the
//! fake. New content makes a new head revision; every outcome maps through
//! the shared transport (E1 gate, E2 latch, 5xx, no redirect); the token
//! never appears in what is logged or in an error.

use std::time::Duration;

use tephra_remote::drive::{DriveAdapter, DriveClient, DriveUploader, Md5Hasher, UploadBody};
use tephra_remote::testing::{DriveLimit, UploadHold};
use tephra_remote::RemoteError;

use crate::common::drive_env;

fn body(dir: &tempfile::TempDir, bytes: &[u8]) -> UploadBody {
    let path = dir.path().join("edit.md");
    std::fs::write(&path, bytes).unwrap();
    UploadBody {
        path,
        content_type: "text/markdown".to_string(),
    }
}

fn md5_of(bytes: &[u8]) -> String {
    let mut h = Md5Hasher::new();
    h.update(bytes);
    h.finish()
}

#[test]
fn the_md5_is_the_one_drive_reports() {
    // RFC 1321 test vectors.
    assert_eq!(md5_of(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_of(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
}

#[tokio::test]
async fn an_upload_makes_a_new_revision_with_the_bytes_and_reports_it() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let before = env.client.get_file("f1").await.unwrap();
    let sent = body(&dir, b"# notes\nedited\n");

    let after = env.client.replace_media("f1", sent).await.unwrap();

    assert_ne!(after.revision(), before.revision(), "a new revision");
    assert_eq!(
        after.revision(),
        Some(env.fake.head_revision("f1").as_str())
    );
    assert_eq!(after.md5_checksum, Some(md5_of(b"# notes\nedited\n")));
    assert_eq!(after.size, Some(15));
    assert_eq!(env.fake.content("f1"), b"# notes\nedited\n");
    assert_eq!(env.fake.upload_count("f1"), 1);
    // Read back through the read side: the same head.
    let read = env.client.get_file("f1").await.unwrap();
    assert_eq!(read.revision(), after.revision());
    assert_eq!(read.name, "notes.md", "the name is untouched");
    assert_eq!(read.parents, ["fld"], "so is the place");
}

#[tokio::test]
async fn the_request_is_one_media_patch_with_the_files_own_type_and_a_sensitive_bearer() {
    let env = drive_env("ya29.secret-token").await;
    let dir = tempfile::tempdir().unwrap();
    env.fake.request_log().clear();
    env.client
        .replace_media("f1", body(&dir, b"x"))
        .await
        .unwrap();
    let log = env.fake.request_log().all();
    assert_eq!(log.len(), 1, "one request, no side calls");
    let req = &log[0];
    assert_eq!(req.method, "PATCH");
    assert_eq!(req.path, "/upload/drive/v3/files/f1");
    let query = req.query.clone().unwrap();
    assert!(query.starts_with("uploadType=media&fields="), "{query}");
    assert_eq!(req.header("content-type"), Some("text/markdown"));
    assert_eq!(
        req.header("authorization"),
        Some("Bearer ya29.secret-token")
    );
}

#[tokio::test]
async fn an_empty_file_uploads_as_empty_content() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let after = env
        .client
        .replace_media("f1", body(&dir, b""))
        .await
        .unwrap();
    assert_eq!(after.size, Some(0));
    assert_eq!(env.fake.content("f1"), b"");
}

#[tokio::test]
async fn a_large_file_goes_out_whole() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let after = env
        .client
        .replace_media("f1", body(&dir, &bytes))
        .await
        .unwrap();
    assert_eq!(after.md5_checksum, Some(md5_of(&bytes)));
    assert_eq!(env.fake.content("f1"), bytes);
}

#[tokio::test]
async fn failures_map_the_way_reads_do_and_apply_nothing() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let up = |bytes: &'static [u8]| env.client.replace_media("f1", body(&dir, bytes));

    env.fake.fail_next(1, 503);
    assert_eq!(
        up(b"a").await.unwrap_err(),
        RemoteError::Server { status: 503 }
    );
    env.fake.fail_next(1, 404);
    assert_eq!(up(b"a").await.unwrap_err(), RemoteError::NotFound);
    env.fake.fail_next(1, 403);
    assert_eq!(up(b"a").await.unwrap_err(), RemoteError::PermissionDenied);
    env.fake.fail_next(1, 400);
    assert_eq!(
        up(b"a").await.unwrap_err(),
        RemoteError::Unexpected { status: 400 }
    );
    assert_eq!(env.fake.upload_count("f1"), 0, "nothing was applied");
    assert_eq!(env.fake.content("f1"), b"# notes\n");

    // Unreadable id: nothing is sent.
    env.fake.request_log().clear();
    let err = env
        .client
        .replace_media("../files/f1", body(&dir, b"a"))
        .await
        .unwrap_err();
    assert!(matches!(err, RemoteError::InvalidInput(_)));
    assert_eq!(env.fake.request_log().count(), 0);
    // A file that is not there to read: nothing is sent.
    let missing = UploadBody {
        path: dir.path().join("gone.md"),
        content_type: "text/markdown".into(),
    };
    assert!(matches!(
        env.client.replace_media("f1", missing).await,
        Err(RemoteError::InvalidInput(_))
    ));
    assert_eq!(env.fake.request_log().count(), 0);
}

#[tokio::test]
async fn a_native_doc_cannot_be_replaced() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let err = env
        .client
        .replace_media("d1", body(&dir, b"x"))
        .await
        .unwrap_err();
    assert_eq!(err, RemoteError::PermissionDenied);
}

#[tokio::test]
async fn a_rate_limit_gates_uploads_locally_and_a_rejected_token_latches() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    env.fake.rate_limit_for(
        Duration::from_secs(120),
        DriveLimit::Status429 { retry_after: true },
    );
    let first = env.client.replace_media("f1", body(&dir, b"a")).await;
    let Err(RemoteError::RateLimited { retry_at }) = first else {
        panic!("rate limited, got {first:?}");
    };
    let seen = env.fake.request_log().count();
    let second = env.client.replace_media("f1", body(&dir, b"a")).await;
    assert_eq!(second, Err(RemoteError::RateLimited { retry_at }));
    assert_eq!(
        env.fake.request_log().count(),
        seen,
        "the gate sent nothing"
    );
    env.clock.advance(Duration::from_secs(121));
    env.fake.clear_rate_limit();
    env.client
        .replace_media("f1", body(&dir, b"a"))
        .await
        .unwrap();

    env.fake.revoke_token();
    assert_eq!(
        env.client.replace_media("f1", body(&dir, b"b")).await,
        Err(RemoteError::AuthRejected)
    );
    let seen = env.fake.request_log().count();
    assert_eq!(
        env.client.replace_media("f1", body(&dir, b"b")).await,
        Err(RemoteError::AuthRejected)
    );
    assert_eq!(
        env.fake.request_log().count(),
        seen,
        "the latch sent nothing"
    );
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_token_goes_nowhere_else() {
    let env = drive_env("ya29.secret-token").await;
    let dir = tempfile::tempdir().unwrap();
    env.fake.redirect_next("http://127.0.0.1:1/elsewhere");
    let err = env
        .client
        .replace_media("f1", body(&dir, b"a"))
        .await
        .unwrap_err();
    assert!(matches!(err, RemoteError::Moved { .. }));
    assert!(!format!("{err:?} {err}").contains("secret-token"));
    assert_eq!(env.fake.upload_count("f1"), 0);
}

#[tokio::test]
async fn an_answer_cut_after_the_bytes_landed_is_an_error_but_the_revision_exists() {
    // The case the daemon's revision check exists for (E14).
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    let before = env.client.get_file("f1").await.unwrap();
    env.fake.truncate_next_bodies(1);
    let err = env
        .client
        .replace_media("f1", body(&dir, b"landed"))
        .await
        .unwrap_err();
    assert!(matches!(err, RemoteError::Unreachable(_)), "{err:?}");
    assert_eq!(env.fake.upload_count("f1"), 1, "the bytes did land");
    let head = env.client.get_file("f1").await.unwrap();
    assert_ne!(head.revision(), before.revision());
    assert_eq!(head.md5_checksum, Some(md5_of(b"landed")));
}

#[tokio::test]
async fn the_fake_can_hold_an_upload_before_or_after_it_applies() {
    let env = drive_env("ya29.x").await;
    let dir = tempfile::tempdir().unwrap();
    env.fake.hold_uploads(UploadHold::BeforeApply);
    let sent = body(&dir, b"held");
    let client = std::sync::Arc::new(
        DriveClient::new(
            Some(env.fake.base_url()),
            env.creds.store.clone(),
            env.clock.clone(),
        )
        .unwrap(),
    );
    let task = tokio::spawn(async move { client.replace_media("f1", sent).await });
    for _ in 0..200 {
        if env.fake.uploads_held() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.fake.uploads_held(), 1);
    assert_eq!(env.fake.upload_count("f1"), 0, "not applied while held");
    env.fake.release_uploads();
    task.await.unwrap().unwrap();
    assert_eq!(env.fake.upload_count("f1"), 1);
}
