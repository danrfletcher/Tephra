//! PR-10 (E1, E2, G5): what a Drive source does when Drive does not answer
//! well while it is listed, indexed or polled. A rate limit is `syncing`
//! with the retry time, a rejected token is `auth_required`, a server error
//! is `offline`; the listing and the backlinks stay through all of them and
//! the next good answer brings the source back. Real poller against the
//! PR-3 fake Drive on a fake clock.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::DriveDaemon;
use serde_json::{json, Value};
use tephra_remote::testing::{DriveLimit, FakeDrive};

fn tree(d: &FakeDrive) {
    d.add_folder("f", "Notes", None);
    d.add_file("a", "a.md", "text/markdown", Some("f"), b"# a\n");
    d.add_file("b", "b.md", "text/markdown", Some("f"), b"# b\n\n[[a]]\n");
}

fn started() -> DriveDaemon {
    let rig = DriveDaemon::start(tree);
    rig.add_active("g", "Notes");
    rig
}

fn row(rig: &DriveDaemon) -> Value {
    rig.d.source_row("g").unwrap()
}

fn reason(rig: &DriveDaemon) -> String {
    row(rig)["status_reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn backlinks_kept(rig: &DriveDaemon) {
    let back = rig.d.ok("graph.get_backlinks", json!({"id": "@g:a.md"}));
    assert_eq!(back.as_array().unwrap().len(), 1, "{back}");
    assert_eq!(rig.paths("g"), vec!["a.md", "b.md"], "the listing stays");
}

#[test]
fn a_rate_limit_while_polling_is_syncing_with_the_retry_time_and_then_recovers() {
    for style in [
        DriveLimit::Status429 { retry_after: true },
        DriveLimit::Reason403 {
            reason: "userRateLimitExceeded",
            retry_after: true,
        },
        DriveLimit::Reason403 {
            reason: "rateLimitExceeded",
            retry_after: false,
        },
    ] {
        let rig = started();
        let window = Duration::from_secs(300);
        rig.drive.rate_limit_for(window, style);
        rig.drive
            .add_file("n", "n.md", "text/markdown", Some("f"), b"# n\n");
        rig.poll();
        let r = row(&rig);
        assert_eq!(r["status"], json!("syncing"), "{style:?}: {r}");
        let why = reason(&rig);
        assert!(why.contains("rate limited"), "{why}");
        assert!(why.contains(':'), "the time it resumes: {why}");
        backlinks_kept(&rig);
        assert!(!rig.paths("g").contains(&"n.md".to_string()));

        // With a Retry-After, nothing more is sent to Drive inside the
        // window; without one the next ordinary interval tries again.
        let hinted = !matches!(
            style,
            DriveLimit::Reason403 {
                retry_after: false,
                ..
            }
        );
        if hinted {
            let sent = rig.drive.request_log().all().len();
            rig.clock.advance(Duration::from_secs(61));
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(
                rig.drive.request_log().all().len(),
                sent,
                "{style:?}: backed off"
            );
        }

        // Past it, the next poll catches up.
        rig.clock.advance(window);
        rig.d.wait_for("recovered", || {
            (row(&rig)["status"] == json!("active")).then_some(())
        });
        assert!(rig.paths("g").contains(&"n.md".to_string()));
        rig.d.close();
    }
}

#[test]
fn a_rate_limit_during_the_first_index_is_syncing_and_the_index_goes_on_after_it() {
    let rig = DriveDaemon::start(tree);
    rig.drive.hold_content_after(0);
    rig.add("g", "Notes");
    rig.d.wait_for("a read in flight", || {
        (rig.media_reads() >= 1).then_some(())
    });
    rig.drive.rate_limit_for(
        Duration::from_secs(120),
        DriveLimit::Status429 { retry_after: true },
    );
    rig.drive.release_content();
    rig.d.wait_for("syncing", || {
        (row(&rig)["status"] == json!("syncing")).then_some(())
    });
    assert!(reason(&rig).contains("rate limited"), "{}", reason(&rig));
    assert_eq!(rig.paths("g"), vec!["a.md", "b.md"], "listed all along");
    rig.clock.advance(Duration::from_secs(130));
    let done = rig.d.wait_active("g");
    assert_eq!(done["index_done"], json!(2));
    rig.d.close();
}

#[test]
fn a_rate_limit_while_listing_refuses_the_add_with_the_time_and_registers_nothing() {
    let rig = DriveDaemon::start(tree);
    rig.drive.rate_limit_for(
        Duration::from_secs(120),
        DriveLimit::Status429 { retry_after: true },
    );
    let reply = rig.add_raw("g", "Notes");
    assert!(reply.get("error").is_some(), "{reply}");
    assert!(reply.to_string().contains("rate limit"), "{reply}");
    assert!(rig.d.source_row("g").is_none());
    // The start token was stored before the listing; the refusal took it,
    // and the half-made source, away again.
    let cache = tephra_cache::Cache::open_read_only(&rig.d.cache_path).unwrap();
    assert!(cache.remote_source("g").unwrap().is_none());
    assert!(cache.remote_cursor("g").unwrap().is_none());
    assert!(cache.remote_items("g").unwrap().is_empty());
    assert!(cache.get_source("g").unwrap().is_none());
    drop(cache);
    rig.d.close();
}

#[test]
fn a_rejected_token_is_auth_required_and_the_listing_and_backlinks_stay() {
    let rig = started();
    rig.drive.revoke_token();
    rig.poll();
    let r = row(&rig);
    assert_eq!(r["status"], json!("auth_required"), "{r}");
    let why = reason(&rig);
    assert!(why.contains("tephractl auth google"), "{why}");
    backlinks_kept(&rig);
    // No credential in anything the daemon says.
    let everything = format!("{r} {}", rig.d.ok("source.list", json!({})));
    assert!(!everything.contains("Bearer"), "{everything}");
    // Signing in again brings it back at the next poll.
    rig.drive.accept_only_token("ya29.SECOND_sentinel");
    rig.sign_in_again("ya29.SECOND_sentinel");
    rig.drive
        .add_file("n", "n.md", "text/markdown", Some("f"), b"# n\n");
    rig.poll();
    assert_eq!(row(&rig)["status"], json!("active"));
    assert!(rig.paths("g").contains(&"n.md".to_string()));
    rig.d.close();
}

#[test]
fn a_rejected_token_during_the_first_index_is_auth_required_and_keeps_the_listing() {
    let rig = DriveDaemon::start(tree);
    rig.drive.hold_content_after(0);
    rig.add("g", "Notes");
    rig.d.wait_for("a read in flight", || {
        (rig.media_reads() >= 1).then_some(())
    });
    rig.drive.revoke_token();
    rig.drive.release_content();
    rig.d.wait_for("auth_required", || {
        (row(&rig)["status"] == json!("auth_required")).then_some(())
    });
    assert_eq!(rig.paths("g"), vec!["a.md", "b.md"]);
    rig.d.close();
}

#[test]
fn a_server_error_is_offline_then_the_next_good_poll_brings_it_back() {
    let rig = started();
    let before = rig.d.source_row("g").unwrap();
    rig.drive.fail_next(1, 503);
    rig.poll();
    let r = row(&rig);
    assert_eq!(r["status"], json!("offline"), "{r}");
    assert!(reason(&rig).contains("Google Drive"), "{}", reason(&rig));
    backlinks_kept(&rig);
    rig.poll();
    let r = row(&rig);
    assert_eq!(r["status"], json!("active"), "{r}");
    assert_eq!(r["index_done"], before["index_done"]);
    rig.d.close();
}

#[test]
fn a_server_error_on_one_file_during_a_poll_leaves_it_listed_and_a_later_poll_reads_it() {
    let rig = started();
    rig.drive
        .add_file("x", "x.md", "text/markdown", Some("f"), b"# x\n");
    rig.drive
        .add_file("y", "y.md", "text/markdown", Some("f"), b"# y\n\n[[a]]\n");
    // Whichever of the two is read first fails: it stays listed and unread.
    rig.drive.fail_content_reads(1, 500);
    rig.poll();
    assert_eq!(
        rig.paths("g"),
        vec!["a.md", "b.md", "x.md", "y.md"],
        "both listed"
    );
    assert_eq!(row(&rig)["status"], json!("active"));
    rig.d.wait_for("the unread one is read again", || {
        let r = row(&rig);
        (r["index_done"] == r["index_total"]).then_some(())
    });
    let row = row(&rig);
    assert_eq!(row["index_total"], json!(4), "{row}");
    rig.d.close();
}

#[test]
fn a_rate_limit_in_the_middle_of_a_poll_keeps_the_old_position_and_the_page_replays() {
    let rig = started();
    let cursor = |rig: &DriveDaemon| {
        tephra_cache::Cache::open_read_only(&rig.d.cache_path)
            .unwrap()
            .remote_poll_state("g")
            .unwrap()
            .and_then(|s| s.commit)
    };
    let before = cursor(&rig);
    rig.drive
        .add_file("x", "x.md", "text/markdown", Some("f"), b"# x\n");
    rig.drive
        .add_file("y", "y.md", "text/markdown", Some("f"), b"# y\n");
    // The feed answers; the read of a new file is rate limited.
    rig.drive.fail_content_reads(1, 429);
    rig.poll();
    assert_eq!(row(&rig)["status"], json!("syncing"), "{}", row(&rig));
    assert_eq!(
        cursor(&rig),
        before,
        "the position only moves with the whole poll"
    );
    assert_eq!(
        rig.paths("g"),
        vec!["a.md", "b.md"],
        "nothing of the page was applied"
    );
    // The same page comes again once the limit has passed.
    rig.clock.advance(Duration::from_secs(400));
    rig.d.wait_for("recovered", || {
        (row(&rig)["status"] == json!("active")).then_some(())
    });
    assert_eq!(rig.paths("g"), vec!["a.md", "b.md", "x.md", "y.md"]);
    assert_ne!(cursor(&rig), before);
    rig.d.close();
}
