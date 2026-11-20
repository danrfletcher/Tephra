//! The base-URL rule: defaults, normalisation, loopback-only, bad URLs,
//! at the pure function and at both real constructors.

use std::sync::Arc;

use tephra_remote::base_url::{
    resolve_base_url, BaseUrlError, DRIVE_DEFAULT_BASE, GITHUB_DEFAULT_BASE,
};
use tephra_remote::clock::SystemClock;
use tephra_remote::drive::DriveClient;
use tephra_remote::github::GithubClient;

use crate::common::Creds;

#[test]
fn the_defaults_are_the_real_endpoints() {
    assert_eq!(GITHUB_DEFAULT_BASE, "https://api.github.com");
    assert_eq!(DRIVE_DEFAULT_BASE, "https://www.googleapis.com/drive/v3");
    assert_eq!(
        resolve_base_url(None, GITHUB_DEFAULT_BASE).unwrap(),
        "https://api.github.com"
    );
    assert_eq!(
        resolve_base_url(None, DRIVE_DEFAULT_BASE).unwrap(),
        "https://www.googleapis.com/drive/v3"
    );
}

#[test]
fn a_trailing_slash_is_normalised_on_the_default_and_on_loopback_bases() {
    let cases = [
        (
            "https://api.github.com/",
            GITHUB_DEFAULT_BASE,
            "https://api.github.com",
        ),
        (
            "https://api.github.com//",
            GITHUB_DEFAULT_BASE,
            "https://api.github.com",
        ),
        (
            "https://www.googleapis.com/drive/v3/",
            DRIVE_DEFAULT_BASE,
            "https://www.googleapis.com/drive/v3",
        ),
        (
            "http://127.0.0.1:8080/",
            GITHUB_DEFAULT_BASE,
            "http://127.0.0.1:8080",
        ),
        (
            "http://127.0.0.1:8080/drive/v3/",
            DRIVE_DEFAULT_BASE,
            "http://127.0.0.1:8080/drive/v3",
        ),
        (
            "http://localhost:9/",
            GITHUB_DEFAULT_BASE,
            "http://localhost:9",
        ),
        ("http://[::1]:9/x/", GITHUB_DEFAULT_BASE, "http://[::1]:9/x"),
        (
            "https://127.0.0.1:9",
            GITHUB_DEFAULT_BASE,
            "https://127.0.0.1:9",
        ),
    ];
    for (given, default, want) in cases {
        assert_eq!(
            resolve_base_url(Some(given), default).unwrap(),
            want,
            "{given}"
        );
    }
}

#[test]
fn a_non_default_base_is_accepted_only_for_a_loopback_host() {
    for url in [
        "https://ghe.example.com",
        "https://example.com/api",
        "http://example.com",
        "http://10.0.0.5:3000",
        "http://0.0.0.0:80",
        "http://127.0.0.1.evil.example",
        "http://localhost.evil.example",
        "https://api.github.com.evil.example",
        "https://api.github.co",
        "http://2130706433",
        "http://[fe80::1]",
    ] {
        assert!(
            matches!(
                resolve_base_url(Some(url), GITHUB_DEFAULT_BASE),
                Err(BaseUrlError::NotLoopback) | Err(BaseUrlError::Malformed(_))
            ),
            "{url} must be refused"
        );
        assert!(
            resolve_base_url(Some(url), DRIVE_DEFAULT_BASE).is_err(),
            "{url}"
        );
    }
    // The host rule, not the scheme: loopback over either scheme is fine.
    for url in [
        "http://127.0.0.1:1",
        "https://127.0.0.1:1",
        "http://localhost",
        "http://[::1]:1",
    ] {
        assert!(
            resolve_base_url(Some(url), GITHUB_DEFAULT_BASE).is_ok(),
            "{url}"
        );
    }
}

#[test]
fn a_bad_url_is_refused_without_echoing_any_part_of_it() {
    for url in [
        "",
        "not a url",
        "ftp://127.0.0.1",
        "file:///etc/passwd",
        "http://user:ghp_SENTINEL@127.0.0.1",
        "http://127.0.0.1:99999",
        "http://127.0.0.1/?token=ghp_SENTINEL",
        "http://127.0.0.1/#ghp_SENTINEL",
        "http://",
    ] {
        let err = resolve_base_url(Some(url), GITHUB_DEFAULT_BASE).expect_err(url);
        let text = format!("{err} {err:?}");
        assert!(!text.contains("SENTINEL"), "{url}: {text}");
        assert!(url.is_empty() || !text.contains(url), "{url}: {text}");
    }
    assert_eq!(
        resolve_base_url(Some("http://127.0.0.1/?x=1"), GITHUB_DEFAULT_BASE),
        Err(BaseUrlError::QueryOrFragment)
    );
}

#[test]
fn both_real_constructors_apply_the_rule_and_report_the_base_they_use() {
    let creds = Creds::new();
    let clock = Arc::new(SystemClock);

    let gh = GithubClient::new(None, creds.store.clone(), clock.clone()).unwrap();
    assert_eq!(gh.base_url(), "https://api.github.com");
    let dr = DriveClient::new(None, creds.store.clone(), clock.clone()).unwrap();
    assert_eq!(dr.base_url(), "https://www.googleapis.com/drive/v3");

    let gh = GithubClient::new(
        Some("http://127.0.0.1:5/"),
        creds.store.clone(),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(gh.base_url(), "http://127.0.0.1:5");
    let dr = DriveClient::new(
        Some("http://localhost:5/drive/v3/"),
        creds.store.clone(),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(dr.base_url(), "http://localhost:5/drive/v3");

    assert_eq!(
        GithubClient::new(
            Some("https://evil.example"),
            creds.store.clone(),
            clock.clone()
        )
        .unwrap_err(),
        BaseUrlError::NotLoopback
    );
    assert_eq!(
        DriveClient::new(
            Some("https://evil.example/drive/v3"),
            creds.store.clone(),
            clock.clone()
        )
        .unwrap_err(),
        BaseUrlError::NotLoopback
    );
}
