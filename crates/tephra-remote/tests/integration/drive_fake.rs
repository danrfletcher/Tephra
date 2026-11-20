//! The real `DriveClient` against the fake Drive: every trait method,
//! paging, the changes feed, E1 and E2 end to end on a fake clock, a
//! redirect that is not followed, a cut body, and awkward names.

use std::time::Duration;

use tephra_remote::clock::Clock;
use tephra_remote::drive::{DriveAdapter, FOLDER_MIME, GOOGLE_DOC_MIME};
use tephra_remote::error::UnreachableCause;
use tephra_remote::testing::{DriveLimit, FakeDrive};
use tephra_remote::{health_for_error, RemoteError, SourceHealth};

use crate::common::{drive_env, DrvEnv};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[tokio::test]
async fn whoami_names_the_user() {
    let env = drive_env("ya29.x").await;
    env.fake.set_user_name("Dan F");
    let user = env.client.whoami().await.unwrap();
    assert_eq!(user.display_name, "Dan F");
    assert_eq!(user.permission_id.as_deref(), Some("perm-1"));
}

#[tokio::test]
async fn get_file_returns_id_name_mime_parents_revision_and_size() {
    let env = drive_env("ya29.x").await;
    let file = env.client.get_file("f1").await.unwrap();
    assert_eq!(file.id, "f1");
    assert_eq!(file.name, "notes.md");
    assert_eq!(file.mime_type, "text/markdown");
    assert_eq!(file.parents, ["fld"]);
    assert_eq!(file.size, Some(8));
    assert_eq!(
        file.head_revision_id.as_deref(),
        Some(env.fake.head_revision("f1").as_str())
    );
    assert!(!file.is_folder() && !file.is_native_doc());

    let folder = env.client.get_file("fld").await.unwrap();
    assert_eq!(folder.mime_type, FOLDER_MIME);
    assert!(folder.is_folder());
    assert_eq!(folder.size, None);
    assert_eq!(folder.head_revision_id, None);

    // A native Doc has no revision id and no size.
    let doc = env.client.get_file("d1").await.unwrap();
    assert_eq!(doc.mime_type, GOOGLE_DOC_MIME);
    assert!(doc.is_native_doc());
    assert_eq!((doc.size, doc.head_revision_id), (None, None));

    // A new revision shows up in headRevisionId.
    env.fake.update_file("f1", b"changed");
    let again = env.client.get_file("f1").await.unwrap();
    assert_ne!(again.head_revision_id, file.head_revision_id);
    assert_eq!(again.size, Some(7));

    assert_eq!(
        env.client.get_file("nope").await.unwrap_err(),
        RemoteError::NotFound
    );
}

#[tokio::test]
async fn list_children_lists_one_folder_and_pages_with_tokens() {
    let env = drive_env("ya29.x").await;
    for i in 0..4 {
        env.fake.add_file(
            &format!("x{i}"),
            &format!("file {i}.txt"),
            "text/plain",
            Some("fld"),
            b"x",
        );
    }
    env.fake.add_folder("other", "Other", None);
    env.fake
        .add_file("o1", "elsewhere.txt", "text/plain", Some("other"), b"x");

    // One page.
    let page = env.client.list_children("fld", None).await.unwrap();
    let mut ids: Vec<_> = page.files.iter().map(|f| f.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["d1", "f1", "x0", "x1", "x2", "x3"]);
    assert_eq!(page.next_page_token, None);

    // Pages of 4, 4 children + Doc + notes = 6: two pages.
    env.fake.set_page_size(4);
    let first = env.client.list_children("fld", None).await.unwrap();
    assert_eq!(first.files.len(), 4);
    let token = first.next_page_token.clone().expect("another page");
    let second = env.client.list_children("fld", Some(&token)).await.unwrap();
    assert_eq!(second.files.len(), 2);
    assert_eq!(second.next_page_token, None);
    let mut all: Vec<_> = first
        .files
        .iter()
        .chain(&second.files)
        .map(|f| f.id.clone())
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 6, "no file twice, none missing");

    // A bad token is the provider's 400.
    assert_eq!(
        env.client
            .list_children("fld", Some("garbage"))
            .await
            .unwrap_err(),
        RemoteError::Unexpected { status: 400 }
    );
    // Root and an empty folder.
    assert_eq!(
        env.client
            .list_children("root", None)
            .await
            .unwrap()
            .files
            .len(),
        2
    );
    env.fake.add_folder("empty", "Empty", Some("other"));
    let empty = env.client.list_children("empty", None).await.unwrap();
    assert!(empty.files.is_empty() && empty.next_page_token.is_none());
}

#[tokio::test]
async fn listing_a_folder_reaches_no_media_or_export_route() {
    let env = drive_env("ya29.x").await;
    env.client.list_children("fld", None).await.unwrap();
    env.client.get_file("f1").await.unwrap();
    let log = env.fake.request_log();
    assert!(log.matching("alt=media").is_empty(), "{:?}", log.all());
    assert!(log.matching("/export").is_empty());
    for r in log.all() {
        assert!(
            r.path == "/drive/v3/files" || r.path == "/drive/v3/files/f1",
            "{}",
            r.path
        );
    }
}

#[tokio::test]
async fn download_and_export() {
    let env = drive_env("ya29.x").await;
    assert_eq!(env.client.download("f1").await.unwrap(), b"# notes\n");
    assert_eq!(env.client.export_markdown("d1").await.unwrap(), b"# Plan\n");
    let log = env.fake.request_log();
    assert_eq!(log.matching("alt=media").len(), 1);
    assert_eq!(
        log.matching("/files/d1/export?mimeType=text%2Fmarkdown")
            .len(),
        1
    );

    // A native Doc has no bytes to download; a binary file cannot be exported.
    assert_eq!(
        env.client.download("d1").await.unwrap_err(),
        RemoteError::PermissionDenied
    );
    assert_eq!(
        env.client.export_markdown("f1").await.unwrap_err(),
        RemoteError::PermissionDenied
    );
    assert_eq!(
        env.client.download("nope").await.unwrap_err(),
        RemoteError::NotFound
    );

    env.fake
        .add_file("empty", "empty.txt", "text/plain", Some("fld"), b"");
    assert_eq!(env.client.download("empty").await.unwrap(), b"");
}

#[tokio::test]
async fn the_changes_feed_from_a_start_token() {
    let env = drive_env("ya29.x").await;
    let token = env.client.start_page_token().await.unwrap();

    // Nothing yet: an empty last page that names the next start token.
    let quiet = env.client.list_changes(&token).await.unwrap();
    assert!(quiet.changes.is_empty());
    assert_eq!(quiet.next_page_token, None);
    assert_eq!(quiet.new_start_page_token.as_deref(), Some(token.as_str()));

    env.fake.update_file("f1", b"v2");
    env.fake.rename_file("d1", "Roadmap");
    env.fake
        .add_file("n1", "new.md", "text/markdown", Some("fld"), b"n");
    env.fake.remove_file("n1");

    let page = env.client.list_changes(&token).await.unwrap();
    assert_eq!(page.next_page_token, None);
    let ids: Vec<_> = page
        .changes
        .iter()
        .map(|c| (c.file_id.as_str(), c.removed))
        .collect();
    assert_eq!(
        ids,
        [("f1", false), ("d1", false), ("n1", false), ("n1", true)]
    );
    // A rename is the same file id with a new name.
    let renamed = page.changes[1].file.as_ref().unwrap();
    assert_eq!(
        (renamed.id.as_str(), renamed.name.as_str()),
        ("d1", "Roadmap")
    );
    assert_eq!(
        page.changes[0]
            .file
            .as_ref()
            .unwrap()
            .head_revision_id
            .as_deref(),
        Some(env.fake.head_revision("f1").as_str())
    );
    assert!(page.changes[3].file.is_none());
    let next = page
        .new_start_page_token
        .expect("the last page names the next start token");

    // Polling again from there sees nothing new.
    assert!(env
        .client
        .list_changes(&next)
        .await
        .unwrap()
        .changes
        .is_empty());
    assert_eq!(
        env.client.list_changes("not-a-token").await.unwrap_err(),
        RemoteError::Unexpected { status: 400 }
    );
}

#[tokio::test]
async fn the_changes_feed_pages() {
    let env = drive_env("ya29.x").await;
    let token = env.client.start_page_token().await.unwrap();
    for i in 0..5 {
        env.fake.update_file("f1", format!("v{i}").as_bytes());
    }
    env.fake.set_page_size(2);

    let mut seen = 0;
    let mut pages = 0;
    let mut at = token;
    let final_token = loop {
        let page = env.client.list_changes(&at).await.unwrap();
        seen += page.changes.len();
        pages += 1;
        match (page.next_page_token, page.new_start_page_token) {
            (Some(next), None) => at = next,
            (None, Some(done)) => break done,
            other => panic!("a page is either next or last: {other:?}"),
        }
    };
    assert_eq!((seen, pages), (5, 3));
    env.fake.set_page_size(100);
    assert!(env
        .client
        .list_changes(&final_token)
        .await
        .unwrap()
        .changes
        .is_empty());
}

#[tokio::test]
async fn every_request_carries_the_stored_token() {
    let env = drive_env("ya29.abc").await;
    env.client.whoami().await.unwrap();
    env.client.get_file("f1").await.unwrap();
    env.client.download("f1").await.unwrap();
    for r in env.fake.request_log().all() {
        assert_eq!(r.method, "GET");
        assert_eq!(r.header("authorization"), Some("Bearer ya29.abc"));
        assert!(r.path.starts_with("/drive/v3/"), "{}", r.path);
    }
}

// ---- E1 ----------------------------------------------------------------------------

async fn expect_limited(env: &DrvEnv, retry_at: std::time::SystemTime) {
    assert_eq!(
        env.client.get_file("f1").await.unwrap_err(),
        RemoteError::RateLimited { retry_at }
    );
}

#[tokio::test]
async fn a_drive_limit_with_retry_after_is_honoured_in_every_style() {
    let styles = [
        DriveLimit::Status429 { retry_after: true },
        DriveLimit::Reason403 {
            reason: "rateLimitExceeded",
            retry_after: true,
        },
        DriveLimit::Reason403 {
            reason: "userRateLimitExceeded",
            retry_after: true,
        },
        DriveLimit::Reason403 {
            reason: "sharingRateLimitExceeded",
            retry_after: true,
        },
    ];
    for style in styles {
        let env = drive_env("ya29.x").await;
        let start = env.clock.now();
        env.fake.rate_limit_for(secs(30), style);

        expect_limited(&env, start + secs(30)).await;
        for _ in 0..3 {
            expect_limited(&env, start + secs(30)).await;
            assert_eq!(
                env.client.list_children("fld", None).await.unwrap_err(),
                RemoteError::RateLimited {
                    retry_at: start + secs(30)
                }
            );
        }
        assert_eq!(env.fake.request_log().count(), 1, "{style:?}");

        env.clock.advance(secs(29));
        expect_limited(&env, start + secs(30)).await;
        assert_eq!(env.fake.request_log().count(), 1);
        env.clock.advance(secs(1));
        env.client.get_file("f1").await.unwrap();
        assert_eq!(env.fake.request_log().count(), 2, "{style:?}");
    }
}

#[tokio::test]
async fn a_drive_limit_without_retry_after_doubles_from_one_second() {
    for style in [
        DriveLimit::Status429 { retry_after: false },
        DriveLimit::Reason403 {
            reason: "rateLimitExceeded",
            retry_after: false,
        },
    ] {
        let env = drive_env("ya29.x").await;
        env.fake.rate_limit_for(secs(3600), style);
        let mut waits = Vec::new();
        for _ in 0..5 {
            let before = env.clock.now();
            let RemoteError::RateLimited { retry_at } =
                env.client.get_file("f1").await.unwrap_err()
            else {
                panic!("expected a rate limit");
            };
            waits.push(retry_at.duration_since(before).unwrap().as_secs());
            env.clock.set(retry_at);
        }
        assert_eq!(waits, [1, 2, 4, 8, 16], "{style:?}");
        assert_eq!(env.fake.request_log().count(), 5);

        env.fake.clear_rate_limit();
        env.client.get_file("f1").await.unwrap();
        env.fake.rate_limit_for(secs(3600), style);
        let before = env.clock.now();
        let RemoteError::RateLimited { retry_at } = env.client.get_file("f1").await.unwrap_err()
        else {
            panic!("expected a rate limit");
        };
        assert_eq!(
            retry_at.duration_since(before).unwrap(),
            secs(1),
            "a success resets the doubling"
        );
    }
}

#[tokio::test]
async fn a_huge_retry_after_of_a_day_is_honoured() {
    let env = drive_env("ya29.x").await;
    let start = env.clock.now();
    env.fake
        .rate_limit_for(secs(86_400), DriveLimit::Status429 { retry_after: true });
    expect_limited(&env, start + secs(86_400)).await;
    env.clock.advance(secs(86_399));
    expect_limited(&env, start + secs(86_400)).await;
    assert_eq!(env.fake.request_log().count(), 1);
}

#[tokio::test]
async fn health_follows_the_error() {
    let env = drive_env("ya29.x").await;
    let start = env.clock.now();
    env.fake
        .rate_limit_for(secs(20), DriveLimit::Status429 { retry_after: true });
    let err = env.client.whoami().await.unwrap_err();
    assert_eq!(
        health_for_error(&err),
        Some(SourceHealth::Syncing {
            retry_at: start + secs(20)
        })
    );
    env.clock.advance(secs(20));
    env.fake.revoke_token();
    let err = env.client.whoami().await.unwrap_err();
    assert_eq!(health_for_error(&err), Some(SourceHealth::AuthRequired));
    // A plain permission failure is neither.
    let env = drive_env("ya29.x").await;
    env.fake.fail_next(1, 403);
    assert_eq!(
        health_for_error(&env.client.whoami().await.unwrap_err()),
        None
    );
}

// ---- E2 -----------------------------------------------------------------------------

#[tokio::test]
async fn a_revoked_drive_token_latches_and_recovers_when_the_store_changes() {
    let env = drive_env("ya29.old").await;
    env.fake.revoke_token();
    assert_eq!(
        env.client.get_file("f1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.list_changes("1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), 1);

    env.fake.accept_only_token("ya29.new");
    env.creds
        .set(tephra_remote::credentials::Provider::Google, "ya29.new");
    assert_eq!(env.client.get_file("f1").await.unwrap().id, "f1");
    assert_eq!(env.fake.request_log().count(), 2);
}

// ---- redirects, cut bodies, names ------------------------------------------------------

#[tokio::test]
async fn a_redirect_is_reported_and_not_followed() {
    let env = drive_env("ya29.secret").await;
    let elsewhere = FakeDrive::start().await;
    let target = format!("{}/files/f1", elsewhere.base_url());
    env.fake.redirect_next(&target);
    assert_eq!(
        env.client.get_file("f1").await.unwrap_err(),
        RemoteError::Moved {
            location: target.clone()
        }
    );
    assert_eq!(env.fake.request_log().count(), 1);
    assert_eq!(
        elsewhere.request_log().count(),
        0,
        "no Authorization header goes to the new location"
    );
    assert!(env.client.get_file("f1").await.is_ok());
}

#[tokio::test]
async fn a_cut_body_is_unreachable_not_a_short_file() {
    let env = drive_env("ya29.x").await;
    env.fake.add_file(
        "big",
        "big.bin",
        "application/octet-stream",
        Some("fld"),
        &vec![1u8; 64 * 1024],
    );
    env.fake.truncate_next_bodies(1);
    assert_eq!(
        env.client.download("big").await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::BodyCut)
    );
    env.fake.truncate_next_bodies(1);
    assert_eq!(
        env.client.list_children("fld", None).await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::BodyCut)
    );
    assert_eq!(env.client.download("big").await.unwrap().len(), 64 * 1024);
}

#[tokio::test]
async fn an_empty_body_where_json_is_expected_is_a_bad_response() {
    let env = drive_env("ya29.x").await;
    for _ in 0..2 {
        env.fake.empty_next_bodies(1);
        assert!(matches!(
            env.client.get_file("f1").await.unwrap_err(),
            RemoteError::BadResponse(_)
        ));
    }
    env.fake.empty_next_bodies(1);
    assert!(matches!(
        env.client.start_page_token().await.unwrap_err(),
        RemoteError::BadResponse(_)
    ));
    assert!(env.client.get_file("f1").await.is_ok());
}

#[tokio::test]
async fn unicode_percent_and_quote_characters_in_names_round_trip() {
    let env = drive_env("ya29.x").await;
    let names = [
        "Plan 100% ü #1.md",
        "日本語のメモ",
        "it's a \"quoted\" name & more",
        "50%25 off",
        "emoji 🙂.txt",
    ];
    for (i, name) in names.iter().enumerate() {
        env.fake
            .add_file(&format!("n{i}"), name, "text/plain", Some("fld"), b"x");
    }
    let listed = env.client.list_children("fld", None).await.unwrap();
    for (i, name) in names.iter().enumerate() {
        let f = listed
            .files
            .iter()
            .find(|f| f.id == format!("n{i}"))
            .unwrap();
        assert_eq!(f.name, *name);
        assert_eq!(
            env.client.get_file(&format!("n{i}")).await.unwrap().name,
            *name
        );
    }
}

#[tokio::test]
async fn ids_and_tokens_that_could_change_the_request_are_refused_before_anything_is_sent() {
    let env = drive_env("ya29.x").await;
    for bad in [
        "",
        "a b",
        "a/b",
        "../x",
        "a'b",
        "x' in parents or 'y",
        "a?b",
        "a%2fb",
        "é",
    ] {
        assert_eq!(
            env.client.get_file(bad).await.unwrap_err(),
            RemoteError::InvalidInput("file id"),
            "{bad:?}"
        );
        assert_eq!(
            env.client.list_children(bad, None).await.unwrap_err(),
            RemoteError::InvalidInput("file id"),
            "{bad:?}"
        );
        assert_eq!(
            env.client.download(bad).await.unwrap_err(),
            RemoteError::InvalidInput("file id"),
            "{bad:?}"
        );
        assert_eq!(
            env.client.export_markdown(bad).await.unwrap_err(),
            RemoteError::InvalidInput("file id"),
            "{bad:?}"
        );
    }
    assert_eq!(
        env.client.list_children("fld", Some("")).await.unwrap_err(),
        RemoteError::InvalidInput("page token")
    );
    assert_eq!(
        env.client.list_changes("a\nb").await.unwrap_err(),
        RemoteError::InvalidInput("page token")
    );
    assert_eq!(env.fake.request_log().count(), 0);
}
