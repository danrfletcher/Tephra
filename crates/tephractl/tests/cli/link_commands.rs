//! `tests/cli/link_commands.test` (G8/GP5/E4/R4/R5): `link resolve`/
//! `link refactor` against the mock RPC server — correct method and
//! params, GP5's cross-source distinction, the ambiguous-candidate
//! rendering, the `--apply`/`--yes` confirmation gate (asserting
//! exactly which RPCs happen before/after confirmation, R3), the
//! preview/apply rendering, and (R5) the offline-source marker and
//! unicode-wikilink round trip at the wire level. Rendered and
//! `--json` output, and exit codes, are asserted byte-for-byte
//! (`assert_eq!`) against golden fixtures under
//! `tests/fixtures/expected/` (R4) rather than `contains` checks.

use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{err_envelope, load_expected, load_fixture, Behavior, MockServer};

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

#[test]
fn link_resolve_sends_the_link_and_default_source_params() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_cross_source"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[@atlas-repo:README]]".to_string(),
            default_source: Some("notes".to_string()),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let sent = server.received();
    assert_eq!(sent[0].body["method"], "unit.resolve_link");
    assert_eq!(sent[0].body["params"]["link"], "[[@atlas-repo:README]]");
    assert_eq!(sent[0].body["params"]["default_source"], "notes");
}

#[test]
fn link_resolve_cross_source_renders_exactly() {
    // GP5: resolving a federated link from "notes" into "atlas-repo"
    // must visibly distinguish the two, not just print one undifferentiated
    // "source" field.
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_cross_source"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[@atlas-repo:README]]".to_string(),
            default_source: Some("notes".to_string()),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("resolve_cross_source_rendered")
    );
}

#[test]
fn link_resolve_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_cross_source"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[@atlas-repo:README]]".to_string(),
            default_source: Some("notes".to_string()),
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("resolve_cross_source_json"));
}

#[test]
fn link_resolve_without_default_source_sends_no_default_source_param() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_cross_source"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[@atlas-repo:README]]".to_string(),
            default_source: None,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0);
    let sent = server.received();
    assert!(sent[0].body["params"].get("default_source").is_none());
}

#[test]
fn link_resolve_ambiguous_renders_exactly_and_exits_nonzero() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_ambiguous"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[guide]]".to_string(),
            default_source: Some("vault".to_string()),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, load_expected("resolve_ambiguous_rendered"));
    assert_eq!(outcome.stdout, "");
}

#[test]
fn link_resolve_ambiguous_json_flag_prints_the_error_envelope_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_ambiguous"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[guide]]".to_string(),
            default_source: Some("vault".to_string()),
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, load_expected("resolve_ambiguous_json"));
}

#[test]
fn link_resolve_unresolved_renders_exactly_and_exits_nonzero() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_unresolved"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[missing]]".to_string(),
            default_source: Some("vault".to_string()),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, load_expected("resolve_unresolved_rendered"));
}

/// R5: an offline-source fixture response (`unit.offline` and
/// `edge.offline` both `true`) must render both the unit-level
/// "offline: yes" line and the edge-level "(offline)" marker.
#[test]
fn link_resolve_offline_source_renders_offline_markers_exactly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_offline"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: "[[Other]]".to_string(),
            default_source: Some("old-vault".to_string()),
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("resolve_offline_rendered"));
    assert!(outcome.stdout.contains("offline: yes"));
    assert!(outcome.stdout.contains("(offline)"));
}

/// R5: a unicode wikilink target (`[[@notes:Café Notes]]`) must be
/// sent to the daemon byte-for-byte untouched (not normalized,
/// percent-encoded, or otherwise mangled), and the resolved unicode
/// path must render correctly in the default (non-`--json`) output.
#[test]
fn link_resolve_unicode_target_round_trips_on_the_wire_and_in_output() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("resolve_success_unicode"),
    }]);
    let client = client_for(&server);
    let target = "[[@notes:Café Notes]]";

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkResolve {
            target: target.to_string(),
            default_source: None,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(server.received()[0].body["params"]["link"], target);
    assert_eq!(outcome.stdout, load_expected("resolve_unicode_rendered"));
    assert!(outcome.stdout.contains("Café Notes.md"));
}

#[test]
fn link_refactor_preview_sends_source_old_new_params_and_renders_exactly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("preview_refactor_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: false,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let sent = server.received();
    assert_eq!(sent[0].body["method"], "link.preview_refactor");
    assert_eq!(sent[0].body["params"]["source"], "notes");
    assert_eq!(sent[0].body["params"]["old_path"], "OldDoc.md");
    assert_eq!(sent[0].body["params"]["new_path"], "NewDoc.md");
    assert_eq!(outcome.stdout, load_expected("refactor_preview_rendered"));
}

#[test]
fn link_refactor_preview_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("preview_refactor_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: false,
            yes: false,
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("refactor_preview_json"));
}

/// R5: a unicode `--old`/`--new` path (`Café Notes.md`) must be sent
/// to the daemon byte-for-byte untouched, and must render correctly
/// (not mangled/escaped) in the preview diff.
#[test]
fn link_refactor_preview_unicode_old_path_round_trips_on_the_wire_and_in_output() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("preview_refactor_unicode"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "Café Notes.md".to_string(),
            new_path: "Café Notes Renamed.md".to_string(),
            apply: false,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let sent = server.received();
    assert_eq!(sent[0].body["params"]["old_path"], "Café Notes.md");
    assert_eq!(sent[0].body["params"]["new_path"], "Café Notes Renamed.md");
    assert_eq!(
        outcome.stdout,
        load_expected("refactor_preview_unicode_rendered")
    );
}

#[test]
fn link_refactor_apply_with_yes_skips_the_confirmation_prompt_and_renders_exactly() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("apply_refactor_success"),
    }]);
    let client = client_for(&server);

    // Empty stdin: if `execute` tried to read a confirmation line
    // despite `--yes`, `read_line` on empty input returns EOF (which
    // `confirm` treats as "no"), so this would fail the exit-code
    // assertion below if the `--yes` bypass were broken.
    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(server.received().len(), 1, "--yes issues exactly one RPC");
    assert_eq!(server.received()[0].body["method"], "link.apply_refactor");
    assert_eq!(outcome.stdout, load_expected("refactor_apply_rendered"));
}

#[test]
fn link_refactor_apply_with_yes_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("apply_refactor_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("refactor_apply_json"));
}

#[test]
fn link_refactor_apply_without_yes_prompts_and_a_refusal_makes_no_apply_rpc_call() {
    // R3: the interactive path fetches a preview to show the user
    // before asking for confirmation, so one request is expected (the
    // preview) — but a refusal must still make zero `apply_refactor`
    // calls.
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("preview_refactor_success"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = b"n\n".as_ref();
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, "aborted: refactor not confirmed\n");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        server.call_count(),
        1,
        "only the preview call, never apply_refactor, on a refused confirmation"
    );
    assert_eq!(server.received()[0].body["method"], "link.preview_refactor");
}

#[test]
fn link_refactor_apply_without_yes_confirmed_with_y_does_call_preview_then_apply() {
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: load_fixture("preview_refactor_success"),
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("apply_refactor_success"),
        },
    ]);
    let client = client_for(&server);

    let mut stdin: &[u8] = b"y\n".as_ref();
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let received = server.received();
    assert_eq!(received.len(), 2, "a preview call, then the apply call");
    assert_eq!(received[0].body["method"], "link.preview_refactor");
    assert_eq!(received[1].body["method"], "link.apply_refactor");
    assert_eq!(outcome.stdout, load_expected("refactor_apply_rendered"));
}

/// R2/R3: the confirmation prompt for `--apply` without `--yes`
/// names the source and old/new paths and shows the preview diff —
/// and (R2) is written to the caller-supplied writer, never folded
/// into stdout, so `--json` output stays parseable even when a
/// confirmation was shown first.
#[test]
fn link_refactor_apply_without_yes_prompt_names_paths_and_shows_preview_diff() {
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: load_fixture("preview_refactor_success"),
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("apply_refactor_success"),
        },
    ]);
    let client = client_for(&server);

    let mut stdin: &[u8] = b"y\n".as_ref();
    let mut prompt_out: Vec<u8> = Vec::new();
    let outcome = commands::execute_with_prompt_writer(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
        &mut prompt_out,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    let prompt = String::from_utf8(prompt_out).expect("prompt is valid utf8");
    assert!(prompt.contains("notes"), "prompt must name the source");
    assert!(prompt.contains("OldDoc.md"), "prompt must name old_path");
    assert!(prompt.contains("NewDoc.md"), "prompt must name new_path");
    assert!(
        prompt.contains("@notes:Journal.md:12"),
        "prompt must show the affected file/line from the preview"
    );
    assert!(prompt.contains("- [[OldDoc]]"));
    assert!(prompt.contains("+ [[NewDoc]]"));
    assert!(prompt.contains("Apply this refactor? [y/N]: "));
    // The prompt must never leak into stdout, which holds only the
    // final applied-refactor rendering.
    assert!(!outcome.stdout.contains("Apply this refactor"));
}

/// R2: `--apply --json` without `--yes` must still produce stdout
/// that is valid, parseable JSON — the interactive confirmation
/// prompt (and the preview diff shown ahead of it) must go to the
/// prompt writer, never interleaved into stdout.
#[test]
fn link_refactor_apply_json_without_yes_keeps_stdout_as_valid_json() {
    let server = MockServer::start(vec![
        Behavior::Respond {
            status: 200,
            body: load_fixture("preview_refactor_success"),
        },
        Behavior::Respond {
            status: 200,
            body: load_fixture("apply_refactor_success"),
        },
    ]);
    let client = client_for(&server);

    let mut stdin: &[u8] = b"y\n".as_ref();
    let mut prompt_out: Vec<u8> = Vec::new();
    let outcome = commands::execute_with_prompt_writer(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: false,
        },
        true,
        &client,
        &mut stdin,
        &mut prompt_out,
    );

    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("refactor_apply_json"));
    let parsed: serde_json::Value =
        serde_json::from_str(&outcome.stdout).expect("stdout must be valid JSON end to end");
    assert_eq!(parsed["status"], "applied");
    // The confirmation prompt happened, but on the separate writer —
    // not mixed into the JSON stdout.
    let prompt = String::from_utf8(prompt_out).expect("prompt is valid utf8");
    assert!(prompt.contains("Apply this refactor"));
}

#[test]
fn link_refactor_apply_aborted_result_renders_exactly_and_exits_nonzero() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("apply_refactor_aborted"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        load_expected("refactor_apply_aborted_rendered")
    );
    assert_eq!(outcome.stdout, "");
}

/// T5/T6: the real `tephrad` built on this branch's base (PR-6) does
/// not implement `link.preview_refactor`/`link.apply_refactor` yet —
/// that dispatch entry lands in PR-8 (see `dispatch.rs`'s
/// `METHOD_NAMES`, which has no `link.*` entries; `other => Err((
/// METHOD_NOT_FOUND, format!("unknown method \"{other}\""), None))`
/// is what every refactor call hits today). Reproduced live against
/// a real container daemon: `tephractl link refactor --source notes
/// --old OldDoc.md --new NewDoc.md` prints exactly `tephractl:
/// unknown method "link.preview_refactor"` and exits 1 — this test
/// locks in that `tephractl`'s side of that interaction is correct
/// (a clean, non-panicking, exit-1 error with the daemon's verbatim
/// message, not a stack trace or a false success) so the only thing
/// still blocking the container screenshot for C32/C33 is PR-8
/// landing the server-side method, not any bug in this crate.
#[test]
fn link_refactor_preview_reports_unknown_method_cleanly_when_daemon_lacks_the_rpc() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32601, "unknown method \"link.preview_refactor\"", None),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: false,
            yes: false,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        "tephractl: unknown method \"link.preview_refactor\"\n"
    );
    assert_eq!(outcome.stdout, "");
}

/// Same as above, for `--apply --yes` hitting `link.apply_refactor`
/// on a daemon that doesn't implement it yet (T5/T6): a clean exit-1
/// error, never a false "applied" success and never a panic.
#[test]
fn link_refactor_apply_with_yes_reports_unknown_method_cleanly_when_daemon_lacks_the_rpc() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: err_envelope(-32601, "unknown method \"link.apply_refactor\"", None),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        false,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        outcome.stderr,
        "tephractl: unknown method \"link.apply_refactor\"\n"
    );
    assert_eq!(outcome.stdout, "");
    assert_eq!(server.received().len(), 1, "--yes issues exactly one RPC");
    assert_eq!(server.received()[0].body["method"], "link.apply_refactor");
}

#[test]
fn link_refactor_apply_aborted_json_flag_prints_raw_result_byte_for_byte() {
    let server = MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture("apply_refactor_aborted"),
    }]);
    let client = client_for(&server);

    let mut stdin: &[u8] = &[];
    let outcome = commands::execute(
        &Command::LinkRefactor {
            source: "notes".to_string(),
            old_path: "OldDoc.md".to_string(),
            new_path: "NewDoc.md".to_string(),
            apply: true,
            yes: true,
        },
        true,
        &client,
        &mut stdin,
    );

    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.stderr, load_expected("refactor_apply_aborted_json"));
}
