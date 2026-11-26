//! `tests/cli/source_commands.test` (G8/R4): `source add`/`remove`/
//! `list` and `status` against the mock RPC server. Rendered and
//! `--json` output is asserted byte-for-byte (`assert_eq!`) against
//! golden fixtures under `tests/fixtures/expected/` — not a
//! substring/`contains` check — per the spec's "asserting rendered
//! table output and `--json` output byte-for-byte against fixtures"
//! (R4), alongside the exact method/params sent and the exit code.

use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{load_expected, load_expected_templated, load_fixture, Behavior, MockServer};

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

#[test]
fn source_add_sends_register_with_name_and_path_and_renders_stats() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_add_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(received[0].body["method"], "source.register");
    assert_eq!(received[0].body["params"]["name"], "notes");
    assert_eq!(received[0].body["params"]["root"], "/home/dan/Notes");
}

#[test]
fn source_add_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_add_json"));
}

#[test]
fn source_add_duplicate_name_error_is_reported_clearly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_register_duplicate_error"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        "tephractl: a source named \"notes\" is already registered\n"
    );
    assert_eq!(outcome.stdout, "");
}

#[test]
fn source_remove_sends_list_then_unregister_and_confirms_removal() {
    // T1: source_remove now checks client-side via `source.list`
    // before calling `source.unregister`, so the mock server needs
    // two scripted responses — "notes" exists in the list, so the
    // unregister call follows.
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_list_two_sources"),
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_success"),
        },
    ]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "notes".to_string(),
            force: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_remove_rendered"));
    let received = server.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].body["method"], "source.list");
    assert_eq!(received[1].body["method"], "source.unregister");
    assert_eq!(received[1].body["params"]["name"], "notes");
}

#[test]
fn source_remove_on_an_unknown_name_reports_not_found_without_calling_unregister() {
    // T1: tephrad's `source.unregister` is idempotent and can never
    // itself report "not found", so `source remove` must check
    // client-side against `source.list` and never even issue the
    // `source.unregister` RPC for a name that isn't registered.
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "ghost".to_string(),
            force: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, "source \"ghost\" not found\n");
    assert_eq!(outcome.stdout, "");
    let received = server.received();
    assert_eq!(
        received.len(),
        1,
        "must not call source.unregister for an unknown name"
    );
    assert_eq!(received[0].body["method"], "source.list");
}

#[test]
fn source_list_renders_a_table_with_offline_marker_exactly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_list_rendered"));
    assert_eq!(server.received()[0].body["method"], "source.list");
}

#[test]
fn source_list_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_list_json"));
}

#[test]
fn source_list_with_zero_sources_says_so_plainly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_empty"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "no sources registered\n");
}

#[test]
fn status_reports_daemon_address_and_source_table_without_fabricating_uptime() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected_templated("status_rendered.template", &server.addr)
    );
}

#[test]
fn status_json_flag_prints_address_uptime_and_sources_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_two_sources"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected_templated("status_json.template", &server.addr)
    );
}

#[test]
fn status_with_zero_sources_says_so_plainly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_empty"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::Status, false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.stdout.contains("no sources registered"));
}

// ---- source promote (G11, E13) ------------------------------------

fn promote_command() -> Command {
    Command::SourcePromote {
        name: "tephrad".to_string(),
        path: "/home/dan/Developer/Tephra/crates/tephrad".to_string(),
    }
}

#[test]
fn source_promote_sends_name_and_root_and_renders_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_promote_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&promote_command(), false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_promote_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(
        received.len(),
        1,
        "one call: the promote, nothing before it"
    );
    assert_eq!(received[0].body["method"], "source.promote");
    assert_eq!(received[0].body["params"]["name"], "tephrad");
    assert_eq!(
        received[0].body["params"]["root"],
        "/home/dan/Developer/Tephra/crates/tephrad"
    );
    assert_eq!(received[0].body["params"].as_object().unwrap().len(), 2);
}

#[test]
fn source_promote_json_flag_prints_the_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_promote_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&promote_command(), true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_promote_json"));
}

#[test]
fn source_promote_says_when_a_link_fix_is_still_waiting() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_promote_partial"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&promote_command(), false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_promote_partial_rendered")
    );
}

#[test]
fn source_promote_resolves_a_relative_path_against_the_callers_directory() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_promote_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourcePromote {
            name: "tephrad".to_string(),
            path: "crates/tephrad".to_string(),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0);
    let sent = server.received()[0].body["params"]["root"]
        .as_str()
        .unwrap()
        .to_string();
    let want = std::env::current_dir().unwrap().join("crates/tephrad");
    assert_eq!(sent, want.display().to_string());
}

#[test]
fn source_promote_refusals_exit_non_zero_and_show_the_reason_verbatim() {
    for (reason, message) in [
        (
            "not_inside_a_source",
            "/home/dan/Documents/Notes is not inside any registered source",
        ),
        (
            "name_taken",
            "the name \"Tephra\" is taken by the source \"tephra\"",
        ),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: load_fixture(&format!("source_promote_refused_{reason}")),
        }]);
        let client = client_for(&server);

        let mut stdin: &[u8] = &[];
        let outcome = commands::execute(&promote_command(), false, &client, &mut stdin);

        assert_eq!(outcome.exit_code, 1, "{reason}");
        assert_eq!(
            outcome.stderr,
            format!("tephractl: {message} (reason: {reason})\n"),
            "{reason}"
        );
        assert_eq!(outcome.stdout, "", "{reason}");
    }
}

// ---- source unpromote (G12) ---------------------------------------

fn unpromote_command() -> Command {
    Command::SourceUnpromote {
        name: "tephrad".to_string(),
    }
}

#[test]
fn source_unpromote_sends_only_the_name_and_renders_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_unpromote_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&unpromote_command(), false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_unpromote_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(received.len(), 1, "one call: the unpromote, no list first");
    assert_eq!(received[0].body["method"], "source.unpromote");
    assert_eq!(received[0].body["params"]["name"], "tephrad");
    assert_eq!(received[0].body["params"].as_object().unwrap().len(), 1);
}

#[test]
fn source_unpromote_json_flag_prints_the_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_unpromote_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&unpromote_command(), true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_unpromote_json"));
}

#[test]
fn source_unpromote_says_when_a_link_fix_is_still_waiting() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_unpromote_partial"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&unpromote_command(), false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_unpromote_partial_rendered")
    );
}

#[test]
fn source_unpromote_refusals_exit_non_zero_and_show_the_reason_verbatim() {
    for (reason, message) in [
        (
            "not_promoted",
            "the source \"tephra\" was not promoted out of another",
        ),
        ("source_offline", "the source \"tephra\" is offline"),
        ("unknown_source", "no source named \"ghost\""),
    ] {
        let server = MockServer::start(vec![Behavior::Respond {
            status: 200,
            body: load_fixture(&format!("source_unpromote_refused_{reason}")),
        }]);
        let client = client_for(&server);

        let mut stdin: &[u8] = &[];
        let outcome = commands::execute(&unpromote_command(), false, &client, &mut stdin);

        assert_eq!(outcome.exit_code, 1, "{reason}");
        assert_eq!(
            outcome.stderr,
            format!("tephractl: {message} (reason: {reason})\n"),
            "{reason}"
        );
        assert_eq!(outcome.stdout, "", "{reason}");
    }
}

// ---- source rename (G13, G14) -------------------------------------

fn rename_command() -> Command {
    Command::SourceRename {
        old: "tephrad".to_string(),
        new: "daemon".to_string(),
    }
}

#[test]
fn source_rename_sends_exactly_the_two_names_and_renders_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_rename_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&rename_command(), false, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_rename_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(received.len(), 1, "one call: the rename, no list first");
    assert_eq!(received[0].body["method"], "source.rename");
    assert_eq!(
        received[0].body["params"],
        serde_json::json!({"name": "tephrad", "new_name": "daemon"})
    );
}

#[test]
fn source_rename_json_flag_prints_the_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_rename_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&rename_command(), true, &client, &mut stdin);

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_rename_json"));
}

#[test]
fn source_rename_says_when_a_link_fix_is_still_waiting_and_when_nothing_was_done() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_rename_partial"),
    }]);
    let client = client_for(&server);
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&rename_command(), false, &client, &mut stdin);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_rename_partial_rendered")
    );

    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_rename_unchanged"),
    }]);
    let client = client_for(&server);
    let same = Command::SourceRename {
        old: "daemon".to_string(),
        new: "daemon".to_string(),
    };
    let outcome = commands::execute(&same, false, &client, &mut stdin);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_rename_unchanged_rendered")
    );
}

#[test]
fn source_rename_refusals_exit_non_zero_and_show_the_sentence_and_reason_verbatim() {
    for (reason, message) in [
        (
            "invalid_name",
            "\"a:b\" is not a valid source name: it must not contain any of / \\ @ : # ^ | [ ] *",
        ),
        (
            "name_taken",
            "the name \"notes\" is taken by the source \"notes\"",
        ),
        ("unknown_source", "there is no source named \"ghost\""),
        (
            "source_offline",
            "the source \"tephrad\" is offline, so its links cannot be rewritten",
        ),
    ] {
        for json in [false, true] {
            let server = MockServer::start(vec![Behavior::Respond {
                status: 200,
                body: load_fixture(&format!("source_rename_refused_{reason}")),
            }]);
            let client = client_for(&server);
            let mut stdin: &[u8] = &[];
            let outcome = commands::execute(&rename_command(), json, &client, &mut stdin);
            assert_eq!(outcome.exit_code, 1, "{reason} json={json}");
            assert_eq!(
                outcome.stderr,
                format!("tephractl: {message} (reason: {reason})\n"),
                "{reason} json={json}"
            );
            assert_eq!(outcome.stdout, "", "{reason} json={json}");
        }
    }
}

#[test]
fn source_remove_of_a_promoted_name_still_lists_first_then_unregisters() {
    let list = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
        {"name": "tephra", "root": "/r", "status": "online", "files": 5},
        {"name": "tephrad", "root": "/r/crates/tephrad", "status": "online", "files": 12},
    ]});
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: list,
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_success"),
        },
    ]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "tephrad".to_string(),
            force: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let received = server.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].body["method"], "source.list");
    assert_eq!(received[1].body["method"], "source.unregister");
    assert_eq!(received[1].body["params"]["name"], "tephrad");
}

// ---- PR-6: a remote source the poller could not keep current -------------

/// `{SYNC}` in an expected file stands for the machine's local reading of
/// 1_700_000_000 (the fixture's last sync), so the golden text does not
/// depend on the time zone.
fn with_sync(expected: String) -> String {
    expected.replace("{SYNC}", &tephractl::render::local_time(1_700_000_000))
}

fn list_remote_health(json: bool) -> (commands::CommandOutcome, MockServer) {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_remote_health"),
    }]);
    let client = client_for(&server);
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(&Command::SourceList, json, &client, &mut stdin);
    (outcome, server)
}

#[test]
fn source_list_shows_offline_syncing_auth_required_and_needs_attention_with_the_reason() {
    let (outcome, server) = list_remote_health(false);
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        with_sync(load_expected("source_list_remote_health_rendered"))
    );
    assert_eq!(outcome.stderr, "");
    assert_eq!(server.received()[0].body["method"], "source.list");
}

#[test]
fn source_list_text_says_why_for_each_unhealthy_status() {
    let (outcome, _server) = list_remote_health(false);
    let text = outcome.stdout;
    for (status, reason) in [
        ("offline", "note: cannot reach GitHub (connection failed)"),
        (
            "syncing",
            "note: rate limited by GitHub; polling resumes at 14:05 UTC",
        ),
        (
            "auth_required",
            "note: GitHub rejected the token: run `tephractl auth github`",
        ),
        (
            "needs_attention",
            "note: danrfletcher/old-name has moved to https://api.github.com/repositories/9",
        ),
    ] {
        assert!(text.contains(status), "{status} in:\n{text}");
        assert!(text.contains(reason), "{reason} in:\n{text}");
    }
    // The last sync of a source that has one, and `never` for one that has not.
    assert!(text.contains(&format!(
        "last sync {}",
        tephractl::render::local_time(1_700_000_000)
    )));
    assert!(text.contains("last sync never"));
    // Every line is clean: no trailing spaces, no control characters.
    for line in text.lines() {
        assert_eq!(line, line.trim_end(), "{line:?}");
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
}

#[test]
fn source_list_json_is_the_daemons_answer_byte_for_byte_and_carries_no_token() {
    let (outcome, _server) = list_remote_health(true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_list_remote_health_json")
    );
    // `client_for` dials with the token "test-token": it is on the wire, and
    // nowhere in what the command prints.
    for out in [&outcome.stdout, &outcome.stderr] {
        assert!(!out.contains("test-token"), "{out}");
        assert!(!out.contains("Bearer"), "{out}");
    }
    let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
    for row in parsed.as_array().unwrap() {
        let keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            keys.iter()
                .all(|k| !k.to_ascii_lowercase().contains("token")),
            "no token field: {keys:?}"
        );
    }
}

// ---- PR-9: a clone's changes not pushed, and how far behind it is ---------

fn list_clone(json: bool) -> commands::CommandOutcome {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("source_list_clone"),
    }]);
    let client = client_for(&server);
    let mut stdin: &[u8] = &[];
    commands::execute(&Command::SourceList, json, &client, &mut stdin)
}

#[test]
fn source_list_renders_changes_not_pushed_and_behind_and_nothing_for_zero() {
    let outcome = list_clone(false);
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        with_sync(load_expected("source_list_clone_rendered"))
    );
    let text = outcome.stdout;
    // One is singular, several are plural, zero prints no line at all.
    assert!(
        text.contains("github-atlas: 1 change not pushed\n"),
        "{text}"
    );
    assert!(!text.contains("1 changes"), "{text}");
    assert!(text.contains("gh-many: 3 changes not pushed\n"), "{text}");
    assert!(text.contains("gh-many: behind by 2\n"), "{text}");
    assert!(!text.contains("gh-clean: "), "{text}");
    assert!(!text.contains("github-atlas: behind"), "{text}");
    // The reason of a rewritten history shows under its source, and points
    // at Dan's own command.
    assert!(text.contains("note: history rewritten on GitHub"), "{text}");
    assert!(text.contains("`git pull --rebase`"), "{text}");
    for line in text.lines() {
        assert_eq!(line, line.trim_end(), "{line:?}");
    }
}

#[test]
fn source_list_json_carries_the_counts_byte_for_byte() {
    let outcome = list_clone(true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_list_clone_json"));
    let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout).unwrap();
    let rows = parsed.as_array().unwrap();
    assert_eq!(rows[0]["not_pushed"], 1);
    assert_eq!(rows[1]["not_pushed"], 3);
    assert_eq!(rows[1]["behind"], 2);
    assert_eq!(rows[3]["status"], "needs_attention");
    let reason = rows[3]["status_reason"].as_str().unwrap();
    assert!(reason.starts_with("history rewritten on GitHub"));
    assert!(reason.contains("git pull --rebase"), "{reason}");
}

fn remove_g(
    force: bool,
    json: bool,
    second: Behavior,
) -> (commands::CommandOutcome, Vec<serde_json::Value>) {
    let list = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
        {"name": "g", "root": "/r", "status": "active", "kind": "github", "files": 5},
    ]});
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: list,
        },
        second,
    ]);
    let client = client_for(&server);
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::SourceRemove {
            name: "g".to_string(),
            force,
        },
        json,
        &client,
        &mut stdin,
    );
    let sent = server.received().into_iter().map(|r| r.body).collect();
    (outcome, sent)
}

#[test]
fn source_remove_refused_for_unsent_work_prints_the_counts_and_the_force_hint() {
    let (outcome, sent) = remove_g(
        false,
        false,
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_unsent_work"),
        },
    );
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        load_expected("source_remove_refused_rendered")
    );
    assert!(
        sent[1]["params"].get("force").is_none(),
        "no force unless asked"
    );
}

#[test]
fn source_remove_refused_json_is_one_object_on_stdout_with_exit_1() {
    let (outcome, _) = remove_g(
        false,
        true,
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_unsent_work"),
        },
    );
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.stdout, load_expected("source_remove_refused_json"));
}

#[test]
fn source_remove_force_sends_the_json_boolean_true() {
    let (outcome, sent) = remove_g(
        true,
        false,
        Behavior::Respond {
            status: 200,
            body: load_fixture("source_unregister_success"),
        },
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "Removed source \"g\"\n");
    assert_eq!(sent[1]["method"], "source.unregister");
    assert_eq!(
        sent[1]["params"],
        serde_json::json!({"name": "g", "force": true})
    );
}

#[test]
fn source_remove_prints_only_the_kinds_that_hold_something() {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {
        "code": -32011,
        "message": "x",
        "data": {"reason": "unsent_work", "not_pushed": 1, "pending_upload": 0, "needs_attention": 0}
    }});
    let (outcome, _) = remove_g(false, false, Behavior::Respond { status: 200, body });
    assert_eq!(outcome.exit_code, 1);
    assert!(
        outcome.stderr.contains("  1 not pushed\n"),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("pending upload"));
    assert!(!outcome.stderr.contains("needs attention"));
}

#[test]
fn source_remove_other_refusals_still_read_as_errors() {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {
        "code": -32011, "message": "could not delete the files Tephra keeps for \"g\" (permission denied); the source is still registered, try again"
    }});
    let (outcome, _) = remove_g(true, false, Behavior::Respond { status: 200, body });
    assert_eq!(outcome.exit_code, 1);
    assert!(
        outcome.stderr.starts_with("tephractl: could not delete"),
        "{}",
        outcome.stderr
    );
}
