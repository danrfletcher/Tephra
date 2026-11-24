//! F2, F3, F4, F10 and F12 as a fence on the read-side adapter surface, and
//! (PR-11) on the one write Tephra makes: new content for a Drive file that
//! exists.
//!
//! The source checks read the text of the trait modules (above their unit
//! tests), so a later change that adds a write method, a list-everything
//! method or a listener fails here and has to be argued for. The F12 check
//! goes through the real adapters and the fakes' request log.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use tephra_remote::clock::SystemClock;
use tephra_remote::credentials::Provider;
use tephra_remote::drive::{DriveAdapter, DriveClient};
use tephra_remote::github::{GithubAdapter, GithubClient, RepoRef};
use tephra_remote::testing::{FakeDrive, FakeGithub};

mod common;
use common::Creds;

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match text.split_once("\n#[cfg(test)]\nmod ") {
        Some((production, _)) => production.to_string(),
        None => text,
    }
}

/// The text of `pub trait <name> { ... }`, up to its closing brace at
/// column 0.
fn trait_body(src: &str, name: &str) -> String {
    let start = src
        .find(&format!("pub trait {name}"))
        .unwrap_or_else(|| panic!("trait {name} exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("the trait closes");
    rest[..end].to_string()
}

/// Every `fn name` in a block of source.
fn fn_names(text: &str) -> Vec<String> {
    text.split("fn ")
        .skip(1)
        .map(|after| {
            after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect()
}

/// Whole `_`-separated words of a method name that mean a write.
const WRITE_WORDS: &[&str] = &[
    "delete",
    "remove",
    "destroy",
    "trash",
    "move",
    "rename",
    "update",
    "create",
    "put",
    "post",
    "patch",
    "push",
    "force",
    "merge",
    "rebase",
    "write",
    "upload",
    "insert",
    "copy",
    "empty",
    "star",
    "unstar",
    "revert",
    "reset",
    "commit",
    "fork",
    "dispatch",
    "set",
    "add",
    "grant",
    "share",
    "permission",
    "permissions",
];

/// Whole words of a method name that mean "everything of the account".
const LIST_EVERYTHING_WORDS: &[&str] = &[
    "repos",
    "repositories",
    "all",
    "orgs",
    "organizations",
    "drives",
    "search",
    "shared",
    "starred",
    "installations",
];

fn assert_no_word(trait_name: &str, names: &[String], words: &[&str], why: &str) {
    for name in names {
        for part in name.to_lowercase().split('_') {
            assert!(
                !words.contains(&part),
                "{trait_name}::{name} looks like {why} (`{part}`)"
            );
        }
    }
}

#[test]
fn f2_f3_neither_trait_can_write_delete_move_rename_or_rewrite_history() {
    let gh = fn_names(&trait_body(&source("src/github.rs"), "GithubAdapter"));
    let dr = fn_names(&trait_body(&source("src/drive.rs"), "DriveAdapter"));
    assert!(
        gh.len() >= 6 && dr.len() >= 7,
        "the fence reads the real traits: {gh:?} {dr:?}"
    );

    assert_no_word("GithubAdapter", &gh, WRITE_WORDS, "a write");
    assert_no_word("DriveAdapter", &dr, WRITE_WORDS, "a write");

    // The whole surface, named: a new method has to be added here on purpose.
    let mut gh_sorted = gh.clone();
    gh_sorted.sort();
    assert_eq!(
        gh_sorted,
        [
            "check_token",
            "get_blob",
            "get_file",
            "get_repo",
            "list_tree",
            "list_tree_level",
            "poll_ref",
            "stream_blob",
            "whoami"
        ]
    );
    let mut dr_sorted = dr.clone();
    dr_sorted.sort();
    assert_eq!(
        dr_sorted,
        [
            "download",
            "export_markdown",
            "get_file",
            "list_changes",
            "list_children",
            "start_page_token",
            "stream_download",
            "whoami"
        ]
    );
}

#[test]
fn f2_f3_the_transport_only_ever_sends_get() {
    let transport = source("src/transport.rs");
    for forbidden in [
        ".post(",
        ".put(",
        ".patch(",
        ".delete(",
        ".head(",
        ".request(",
        "Method::POST",
        "Method::PUT",
        "Method::PATCH",
        "Method::DELETE",
        ".form(",
        ".multipart(",
        ".body(",
        ".json(",
    ] {
        assert!(
            !transport.contains(forbidden),
            "transport.rs must not use `{forbidden}`"
        );
    }
    assert_eq!(
        transport.matches(".get(self.url(&request))").count(),
        1,
        "one send path, and it is a GET"
    );
    for rel in ["src/github.rs", "src/drive.rs"] {
        let text = source(rel);
        for forbidden in [
            "reqwest::Client",
            ".post(",
            ".put(",
            ".patch(",
            ".delete(",
            ".send()",
        ] {
            assert!(
                !text.contains(forbidden),
                "{rel} must send only through the shared transport, not `{forbidden}`"
            );
        }
    }
}

/// PR-11: the spec has Dan's saved edits of a Drive file go to Drive as a
/// new revision, so there is one write. It is fenced as tightly as the
/// reads: its own trait with one method, its own module with one `PATCH` to
/// one route, and nothing that deletes, trashes, moves, renames or creates.
#[test]
fn pr11_the_only_write_is_new_content_for_a_drive_file_that_exists() {
    let drive = source("src/drive.rs");
    let up = fn_names(&trait_body(&drive, "DriveUploader"));
    assert_eq!(up, ["replace_media"], "one method, on its own trait");
    assert_no_word(
        "DriveUploader",
        &up,
        &[
            "delete",
            "remove",
            "trash",
            "move",
            "rename",
            "create",
            "copy",
            "share",
            "permission",
        ],
        "a write other than new content",
    );

    // The one module that builds a write request.
    let upload = source("src/upload.rs");
    assert_eq!(upload.matches(".patch(").count(), 1, "one PATCH");
    for forbidden in [
        ".post(",
        ".put(",
        ".delete(",
        ".head(",
        ".request(",
        "Method::",
        ".multipart(",
    ] {
        assert!(
            !upload.contains(forbidden),
            "upload.rs must not use `{forbidden}`"
        );
    }
    let code: String = upload
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        code.matches("/upload/drive/v3/files/{id}?uploadType=media")
            .count(),
        1,
        "one route: media for a file id"
    );
    assert!(!upload.contains("uploadType=multipart") && !upload.contains("uploadType=resumable"));

    // Nothing else sends a write: not the transport, not the read adapters,
    // not GitHub (whose writes are PR-9's and go through git).
    for rel in [
        "src/transport.rs",
        "src/github.rs",
        "src/drive.rs",
        "src/rate_limit.rs",
    ] {
        let text = source(rel);
        for forbidden in [".patch(", ".put(", ".post(", ".delete(", "Method::PATCH"] {
            assert!(
                !text.contains(forbidden),
                "{rel} must not use `{forbidden}`"
            );
        }
    }
    assert!(!source("src/github.rs").contains("Uploader"));
    // The write path is reached only through the shared transport: no
    // client of its own.
    assert!(!upload.contains("reqwest::Client"));
    assert!(upload.contains("send_built"));
}

#[test]
fn f4_neither_trait_can_list_every_repo_or_every_drive_of_an_account() {
    let gh_trait = trait_body(&source("src/github.rs"), "GithubAdapter");
    let dr_trait = trait_body(&source("src/drive.rs"), "DriveAdapter");
    assert_no_word(
        "GithubAdapter",
        &fn_names(&gh_trait),
        LIST_EVERYTHING_WORDS,
        "a list-everything call",
    );
    assert_no_word(
        "DriveAdapter",
        &fn_names(&dr_trait),
        LIST_EVERYTHING_WORDS,
        "a list-everything call",
    );

    // Every call that lists takes the one repo or the one folder it lists.
    let gh_src = source("src/github.rs");
    assert!(gh_trait.contains("list_tree<'a>"));
    assert!(gh_trait.contains("repo: &'a RepoRef"));
    assert!(dr_trait.contains("list_children<'a>"));
    assert!(dr_trait.contains("folder_id") || dr_trait.contains("file_id"));

    // No route that names the account's repos or drives is built anywhere.
    for rel in [
        "src/github.rs",
        "src/drive.rs",
        "src/transport.rs",
        "src/upload.rs",
    ] {
        let text = source(rel);
        for route in [
            "\"/user/repos\"",
            "/user/repos",
            "/user/orgs",
            "/orgs/",
            "/installation/repositories",
            "/drives",
            "corpora",
            "allDrives",
            "includeItemsFromAllDrives",
            "supportsAllDrives",
        ] {
            assert!(!text.contains(route), "{rel} must not use `{route}` (F4)");
        }
    }
    assert!(!gh_src.contains("/search/"));
}

#[test]
fn f10_the_real_adapters_open_no_listener() {
    let production = [
        "src/github.rs",
        "src/drive.rs",
        "src/transport.rs",
        "src/upload.rs",
        "src/rate_limit.rs",
        "src/base_url.rs",
        "src/health.rs",
        "src/clock.rs",
        "src/error.rs",
    ];
    for rel in production {
        let text = source(rel);
        for forbidden in [
            "TcpListener",
            "UdpSocket",
            "UnixListener",
            "axum",
            "hyper::server",
            ".listen(",
            "bind(",
        ] {
            assert!(
                !text.contains(forbidden),
                "{rel} must not use `{forbidden}` (F10)"
            );
        }
    }
    // The only listener code is behind the test-support feature.
    let lib = source("src/lib.rs");
    let at = lib
        .find("pub mod testing")
        .expect("the testing module is declared");
    assert!(
        lib[..at]
            .trim_end()
            .ends_with("#[cfg(feature = \"test-support\")]"),
        "`pub mod testing` must be gated on the test-support feature"
    );
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let deps = manifest.split("[dev-dependencies]").next().unwrap();
    assert!(
        deps.contains("axum = { version = \"0.8.9\", optional = true"),
        "axum is optional"
    );
    assert!(deps.contains("test-support = [\"dep:axum\""));
}

#[test]
fn f10_the_fakes_bind_only_to_loopback() {
    let testing = [
        "src/testing/mod.rs",
        "src/testing/github.rs",
        "src/testing/drive.rs",
    ];
    let mut binds = 0;
    for rel in testing {
        let text = source(rel);
        for forbidden in [
            "0.0.0.0",
            "UNSPECIFIED",
            "[::]",
            "\"::\"",
            "Ipv6Addr::new(0",
            "SocketAddr::from(([0, 0, 0, 0]",
            "to_socket_addrs",
        ] {
            assert!(
                !text.contains(forbidden),
                "{rel} must not use `{forbidden}`"
            );
        }
        binds += text.matches("TcpListener::bind(").count();
        if text.contains("TcpListener::bind(") {
            assert!(
                text.contains("bind((\"127.0.0.1\", 0))"),
                "{rel}: bind to 127.0.0.1 with port 0 only"
            );
        }
    }
    assert_eq!(binds, 1, "one bind site, in the shared `serve`");
}

#[tokio::test]
async fn f10_a_running_fake_answers_on_loopback_and_reports_loopback() {
    let gh = FakeGithub::start().await;
    let dr = FakeDrive::start().await;
    for url in [gh.base_url(), dr.base_url()] {
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    }
}

#[test]
fn the_crate_depends_on_neither_tephra_cache_nor_tephrad() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for name in ["tephra-cache", "tephrad", "tephractl", "tephra_cache"] {
        // The only mention allowed is in a comment.
        for line in manifest
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
        {
            assert!(!line.contains(name), "Cargo.toml depends on {name}: {line}");
        }
    }
}

#[tokio::test]
async fn f12_listing_a_tree_or_a_folder_reaches_no_blob_media_or_export_route() {
    let creds = Creds::with_token(Provider::Github, "ghp_x");
    let gh = FakeGithub::start().await;
    gh.add_repo("octo/hello", "main");
    for i in 0..5 {
        gh.put_file("octo/hello", "main", &format!("dir/f{i}.md"), b"content");
    }
    let client = GithubClient::new(
        Some(gh.base_url()),
        creds.store.clone(),
        Arc::new(SystemClock),
    )
    .unwrap();
    let repo = RepoRef::parse("octo/hello").unwrap();
    let tree = client.list_tree(&repo, "main").await.unwrap();
    assert_eq!(tree.entries.len(), 6);
    client.get_repo(&repo).await.unwrap();
    client.poll_ref(&repo, "main", None).await.unwrap();
    for r in gh.request_log().all() {
        assert!(
            !r.path.contains("/git/blobs/") && !r.path.contains("/contents/"),
            "listing reached a content route: {}",
            r.target()
        );
    }
    assert!(gh.request_log().count() >= 3);

    let creds = Creds::with_token(Provider::Google, "ya29.x");
    let dr = FakeDrive::start().await;
    dr.add_folder("fld", "Notes", None);
    dr.add_file("f1", "a.md", "text/markdown", Some("fld"), b"a");
    dr.add_doc("d1", "Doc", Some("fld"), "# d");
    dr.set_page_size(1);
    let client = DriveClient::new(
        Some(dr.base_url()),
        creds.store.clone(),
        Arc::new(SystemClock),
    )
    .unwrap();
    let mut token = None;
    let mut listed = 0;
    loop {
        let page = client.list_children("fld", token.as_deref()).await.unwrap();
        listed += page.files.len();
        match page.next_page_token {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    assert_eq!(listed, 2);
    client.get_file("f1").await.unwrap();
    client.start_page_token().await.unwrap();
    client.list_changes("1").await.unwrap();
    for r in dr.request_log().all() {
        let target = r.target();
        assert!(
            !target.contains("alt=media") && !target.contains("/export"),
            "listing reached a media or export route: {target}"
        );
    }
}
