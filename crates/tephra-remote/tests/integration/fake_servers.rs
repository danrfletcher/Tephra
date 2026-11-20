//! The fakes' own behaviour, so later PRs can trust them: loopback
//! binding, the request log, tokens, scripting, ETags, page tokens, and
//! the shape of every scripted failure. These tests talk plain HTTP to
//! the fakes; no adapter code is involved.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::HeaderMap;
use tephra_remote::clock::{Clock, FakeClock};
use tephra_remote::testing::{DriveLimit, FakeDrive, FakeGithub, GithubLimit};

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {:?}", self.body))
    }
}

async fn get_with(url: &str, headers: &[(&str, &str)]) -> Answer {
    let mut req = http().get(url);
    for (n, v) in headers {
        req = req.header(*n, *v);
    }
    let resp = req.send().await.expect("the fake answers");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().await.expect("a complete body");
    Answer {
        status,
        headers,
        body,
    }
}

const AUTH: (&str, &str) = ("authorization", "Bearer tok");

async fn get(url: &str) -> Answer {
    get_with(url, &[AUTH]).await
}

async fn github() -> FakeGithub {
    let fake = FakeGithub::start().await;
    fake.add_repo("octo/hello", "main");
    fake.put_file("octo/hello", "main", "README.md", b"# hello\n");
    fake.put_file("octo/hello", "main", "docs/a.md", b"a\n");
    fake
}

async fn drive() -> FakeDrive {
    let fake = FakeDrive::start().await;
    fake.add_folder("fld", "Notes", None);
    fake.add_file("f1", "a.md", "text/markdown", Some("fld"), b"# a\n");
    fake.add_doc("d1", "Doc", Some("fld"), "# Doc\n");
    fake
}

// ---- loopback ----------------------------------------------------------------------

#[tokio::test]
async fn the_fakes_bind_only_to_loopback_on_their_own_ports_and_close_on_drop() {
    let gh = github().await;
    let dr = drive().await;
    let gh2 = FakeGithub::start().await;
    for url in [gh.base_url(), dr.base_url(), gh2.base_url()] {
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let port: u16 = url["http://127.0.0.1:".len()..]
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(port, 0);
    }
    assert!(dr.base_url().ends_with("/drive/v3"));
    assert!(!gh.base_url().ends_with('/'));
    assert_ne!(gh.base_url(), gh2.base_url());

    let url = format!("{}/user", gh2.base_url());
    assert_eq!(get(&url).await.status, 200);
    drop(gh2);
    assert!(http()
        .get(&url)
        .header("authorization", "Bearer tok")
        .send()
        .await
        .is_err());
}

// ---- the request log -----------------------------------------------------------------

#[tokio::test]
async fn the_log_keeps_method_path_query_headers_and_order() {
    let gh = github().await;
    let log = gh.request_log();
    assert_eq!(log.count(), 0);

    get_with(
        &format!(
            "{}/repos/octo/hello/contents/docs/a.md?ref=main",
            gh.base_url()
        ),
        &[AUTH, ("x-probe", "one")],
    )
    .await;
    get(&format!("{}/user", gh.base_url())).await;

    let all = log.all();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].method, "GET");
    assert_eq!(all[0].path, "/repos/octo/hello/contents/docs/a.md");
    assert_eq!(all[0].query.as_deref(), Some("ref=main"));
    assert_eq!(
        all[0].target(),
        "/repos/octo/hello/contents/docs/a.md?ref=main"
    );
    assert_eq!(all[0].header("X-Probe"), Some("one"));
    assert_eq!(all[0].header("authorization"), Some("Bearer tok"));
    assert_eq!(all[0].header("nope"), None);
    assert_eq!(
        (all[1].path.as_str(), all[1].query.as_deref()),
        ("/user", None)
    );
    assert_eq!(log.last().unwrap().path, "/user");
    assert_eq!(log.matching("contents").len(), 1);

    // A clone shares the list; clear empties it.
    let clone = log.clone();
    get(&format!("{}/user", gh.base_url())).await;
    assert_eq!(clone.count(), 3);
    log.clear();
    assert_eq!(clone.count(), 0);
}

#[tokio::test]
async fn refused_failed_and_rate_limited_requests_are_logged_too() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    get_with(&url, &[]).await; // no token
    gh.fail_next(1, 500);
    get(&url).await;
    gh.rate_limit_requests(1, GithubLimit::Secondary403);
    get(&url).await;
    get(&format!("{}/nope", gh.base_url())).await; // no such route
    assert_eq!(gh.request_log().count(), 4);

    let dr = drive().await;
    get_with(&format!("{}/about", dr.base_url()), &[]).await;
    assert_eq!(dr.request_log().count(), 1);
}

// ---- tokens --------------------------------------------------------------------------

#[tokio::test]
async fn tokens_any_only_revoked_in_both_providers_formats() {
    let gh = github().await;
    let gh_url = format!("{}/user", gh.base_url());
    let dr = drive().await;
    let dr_url = format!("{}/about", dr.base_url());

    // No header, empty token, wrong scheme: 401 in each provider's words.
    for headers in [
        &[][..],
        &[("authorization", "Bearer ")][..],
        &[("authorization", "Basic abc")][..],
    ] {
        let a = get_with(&gh_url, headers).await;
        assert_eq!(
            (a.status, a.json()["message"].as_str()),
            (401, Some("Bad credentials"))
        );
        let a = get_with(&dr_url, headers).await;
        assert_eq!(a.status, 401);
        assert_eq!(a.json()["error"]["errors"][0]["reason"], "authError");
    }
    assert_eq!(get(&gh_url).await.status, 200);
    assert_eq!(get(&dr_url).await.status, 200);

    gh.accept_only_token("good");
    dr.accept_only_token("good");
    assert_eq!(get(&gh_url).await.status, 401);
    assert_eq!(
        get_with(&gh_url, &[("authorization", "Bearer good")])
            .await
            .status,
        200
    );
    assert_eq!(
        get_with(&dr_url, &[("authorization", "Bearer good")])
            .await
            .status,
        200
    );

    gh.revoke_token();
    dr.revoke_token();
    assert_eq!(
        get_with(&gh_url, &[("authorization", "Bearer good")])
            .await
            .status,
        401
    );
    assert_eq!(
        get_with(&dr_url, &[("authorization", "Bearer good")])
            .await
            .status,
        401
    );

    gh.accept_any_token();
    assert_eq!(get(&gh_url).await.status, 200);
}

// ---- scripting -------------------------------------------------------------------------

#[tokio::test]
async fn fail_next_is_first_in_first_out_and_uses_the_providers_error_format() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    gh.fail_next(2, 503);
    gh.fail_next(1, 404);
    gh.fail_next(1, 401);
    let statuses: Vec<u16> = vec![
        get(&url).await.status,
        get(&url).await.status,
        get(&url).await.status,
        get(&url).await.status,
        get(&url).await.status,
    ];
    assert_eq!(statuses, [503, 503, 404, 401, 200]);

    gh.fail_next(1, 403);
    let a = get(&url).await;
    assert_eq!(
        a.json()["message"],
        "Resource not accessible by integration"
    );

    let dr = drive().await;
    let url = format!("{}/about", dr.base_url());
    dr.fail_next(1, 403);
    dr.fail_next(1, 500);
    let a = get(&url).await;
    assert_eq!(
        (a.status, a.json()["error"]["errors"][0]["reason"].clone()),
        (403, "insufficientPermissions".into())
    );
    assert_eq!(get(&url).await.status, 500);
    assert_eq!(get(&url).await.status, 200);
}

#[tokio::test]
async fn a_failed_request_precedes_the_route_but_follows_the_token_check() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    gh.fail_next(1, 500);
    // No token: the 401 comes first and the scripted failure is kept.
    assert_eq!(get_with(&url, &[]).await.status, 401);
    assert_eq!(get(&url).await.status, 500);
}

#[tokio::test]
async fn github_rate_limit_styles_have_the_documented_shapes() {
    let clock = Arc::new(FakeClock::at_epoch_2026());
    let now = clock
        .now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let gh = FakeGithub::start_with_clock(clock.clone()).await;
    let url = format!("{}/user", gh.base_url());

    gh.rate_limit_for(Duration::from_secs(90), GithubLimit::RetryAfter403);
    let a = get(&url).await;
    assert_eq!((a.status, a.header("retry-after")), (403, Some("90")));

    gh.rate_limit_for(Duration::from_secs(90), GithubLimit::RetryAfter429);
    let a = get(&url).await;
    assert_eq!((a.status, a.header("retry-after")), (429, Some("90")));

    gh.rate_limit_for(Duration::from_secs(90), GithubLimit::Reset403);
    let a = get(&url).await;
    assert_eq!(a.status, 403);
    assert_eq!(a.header("x-ratelimit-remaining"), Some("0"));
    assert_eq!(
        a.header("x-ratelimit-reset"),
        Some((now + 90).to_string().as_str())
    );
    assert_eq!(a.header("retry-after"), None);

    gh.rate_limit_for(Duration::from_secs(90), GithubLimit::Secondary403);
    let a = get(&url).await;
    assert_eq!(a.status, 403);
    assert!(a.body.contains("secondary rate limit"));
    assert!(a.header("retry-after").is_none() && a.header("x-ratelimit-remaining").is_none());

    // The window counts down on the fake's clock, rounded up to whole seconds.
    gh.rate_limit_for(Duration::from_millis(2500), GithubLimit::RetryAfter403);
    assert_eq!(get(&url).await.header("retry-after"), Some("3"));
    clock.advance(Duration::from_millis(1600));
    assert_eq!(get(&url).await.header("retry-after"), Some("1"));
    clock.advance(Duration::from_millis(900));
    assert_eq!(get(&url).await.status, 200, "the window is over");
    assert_eq!(get(&url).await.status, 200);
}

#[tokio::test]
async fn drive_rate_limit_styles_have_the_documented_shapes() {
    let dr = drive().await;
    let url = format!("{}/about", dr.base_url());

    dr.rate_limit_for(
        Duration::from_secs(60),
        DriveLimit::Status429 { retry_after: true },
    );
    let a = get(&url).await;
    assert_eq!((a.status, a.header("retry-after")), (429, Some("60")));

    dr.rate_limit_for(
        Duration::from_secs(60),
        DriveLimit::Status429 { retry_after: false },
    );
    let a = get(&url).await;
    assert_eq!((a.status, a.header("retry-after")), (429, None));

    for reason in [
        "rateLimitExceeded",
        "userRateLimitExceeded",
        "sharingRateLimitExceeded",
    ] {
        dr.rate_limit_for(
            Duration::from_secs(60),
            DriveLimit::Reason403 {
                reason,
                retry_after: false,
            },
        );
        let a = get(&url).await;
        assert_eq!(a.status, 403);
        assert_eq!(a.json()["error"]["errors"][0]["reason"], reason);
        assert_eq!(a.header("retry-after"), None);
    }
    dr.rate_limit_for(
        Duration::from_secs(60),
        DriveLimit::Reason403 {
            reason: "userRateLimitExceeded",
            retry_after: true,
        },
    );
    assert_eq!(get(&url).await.header("retry-after"), Some("60"));

    dr.clear_rate_limit();
    assert_eq!(get(&url).await.status, 200);
}

#[tokio::test]
async fn rate_limit_for_a_count_of_requests_then_normal() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    gh.rate_limit_requests(2, GithubLimit::RetryAfter429);
    let statuses = [
        get(&url).await.status,
        get(&url).await.status,
        get(&url).await.status,
    ];
    assert_eq!(statuses, [429, 429, 200]);
    gh.rate_limit_requests(5, GithubLimit::RetryAfter429);
    gh.clear_rate_limit();
    assert_eq!(get(&url).await.status, 200);
}

#[tokio::test]
async fn a_failure_script_beats_a_rate_limit_and_a_rate_limit_beats_the_route() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    gh.rate_limit_for(Duration::from_secs(60), GithubLimit::RetryAfter403);
    gh.fail_next(1, 500);
    assert_eq!(get(&url).await.status, 500);
    assert_eq!(get(&url).await.status, 403);
}

#[tokio::test]
async fn latency_delays_the_answer() {
    let dr = drive().await;
    let url = format!("{}/about", dr.base_url());
    dr.set_latency(Duration::from_millis(200));
    let started = Instant::now();
    assert_eq!(get(&url).await.status, 200);
    assert!(started.elapsed() >= Duration::from_millis(200));
    dr.set_latency(Duration::ZERO);
    let started = Instant::now();
    get(&url).await;
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[tokio::test]
async fn a_cut_body_promises_the_full_length_and_fails_the_stream() {
    let gh = github().await;
    gh.put_file("octo/hello", "main", "big.bin", &vec![9u8; 40_000]);
    let url = format!(
        "{}/repos/octo/hello/contents/big.bin?ref=main",
        gh.base_url()
    );

    gh.truncate_next_bodies(1);
    let resp = http()
        .get(&url)
        .header("authorization", "Bearer tok")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.content_length(), Some(40_000));
    assert!(resp.bytes().await.is_err(), "the body ends early");

    // One use per count; error answers do not use it up.
    gh.truncate_next_bodies(1);
    assert_eq!(
        get(&format!("{}/repos/octo/nope", gh.base_url()))
            .await
            .status,
        404
    );
    let resp = http()
        .get(&url)
        .header("authorization", "Bearer tok")
        .send()
        .await
        .unwrap();
    assert!(resp.bytes().await.is_err());
    let resp = http()
        .get(&url)
        .header("authorization", "Bearer tok")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().len(), 40_000);

    let dr = drive().await;
    dr.truncate_next_bodies(1);
    let resp = http()
        .get(format!("{}/files/f1?alt=media", dr.base_url()))
        .header("authorization", "Bearer tok")
        .send()
        .await
        .unwrap();
    assert!(resp.bytes().await.is_err());
}

#[tokio::test]
async fn an_empty_body_is_a_200_with_nothing_in_it() {
    let gh = github().await;
    gh.empty_next_bodies(1);
    let a = get(&format!("{}/user", gh.base_url())).await;
    assert_eq!((a.status, a.body.as_str()), (200, ""));
    assert!(!get(&format!("{}/user", gh.base_url()))
        .await
        .body
        .is_empty());
}

#[tokio::test]
async fn redirect_next_answers_301_with_the_location_once_per_call() {
    let gh = github().await;
    let url = format!("{}/user", gh.base_url());
    gh.redirect_next("http://127.0.0.1:1/a");
    gh.redirect_next("http://127.0.0.1:1/b");
    let a = get(&url).await;
    assert_eq!(
        (a.status, a.header("location")),
        (301, Some("http://127.0.0.1:1/a"))
    );
    assert_eq!(
        get(&url).await.header("location"),
        Some("http://127.0.0.1:1/b")
    );
    assert_eq!(get(&url).await.status, 200);
}

// ---- the GitHub model -------------------------------------------------------------------

#[tokio::test]
async fn github_etags_on_every_route_and_304_on_a_match() {
    let gh = github().await;
    let base = gh.base_url().to_string();
    let sha = FakeGithub::blob_sha(b"# hello\n");
    let urls = [
        format!("{base}/user"),
        format!("{base}/repos/octo/hello"),
        format!("{base}/repos/octo/hello/git/trees/main?recursive=1"),
        format!("{base}/repos/octo/hello/git/blobs/{sha}"),
        format!("{base}/repos/octo/hello/contents/README.md?ref=main"),
        format!("{base}/repos/octo/hello/commits/main"),
    ];
    for url in &urls {
        let first = get(url).await;
        assert_eq!(first.status, 200, "{url}");
        let etag = first
            .header("etag")
            .unwrap_or_else(|| panic!("{url}: no ETag"))
            .to_string();
        assert!(!first.body.is_empty());

        let hit = get_with(url, &[AUTH, ("if-none-match", &etag)]).await;
        assert_eq!((hit.status, hit.body.as_str()), (304, ""), "{url}");
        assert_eq!(hit.header("etag"), Some(etag.as_str()));

        for variant in [
            format!("W/{etag}"),
            format!("\"other\", {etag}"),
            "*".to_string(),
        ] {
            assert_eq!(
                get_with(url, &[AUTH, ("if-none-match", &variant)])
                    .await
                    .status,
                304,
                "{url} {variant}"
            );
        }
        let miss = get_with(url, &[AUTH, ("if-none-match", "\"stale\"")]).await;
        assert_eq!(miss.status, 200, "{url}");
        assert_eq!(miss.body, first.body);
    }
}

#[tokio::test]
async fn a_commit_changes_the_head_sha_and_the_etag_and_a_304_stops_matching() {
    let gh = github().await;
    let url = format!("{}/repos/octo/hello/commits/main", gh.base_url());
    let accept = ("accept", "application/vnd.github.sha");
    let first = get_with(&url, &[AUTH, accept]).await;
    assert_eq!(first.body, gh.head_sha("octo/hello", "main"));
    let etag = first.header("etag").unwrap().to_string();

    let new_head = gh.put_file("octo/hello", "main", "new.md", b"n");
    assert_ne!(new_head, first.body);
    let after = get_with(&url, &[AUTH, accept, ("if-none-match", &etag)]).await;
    assert_eq!(
        (after.status, after.body.as_str()),
        (200, new_head.as_str())
    );
    assert_ne!(after.header("etag"), Some(etag.as_str()));

    // Removing a file is a commit too.
    let removed = gh.remove_file("octo/hello", "main", "new.md");
    assert_ne!(removed, new_head);
    assert_eq!(get_with(&url, &[AUTH, accept]).await.body, removed);
}

#[tokio::test]
async fn github_tree_lists_files_and_intermediate_directories() {
    let gh = github().await;
    gh.put_file("octo/hello", "main", "a/b/c.txt", b"c");
    let a = get(&format!(
        "{}/repos/octo/hello/git/trees/main?recursive=1",
        gh.base_url()
    ))
    .await;
    let tree = a.json();
    let entries: Vec<(String, String)> = tree["tree"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want: Vec<(String, String)> = [
        ("README.md", "blob"),
        ("a", "tree"),
        ("a/b", "tree"),
        ("a/b/c.txt", "blob"),
        ("docs", "tree"),
        ("docs/a.md", "blob"),
    ]
    .iter()
    .map(|(p, t)| (p.to_string(), t.to_string()))
    .collect();
    assert_eq!(entries, want);
    assert_eq!(tree["truncated"], false);
}

#[tokio::test]
async fn github_default_and_other_branches_and_unknown_things() {
    let gh = github().await;
    gh.put_file("octo/hello", "dev", "dev.md", b"d");
    let base = gh.base_url().to_string();
    let on_dev = get(&format!("{base}/repos/octo/hello/contents/dev.md?ref=dev")).await;
    assert_eq!((on_dev.status, on_dev.body.as_str()), (200, "d"));
    // No `ref`: the default branch.
    assert_eq!(
        get(&format!("{base}/repos/octo/hello/contents/dev.md"))
            .await
            .status,
        404
    );
    assert_eq!(
        get(&format!("{base}/repos/octo/hello/contents/README.md"))
            .await
            .body,
        "# hello\n"
    );
    for missing in [
        "/repos/octo/none",
        "/repos/octo/hello/git/trees/zzz",
        "/repos/octo/hello/git/blobs/abcd",
        "/repos/octo/hello/commits/zzz",
    ] {
        assert_eq!(
            get(&format!("{base}{missing}")).await.status,
            404,
            "{missing}"
        );
    }
}

#[tokio::test]
async fn a_hidden_repo_is_a_404_and_a_moved_repo_a_301_on_every_route() {
    let gh = github().await;
    let base = gh.base_url().to_string();
    let routes = [
        "/repos/octo/hello",
        "/repos/octo/hello/git/trees/main",
        "/repos/octo/hello/contents/README.md",
        "/repos/octo/hello/commits/main",
    ];
    gh.set_hidden("octo/hello", true);
    for r in routes {
        assert_eq!(get(&format!("{base}{r}")).await.status, 404, "{r}");
    }
    gh.set_hidden("octo/hello", false);
    assert_eq!(get(&format!("{base}{}", routes[0])).await.status, 200);

    gh.move_repo("octo/hello", "https://api.github.com/repositories/42");
    for r in routes {
        let a = get(&format!("{base}{r}")).await;
        assert_eq!(
            (a.status, a.header("location")),
            (301, Some("https://api.github.com/repositories/42")),
            "{r}"
        );
    }
    gh.clear_move("octo/hello");
    assert_eq!(get(&format!("{base}{}", routes[0])).await.status, 200);
}

// ---- the Drive model ----------------------------------------------------------------------

#[tokio::test]
async fn drive_etags_on_every_route_and_304_on_a_match() {
    let dr = drive().await;
    let base = dr.base_url().to_string();
    let urls = [
        format!("{base}/about"),
        format!("{base}/files?q=%27fld%27%20in%20parents"),
        format!("{base}/files/f1"),
        format!("{base}/files/f1?alt=media"),
        format!("{base}/files/d1/export?mimeType=text%2Fmarkdown"),
        format!("{base}/changes/startPageToken"),
        format!("{base}/changes?pageToken=100"),
    ];
    for url in &urls {
        let first = get(url).await;
        assert_eq!(first.status, 200, "{url}");
        let etag = first
            .header("etag")
            .unwrap_or_else(|| panic!("{url}: no ETag"))
            .to_string();
        let hit = get_with(url, &[AUTH, ("if-none-match", &etag)]).await;
        assert_eq!((hit.status, hit.body.as_str()), (304, ""), "{url}");
        assert_eq!(
            get_with(url, &[AUTH, ("if-none-match", "\"stale\"")])
                .await
                .status,
            200
        );
    }
}

#[tokio::test]
async fn drive_metadata_shapes() {
    let dr = drive().await;
    let base = dr.base_url().to_string();
    let f = get(&format!("{base}/files/f1")).await.json();
    assert_eq!(f["id"], "f1");
    assert_eq!(f["name"], "a.md");
    assert_eq!(f["mimeType"], "text/markdown");
    assert_eq!(f["parents"][0], "fld");
    assert_eq!(f["size"], "4", "Drive sends size as a string");
    assert_eq!(f["headRevisionId"], dr.head_revision("f1"));

    let d = get(&format!("{base}/files/d1")).await.json();
    assert!(d.get("headRevisionId").is_none() && d.get("size").is_none());
    let folder = get(&format!("{base}/files/fld")).await.json();
    assert_eq!(folder["parents"][0], "root");

    assert_eq!(get(&format!("{base}/files/none")).await.status, 404);
    // Export needs the right format and a native file.
    assert_eq!(
        get(&format!("{base}/files/d1/export?mimeType=text%2Fhtml"))
            .await
            .status,
        400
    );
    assert_eq!(
        get(&format!("{base}/files/f1/export?mimeType=text%2Fmarkdown"))
            .await
            .status,
        403
    );
    assert_eq!(get(&format!("{base}/files/d1?alt=media")).await.status, 403);
    assert_eq!(get(&format!("{base}/files?q=nothing")).await.status, 400);
}

#[tokio::test]
async fn drive_page_tokens_and_page_size() {
    let dr = drive().await;
    for i in 0..5 {
        dr.add_file(&format!("x{i}"), "x", "text/plain", Some("fld"), b"x");
    }
    let base = dr.base_url().to_string();
    let q = "q=%27fld%27%20in%20parents";

    // 7 children; requested page size 3 → pages of 3, 3, 1.
    let mut token: Option<String> = None;
    let mut sizes = Vec::new();
    loop {
        let url = match &token {
            None => format!("{base}/files?{q}&pageSize=3"),
            Some(t) => format!("{base}/files?{q}&pageSize=3&pageToken={t}"),
        };
        let page = get(&url).await.json();
        sizes.push(page["files"].as_array().unwrap().len());
        token = page["nextPageToken"].as_str().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    assert_eq!(sizes, [3, 3, 1]);

    // The fake's own page size caps the request's.
    dr.set_page_size(2);
    let page = get(&format!("{base}/files?{q}&pageSize=100")).await.json();
    assert_eq!(page["files"].as_array().unwrap().len(), 2);
    assert!(page["nextPageToken"].is_string());
    assert_eq!(
        get(&format!("{base}/files?{q}&pageToken=bogus"))
            .await
            .status,
        400
    );
}

#[tokio::test]
async fn drive_changes_feed_records_every_kind_of_change() {
    let dr = drive().await;
    let base = dr.base_url().to_string();
    let start = get(&format!("{base}/changes/startPageToken")).await.json()["startPageToken"]
        .as_str()
        .unwrap()
        .to_string();

    // Nothing since the start token: the next start token is the same.
    let page = get(&format!("{base}/changes?pageToken={start}"))
        .await
        .json();
    assert_eq!(page["changes"].as_array().unwrap().len(), 0);
    assert_eq!(page["newStartPageToken"], start.as_str());
    assert!(page.get("nextPageToken").is_none());

    dr.update_file("f1", b"two");
    dr.rename_file("f1", "b.md");
    dr.remove_file("d1");
    let page = get(&format!("{base}/changes?pageToken={start}"))
        .await
        .json();
    let changes = page["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 3);
    assert_eq!(changes[0]["file"]["headRevisionId"], dr.head_revision("f1"));
    assert_eq!(changes[1]["file"]["name"], "b.md");
    assert_eq!(changes[2]["removed"], true);
    assert!(changes[2].get("file").is_none());
    let new_start = page["newStartPageToken"].as_str().unwrap();
    assert_ne!(new_start, start);

    // After the removal the file is gone from listings.
    assert_eq!(get(&format!("{base}/files/d1")).await.status, 404);
    assert_eq!(
        get(&format!("{base}/changes?pageToken=oops")).await.status,
        400
    );
}
