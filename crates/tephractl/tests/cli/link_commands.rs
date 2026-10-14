//! `tests/cli/link_commands.test` (G8/GP5/E4): `link resolve`/
//! `link refactor` against the mock RPC server — correct method and
//! params, GP5's cross-source distinction, the ambiguous-candidate
//! rendering, the `--apply`/`--yes` confirmation gate (asserting no
//! RPC call happens until confirmed), and the preview/apply
//! rendering for the provisional response shape documented in
//! `src/render.rs`.

use tephractl::cli::Command;
use tephractl::commands;
use tephractl::transport::RpcClient;

use crate::support::{load_fixture, Behavior, MockServer};

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
fn link_resolve_cross_source_output_distinguishes_resolving_from_target_source() {
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
    let resolving_line = outcome
        .stdout
        .lines()
        .find(|l| l.contains("Resolving from"))
        .expect("a 'Resolving from' line");
    let target_line = outcome
        .stdout
        .lines()
        .find(|l| l.contains("Target source"))
        .expect("a 'Target source' line");
    assert!(resolving_line.contains("notes"));
    assert!(target_line.contains("atlas-repo"));
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
fn link_resolve_ambiguous_renders_numbered_candidate_list_not_raw_blob() {
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

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("1. @vault:lib/guide.md"));
    assert!(outcome.stderr.contains("2. @vault:api/guide.md"));
    assert!(!outcome.stderr.contains("\"data\":"));
}

#[test]
fn link_resolve_unresolved_prints_a_plain_clear_message() {
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

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome
        .stderr
        .contains("no unit in source \"vault\" matches \"missing\""));
}

#[test]
fn link_refactor_preview_sends_source_old_new_params_and_renders_diff() {
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
    assert!(outcome.stdout.contains("@notes:Journal.md:12"));
    assert!(outcome.stdout.contains("- [[OldDoc]]"));
    assert!(outcome.stdout.contains("+ [[NewDoc]]"));
}

#[test]
fn link_refactor_apply_with_yes_skips_the_confirmation_prompt() {
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
    assert_eq!(server.received()[0].body["method"], "link.apply_refactor");
    assert!(outcome
        .stdout
        .contains("Refactor applied: 2 file(s) changed"));
}

#[test]
fn link_refactor_apply_without_yes_prompts_and_a_refusal_makes_no_rpc_call() {
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

    assert_ne!(outcome.exit_code, 0);
    assert_eq!(
        server.call_count(),
        1,
        "only the preview call, never apply_refactor, on a refused confirmation"
    );
    assert_eq!(server.received()[0].body["method"], "link.preview_refactor");
}

#[test]
fn link_refactor_apply_without_yes_confirmed_with_y_does_call_the_rpc() {
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
}

#[test]
fn link_refactor_apply_aborted_result_is_reported_as_a_failure() {
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

    assert_ne!(outcome.exit_code, 0);
    assert!(outcome.stderr.contains("Refactor aborted"));
    assert!(outcome.stderr.contains("file modified since preview"));
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
    let parsed: serde_json::Value =
        serde_json::from_str(&outcome.stdout).expect("stdout must be valid JSON end to end");
    assert_eq!(parsed["status"], "applied");
    // The confirmation prompt happened, but on the separate writer —
    // not mixed into the JSON stdout.
    let prompt = String::from_utf8(prompt_out).expect("prompt is valid utf8");
    assert!(prompt.contains("Apply this refactor"));
}
