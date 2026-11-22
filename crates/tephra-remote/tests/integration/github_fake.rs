//! The real `GithubClient` against the fake GitHub: every trait method,
//! conditional polling, E1 and E2 end to end on a fake clock, the redirect
//! rule (E13), a cut body, and awkward names.

use std::time::Duration;

use tephra_remote::clock::Clock;
use tephra_remote::error::UnreachableCause;
use tephra_remote::github::{GithubAdapter, RepoRef, TreeEntryKind};
use tephra_remote::testing::{FakeGithub, GithubLimit};
use tephra_remote::{health_for_error, Conditional, RemoteError, SourceHealth};

use crate::common::{github_env, GhEnv};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---- the trait, method by method --------------------------------------------

#[tokio::test]
async fn whoami_and_check_token() {
    let env = github_env("ghp_x").await;
    env.fake.set_login("dan");
    let user = env.client.whoami().await.unwrap();
    assert_eq!(user.login, "dan");
    env.client.check_token().await.unwrap();
}

#[tokio::test]
async fn get_repo_reports_existence_and_default_branch() {
    let env = github_env("ghp_x").await;
    env.fake.add_repo("octo/trunk", "trunk");
    let info = env.client.get_repo(&env.repo).await.unwrap();
    assert_eq!(info.full_name, "octo/hello");
    assert_eq!(info.default_branch, "main");
    assert!(!info.private);

    let trunk = RepoRef::parse("octo/trunk").unwrap();
    assert_eq!(
        env.client.get_repo(&trunk).await.unwrap().default_branch,
        "trunk"
    );

    let missing = RepoRef::parse("octo/nope").unwrap();
    assert_eq!(
        env.client.get_repo(&missing).await.unwrap_err(),
        RemoteError::NotFound
    );
}

#[tokio::test]
async fn list_tree_lists_files_and_directories_with_sizes_and_shas() {
    let env = github_env("ghp_x").await;
    let tree = env.client.list_tree(&env.repo, "main").await.unwrap();
    assert!(!tree.truncated);
    let paths: Vec<&str> = tree.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["README.md", "docs", "docs/setup.md"]);

    let readme = &tree.entries[0];
    assert_eq!(readme.kind, TreeEntryKind::Blob);
    assert_eq!(readme.size, Some(8));
    assert_eq!(readme.sha, FakeGithub::blob_sha(b"# hello\n"));
    assert_eq!(tree.entries[1].kind, TreeEntryKind::Tree);
    assert_eq!(tree.entries[1].size, None);

    // By branch, by HEAD and by commit sha all name the same tree.
    let head = env.fake.head_sha("octo/hello", "main");
    for git_ref in ["HEAD", head.as_str()] {
        assert_eq!(
            env.client.list_tree(&env.repo, git_ref).await.unwrap(),
            tree,
            "{git_ref}"
        );
    }
    assert_eq!(
        env.client
            .list_tree(&env.repo, "no-such-branch")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
}

#[tokio::test]
async fn listing_a_tree_reaches_no_blob_or_contents_route() {
    let env = github_env("ghp_x").await;
    env.client.list_tree(&env.repo, "main").await.unwrap();
    let log = env.fake.request_log().all();
    assert_eq!(log.len(), 1);
    assert!(log[0].path.contains("/git/trees/"), "{log:?}");
    assert!(env.fake.request_log().matching("/git/blobs/").is_empty());
    assert!(env.fake.request_log().matching("/contents/").is_empty());
}

#[tokio::test]
async fn get_blob_and_get_file_return_the_bytes() {
    let env = github_env("ghp_x").await;
    let tree = env.client.list_tree(&env.repo, "main").await.unwrap();
    let setup = tree
        .entries
        .iter()
        .find(|e| e.path == "docs/setup.md")
        .unwrap();

    assert_eq!(
        env.client.get_blob(&env.repo, &setup.sha).await.unwrap(),
        b"setup\n"
    );
    assert_eq!(
        env.client
            .get_file(&env.repo, "docs/setup.md", "main")
            .await
            .unwrap(),
        b"setup\n"
    );
    assert_eq!(
        env.client
            .get_file(&env.repo, "README.md", "main")
            .await
            .unwrap(),
        b"# hello\n"
    );
    assert_eq!(
        env.client
            .get_file(&env.repo, "missing.md", "main")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    assert_eq!(
        env.client
            .get_blob(&env.repo, "0123456789abcdef")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
}

#[tokio::test]
async fn get_file_reads_the_ref_it_is_given() {
    let env = github_env("ghp_x").await;
    env.fake
        .put_file("octo/hello", "feature/ü x", "README.md", b"on the branch\n");
    assert_eq!(
        env.client
            .get_file(&env.repo, "README.md", "feature/ü x")
            .await
            .unwrap(),
        b"on the branch\n"
    );
    assert_eq!(
        env.client
            .get_file(&env.repo, "README.md", "main")
            .await
            .unwrap(),
        b"# hello\n"
    );
}

#[tokio::test]
async fn an_empty_file_is_empty_bytes_not_an_error() {
    let env = github_env("ghp_x").await;
    env.fake.put_file("octo/hello", "main", "empty.txt", b"");
    assert_eq!(
        env.client
            .get_file(&env.repo, "empty.txt", "main")
            .await
            .unwrap(),
        b""
    );
    let sha = FakeGithub::blob_sha(b"");
    assert_eq!(env.client.get_blob(&env.repo, &sha).await.unwrap(), b"");
}

#[tokio::test]
async fn an_empty_body_where_json_is_expected_is_a_bad_response() {
    let env = github_env("ghp_x").await;
    env.fake.empty_next_bodies(1);
    assert!(matches!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::BadResponse(_)
    ));
    // A bad body is not a verdict on the token or the limit.
    assert!(env.client.whoami().await.is_ok());
}

// ---- 304 ---------------------------------------------------------------------

#[tokio::test]
async fn poll_ref_is_a_conditional_request_and_not_modified_is_a_result() {
    let env = github_env("ghp_x").await;
    let head = env.fake.head_sha("octo/hello", "main");

    let first = env.client.poll_ref(&env.repo, "main", None).await.unwrap();
    let Conditional::Modified(first) = first else {
        panic!("a poll with no ETag is a full answer");
    };
    assert_eq!(first.sha, head);
    let etag = first.etag.expect("the fake sends an ETag");

    let again = env
        .client
        .poll_ref(&env.repo, "main", Some(&etag))
        .await
        .unwrap();
    assert_eq!(again, Conditional::NotModified);
    let sent = env.fake.request_log().last().unwrap();
    assert_eq!(sent.header("if-none-match"), Some(etag.as_str()));

    // A new commit changes the answer.
    let new_head = env.fake.put_file("octo/hello", "main", "NEW.md", b"x");
    let after = env
        .client
        .poll_ref(&env.repo, "main", Some(&etag))
        .await
        .unwrap();
    let Conditional::Modified(after) = after else {
        panic!("the ref moved");
    };
    assert_eq!(after.sha, new_head);
    assert_ne!(after.etag.as_deref(), Some(etag.as_str()));

    // A stale or foreign ETag is just a full answer.
    let foreign = env
        .client
        .poll_ref(&env.repo, "main", Some("\"nope\""))
        .await
        .unwrap();
    assert!(matches!(foreign, Conditional::Modified(_)));
}

// ---- headers the adapter sends -------------------------------------------------

#[tokio::test]
async fn every_request_carries_the_stored_token_and_github_headers() {
    let env = github_env("ghp_abc123").await;
    env.client.whoami().await.unwrap();
    env.client
        .get_file(&env.repo, "README.md", "main")
        .await
        .unwrap();
    for r in env.fake.request_log().all() {
        assert_eq!(r.method, "GET");
        assert_eq!(r.header("authorization"), Some("Bearer ghp_abc123"));
        assert!(r
            .header("user-agent")
            .unwrap()
            .starts_with("tephra-remote/"));
        assert_eq!(r.header("x-github-api-version"), Some("2022-11-28"));
    }
    assert_eq!(
        env.fake.request_log().last().unwrap().header("accept"),
        Some("application/vnd.github.raw+json")
    );
}

// ---- E1: rate limits, end to end -------------------------------------------------

async fn expect_limited(env: &GhEnv, retry_at: std::time::SystemTime) {
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::RateLimited { retry_at }
    );
}

#[tokio::test]
async fn a_retry_after_limit_is_honoured_without_sending_until_it_passes() {
    for style in [GithubLimit::RetryAfter403, GithubLimit::RetryAfter429] {
        let env = github_env("ghp_x").await;
        let start = env.clock.now();
        env.fake.rate_limit_for(secs(45), style);

        expect_limited(&env, start + secs(45)).await;
        assert_eq!(env.fake.request_log().count(), 1);

        // Further calls of every kind answer locally, with the same time.
        for _ in 0..3 {
            expect_limited(&env, start + secs(45)).await;
            assert_eq!(
                env.client.get_repo(&env.repo).await.unwrap_err(),
                RemoteError::RateLimited {
                    retry_at: start + secs(45)
                }
            );
            assert_eq!(
                env.client.list_tree(&env.repo, "main").await.unwrap_err(),
                RemoteError::RateLimited {
                    retry_at: start + secs(45)
                }
            );
        }
        assert_eq!(
            env.fake.request_log().count(),
            1,
            "{style:?}: nothing sent while limited"
        );

        env.clock.advance(secs(44));
        expect_limited(&env, start + secs(45)).await;
        assert_eq!(env.fake.request_log().count(), 1);

        // The first call at retry_at goes out, and the limit is over.
        env.clock.advance(secs(1));
        env.client.whoami().await.unwrap();
        assert_eq!(env.fake.request_log().count(), 2, "{style:?}");
        env.client.whoami().await.unwrap();
    }
}

#[tokio::test]
async fn a_primary_limit_with_a_reset_time_retries_at_the_reset() {
    let env = github_env("ghp_x").await;
    let start = env.clock.now();
    env.fake.rate_limit_for(secs(120), GithubLimit::Reset403);

    expect_limited(&env, start + secs(120)).await;
    let log = env.fake.request_log();
    assert_eq!(log.count(), 1);

    expect_limited(&env, start + secs(120)).await;
    assert_eq!(log.count(), 1);
    env.clock.advance(secs(120));
    env.client.whoami().await.unwrap();
    assert_eq!(log.count(), 2);
}

#[tokio::test]
async fn a_secondary_limit_with_no_headers_backs_off_1_2_4_seconds_then_resets_on_success() {
    let env = github_env("ghp_x").await;
    env.fake
        .rate_limit_for(secs(3600), GithubLimit::Secondary403);

    let mut waits = Vec::new();
    for _ in 0..4 {
        let before = env.clock.now();
        let RemoteError::RateLimited { retry_at } = env.client.whoami().await.unwrap_err() else {
            panic!("expected a rate limit");
        };
        waits.push(retry_at.duration_since(before).unwrap().as_secs());
        env.clock.set(retry_at);
    }
    assert_eq!(waits, [1, 2, 4, 8]);
    assert_eq!(
        env.fake.request_log().count(),
        4,
        "one request per attempt, none while waiting"
    );

    // The limit lifts, one call succeeds, and the doubling starts again.
    env.clear_limit_and_succeed().await;
    env.fake
        .rate_limit_for(secs(3600), GithubLimit::Secondary403);
    let before = env.clock.now();
    let RemoteError::RateLimited { retry_at } = env.client.whoami().await.unwrap_err() else {
        panic!("expected a rate limit");
    };
    assert_eq!(retry_at.duration_since(before).unwrap(), secs(1));
}

impl GhEnv {
    async fn clear_limit_and_succeed(&self) {
        self.fake.clear_rate_limit();
        self.client.whoami().await.expect("the limit is lifted");
    }
}

#[tokio::test]
async fn a_huge_retry_after_of_a_day_is_honoured() {
    let env = github_env("ghp_x").await;
    let start = env.clock.now();
    env.fake
        .rate_limit_for(secs(86_400), GithubLimit::RetryAfter429);
    expect_limited(&env, start + secs(86_400)).await;
    env.clock.advance(secs(86_399));
    expect_limited(&env, start + secs(86_400)).await;
    assert_eq!(env.fake.request_log().count(), 1);
}

#[tokio::test]
async fn a_rate_limit_for_a_number_of_requests_is_retried_after_the_floor() {
    let env = github_env("ghp_x").await;
    let start = env.clock.now();
    env.fake.rate_limit_requests(2, GithubLimit::RetryAfter403);
    // The fake says 1 s; the adapter waits the 1 s floor, then tries again.
    expect_limited(&env, start + secs(1)).await;
    env.clock.advance(secs(1));
    expect_limited(&env, start + secs(2)).await;
    env.clock.advance(secs(1));
    env.client.whoami().await.unwrap();
    assert_eq!(env.fake.request_log().count(), 3);
}

#[tokio::test]
async fn a_rate_limit_maps_to_syncing_and_a_rejected_token_to_auth_required() {
    let env = github_env("ghp_x").await;
    let start = env.clock.now();
    env.fake
        .rate_limit_for(secs(30), GithubLimit::RetryAfter403);
    let err = env.client.whoami().await.unwrap_err();
    assert_eq!(
        health_for_error(&err),
        Some(SourceHealth::Syncing {
            retry_at: start + secs(30)
        })
    );

    env.clock.advance(secs(30));
    env.fake.revoke_token();
    let err = env.client.whoami().await.unwrap_err();
    assert_eq!(health_for_error(&err), Some(SourceHealth::AuthRequired));

    for other in [
        RemoteError::PermissionDenied,
        RemoteError::NotFound,
        RemoteError::Server { status: 503 },
        RemoteError::Unreachable(UnreachableCause::Connect),
        RemoteError::Moved {
            location: "x".into(),
        },
    ] {
        assert_eq!(health_for_error(&other), None, "{other}");
    }
}

#[tokio::test]
async fn a_5xx_is_a_transient_server_error_and_the_adapter_does_not_retry_it() {
    let env = github_env("ghp_x").await;
    env.fake.fail_next(2, 503);
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::Server { status: 503 }
    );
    assert_eq!(env.fake.request_log().count(), 1);
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::Server { status: 503 }
    );
    env.client.whoami().await.unwrap();
}

// ---- E2 end to end ---------------------------------------------------------------

#[tokio::test]
async fn a_rejected_token_during_a_rate_limit_window_is_still_auth_rejected_after_it() {
    let env = github_env("ghp_old").await;
    env.fake
        .rate_limit_for(secs(10), GithubLimit::RetryAfter403);
    assert!(matches!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::RateLimited { .. }
    ));
    env.clock.advance(secs(10));
    env.fake.revoke_token();
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    let sent = env.fake.request_log().count();
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), sent);
}

// ---- E13: redirects -------------------------------------------------------------

#[tokio::test]
async fn a_moved_repo_is_a_typed_error_and_the_token_goes_nowhere_else() {
    let env = github_env("ghp_secret_token").await;
    let elsewhere = FakeGithub::start().await;
    let target = format!("{}/repos/octo/renamed", elsewhere.base_url());
    env.fake.move_repo("octo/hello", &target);

    for _ in 0..2 {
        assert_eq!(
            env.client.get_repo(&env.repo).await.unwrap_err(),
            RemoteError::Moved {
                location: target.clone()
            }
        );
    }
    assert_eq!(
        env.client.list_tree(&env.repo, "main").await.unwrap_err(),
        RemoteError::Moved {
            location: target.clone()
        }
    );
    assert_eq!(
        env.fake.request_log().count(),
        3,
        "one request per call: no following"
    );
    assert_eq!(
        elsewhere.request_log().count(),
        0,
        "the new location was never contacted"
    );
    assert_eq!(
        env.client
            .get_file(&env.repo, "README.md", "main")
            .await
            .unwrap_err(),
        RemoteError::Moved {
            location: target.clone()
        }
    );
    assert_eq!(elsewhere.request_log().count(), 0);

    // A redirect says nothing about the token: other repos still work.
    assert!(env.client.whoami().await.is_ok());
}

#[tokio::test]
async fn a_scripted_redirect_to_another_host_is_not_followed_either() {
    let env = github_env("ghp_secret_token").await;
    let elsewhere = FakeGithub::start().await;
    env.fake
        .redirect_next(&format!("{}/user", elsewhere.base_url()));
    let err = env.client.whoami().await.unwrap_err();
    assert!(matches!(err, RemoteError::Moved { .. }), "{err:?}");
    assert_eq!(elsewhere.request_log().count(), 0);
}

// ---- a cut body -------------------------------------------------------------------

#[tokio::test]
async fn a_body_cut_mid_stream_is_unreachable_not_a_short_success() {
    let env = github_env("ghp_x").await;
    env.fake
        .put_file("octo/hello", "main", "big.bin", &vec![7u8; 64 * 1024]);

    env.fake.truncate_next_bodies(1);
    assert_eq!(
        env.client
            .get_file(&env.repo, "big.bin", "main")
            .await
            .unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::BodyCut)
    );
    env.fake.truncate_next_bodies(1);
    assert_eq!(
        env.client.list_tree(&env.repo, "main").await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::BodyCut)
    );
    // Nothing latched or limited: the next call is a full answer.
    assert_eq!(
        env.client
            .get_file(&env.repo, "big.bin", "main")
            .await
            .unwrap()
            .len(),
        64 * 1024
    );
}

// ---- names ----------------------------------------------------------------------------

#[tokio::test]
async fn unicode_percent_and_reserved_characters_in_paths_round_trip() {
    let env = github_env("ghp_x").await;
    let names = [
        "docs/ünï cödé 100%.md",
        "日本語/メモ.md",
        "a b/c#d?e.md",
        "100%25.md",
        "emoji 🙂.md",
        "plus+and&amp=eq.md",
    ];
    for (i, name) in names.iter().enumerate() {
        env.fake
            .put_file("octo/hello", "main", name, format!("body {i}").as_bytes());
    }
    let tree = env.client.list_tree(&env.repo, "main").await.unwrap();
    for (i, name) in names.iter().enumerate() {
        assert!(
            tree.entries.iter().any(|e| e.path == *name),
            "{name} is listed"
        );
        assert_eq!(
            env.client.get_file(&env.repo, name, "main").await.unwrap(),
            format!("body {i}").as_bytes(),
            "{name}"
        );
    }
    // On the wire every path is percent-encoded segment by segment.
    for r in env.fake.request_log().matching("/contents/") {
        assert!(
            !r.path.contains(' ') && !r.path.contains('#') && !r.path.contains('?'),
            "{}",
            r.path
        );
    }
    assert!(
        env.fake
            .request_log()
            .matching("/contents/docs/%C3%BCn%C3%AF%20c%C3%B6d%C3%A9%20100%25.md")
            .len()
            == 1
    );
}

#[tokio::test]
async fn names_that_could_change_the_request_are_refused_before_anything_is_sent() {
    let env = github_env("ghp_x").await;
    for bad in [
        "",
        "../etc/passwd",
        "a/../b",
        "a//b",
        "./a",
        "/abs",
        "a/",
        "line\nbreak",
    ] {
        assert_eq!(
            env.client
                .get_file(&env.repo, bad, "main")
                .await
                .unwrap_err(),
            RemoteError::InvalidInput("path"),
            "{bad:?}"
        );
    }
    assert_eq!(
        env.client
            .get_file(&env.repo, "README.md", "")
            .await
            .unwrap_err(),
        RemoteError::InvalidInput("ref")
    );
    assert_eq!(
        env.client.list_tree(&env.repo, "../x").await.unwrap_err(),
        RemoteError::InvalidInput("ref")
    );
    assert_eq!(
        env.client
            .get_blob(&env.repo, "not-hex!")
            .await
            .unwrap_err(),
        RemoteError::InvalidInput("blob sha")
    );
    for bad in [
        "", "owner", "a/b/c", "../x", "a/..", "own er/x", "a/b?c", "a/b#c", "a/%2e%2e", "/x",
    ] {
        assert_eq!(
            RepoRef::parse(bad).unwrap_err(),
            RemoteError::InvalidInput("repository name"),
            "{bad:?}"
        );
    }
    assert_eq!(env.fake.request_log().count(), 0);
}

// ---- tree walks, empty repos, symlinks, submodules (PR-5) --------------------

#[tokio::test]
async fn a_truncated_recursive_tree_is_finished_level_by_level_with_nothing_lost() {
    let env = github_env("ghp_x").await;
    for i in 0..6 {
        env.fake.put_file(
            "octo/hello",
            "main",
            &format!("dir{}/sub/file{i}.md", i % 2),
            b"x",
        );
        env.fake
            .put_file("octo/hello", "main", &format!("top{i}.md"), b"y");
    }
    let whole = tephra_remote::github::full_tree(&env.client, &env.repo, "main")
        .await
        .unwrap();
    assert!(!whole.truncated);

    env.fake.set_tree_limit(Some(3));
    assert!(
        env.client
            .list_tree(&env.repo, "main")
            .await
            .unwrap()
            .truncated
    );
    let walked = tephra_remote::github::full_tree(&env.client, &env.repo, "main")
        .await
        .unwrap();
    assert!(!walked.truncated);

    let mut a: Vec<&str> = whole.entries.iter().map(|e| e.path.as_str()).collect();
    let mut b: Vec<&str> = walked.entries.iter().map(|e| e.path.as_str()).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "no file lost");
    let mut dedup = b.clone();
    dedup.dedup();
    assert_eq!(dedup, b, "no file listed twice");
    assert_eq!(whole.sha, walked.sha);
}

#[tokio::test]
async fn a_repo_with_no_commit_answers_409_to_the_tree_and_the_commit() {
    let env = github_env("ghp_x").await;
    env.fake.set_empty("octo/hello", true);
    assert_eq!(
        env.client.list_tree(&env.repo, "main").await.unwrap_err(),
        RemoteError::Unexpected { status: 409 }
    );
    assert_eq!(
        env.client
            .poll_ref(&env.repo, "main", None)
            .await
            .unwrap_err(),
        RemoteError::Unexpected { status: 409 }
    );
    env.client.get_repo(&env.repo).await.unwrap();
}

#[tokio::test]
async fn symlinks_and_submodules_are_tree_entries_with_their_modes() {
    let env = github_env("ghp_x").await;
    env.fake
        .put_symlink("octo/hello", "main", "link.md", "README.md");
    env.fake.put_submodule(
        "octo/hello",
        "main",
        "vendor/lib",
        "0123456789abcdef0123456789abcdef01234567",
    );
    let tree = env.client.list_tree(&env.repo, "main").await.unwrap();
    let link = tree.entries.iter().find(|e| e.path == "link.md").unwrap();
    assert_eq!(link.mode, "120000");
    assert_eq!(link.kind, TreeEntryKind::Blob);
    let sub = tree
        .entries
        .iter()
        .find(|e| e.path == "vendor/lib")
        .unwrap();
    assert_eq!(sub.mode, "160000");
    assert_eq!(sub.kind, TreeEntryKind::Commit);
}

// ---- streamed blobs ----------------------------------------------------------

/// Reads a whole stream, piece by piece, and counts the pieces.
async fn drain(mut body: tephra_remote::BodyStream) -> Result<(Vec<u8>, usize), RemoteError> {
    let mut all = Vec::new();
    let mut pieces = 0;
    while let Some(piece) = body.chunk().await? {
        pieces += 1;
        all.extend_from_slice(&piece);
    }
    Ok((all, pieces))
}

#[tokio::test]
async fn stream_blob_gives_the_same_bytes_as_get_blob_in_more_than_one_piece() {
    let env = github_env("ghp_x").await;
    let big: Vec<u8> = (0..5 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    env.fake.put_file("octo/hello", "main", "big.bin", &big);
    let sha = FakeGithub::blob_sha(&big);

    let body = env.client.stream_blob(&env.repo, &sha).await.unwrap();
    assert_eq!(body.content_length(), Some(big.len() as u64));
    let (bytes, pieces) = drain(body).await.unwrap();
    assert_eq!(bytes, big);
    assert!(pieces > 1, "one piece for 5 MB is not a stream");
    assert_eq!(env.client.get_blob(&env.repo, &sha).await.unwrap(), big);
}

#[tokio::test]
async fn stream_blob_fails_like_get_blob_before_any_body() {
    let env = github_env("ghp_x").await;
    assert_eq!(
        env.client
            .stream_blob(&env.repo, "0123456789abcdef")
            .await
            .unwrap_err(),
        RemoteError::NotFound
    );
    assert_eq!(
        env.client
            .stream_blob(&env.repo, "not-hex!")
            .await
            .unwrap_err(),
        RemoteError::InvalidInput("blob sha")
    );
    let sha = FakeGithub::blob_sha(b"# hello\n");
    env.fake.fail_next(1, 503);
    assert_eq!(
        env.client.stream_blob(&env.repo, &sha).await.unwrap_err(),
        RemoteError::Server { status: 503 }
    );
    env.fake.revoke_token();
    assert_eq!(
        env.client.stream_blob(&env.repo, &sha).await.unwrap_err(),
        RemoteError::AuthRejected
    );
}

#[tokio::test]
async fn a_streamed_body_that_is_cut_fails_with_body_cut_after_some_pieces() {
    let env = github_env("ghp_x").await;
    let big = vec![7u8; 256 * 1024];
    env.fake.put_file("octo/hello", "main", "big.bin", &big);
    let sha = FakeGithub::blob_sha(&big);
    env.fake.truncate_next_bodies(1);
    let body = env.client.stream_blob(&env.repo, &sha).await.unwrap();
    assert_eq!(
        drain(body).await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::BodyCut)
    );
}
