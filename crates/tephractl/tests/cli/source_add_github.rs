//! `tests/cli/source_add_github.rs` (PR-5 G4/G8): `tephractl source add
//! github <name> <owner/repo> [--branch <b>]`, from argv to rendered output.
//! Rendered and `--json` output are compared byte for byte with the golden
//! files `tests/fixtures/expected/source_add_github_*.txt`; the params sent
//! are asserted exactly; a refusal that argv alone decides is made before
//! any request (the mock server sees none).

use tephractl::cli::{self, Action, ArgsError, Command, ParsedArgs};
use tephractl::commands;
use tephractl::render;
use tephractl::transport::RpcClient;

use crate::support::{load_expected, load_fixture, Behavior, MockServer};

fn run(args: &[&str]) -> Result<ParsedArgs, ArgsError> {
    match cli::parse_args(args)? {
        Action::Help => panic!("expected a command, got help"),
        Action::Run(parsed) => Ok(parsed),
    }
}

fn github(name: &str, repo: &str, branch: Option<&str>) -> Command {
    Command::SourceAddGithub {
        name: name.to_string(),
        repo: repo.to_string(),
        branch: branch.map(str::to_string),
    }
}

fn client_for(server: &MockServer) -> RpcClient {
    RpcClient::new(server.addr.clone(), "test-token".to_string())
}

fn respond(fixture: &str) -> MockServer {
    MockServer::start(vec![Behavior::Respond {
        status: 200,
        body: load_fixture(fixture),
    }])
}

fn execute(server: &MockServer, command: &Command, json: bool) -> commands::CommandOutcome {
    let mut stdin: &[u8] = &[];
    commands::execute(command, json, &client_for(server), &mut stdin)
}

// ---- argument parsing ------------------------------------------------------

#[test]
fn parses_name_and_repo_with_the_branch_unset() {
    let parsed = run(&[
        "source",
        "add",
        "github",
        "github-atlas",
        "danrfletcher/atlas",
    ])
    .unwrap();
    assert_eq!(
        parsed.command,
        github("github-atlas", "danrfletcher/atlas", None)
    );
    assert!(!parsed.json);
    assert_eq!(parsed.addr, None);
}

#[test]
fn parses_branch_json_and_addr_in_any_order_after_the_two_positionals() {
    let a = run(&[
        "source",
        "add",
        "github",
        "g",
        "o/r",
        "--branch",
        "release/1.x",
        "--json",
    ])
    .unwrap();
    assert_eq!(a.command, github("g", "o/r", Some("release/1.x")));
    assert!(a.json);

    let b = run(&[
        "source",
        "add",
        "github",
        "g",
        "o/r",
        "--json",
        "--addr",
        "127.0.0.1:9",
        "--branch",
        "dev",
    ])
    .unwrap();
    assert_eq!(b.command, github("g", "o/r", Some("dev")));
    assert_eq!(b.addr.as_deref(), Some("127.0.0.1:9"));

    let c = run(&["--json", "source", "add", "github", "g", "o/r"]).unwrap();
    assert!(c.json);
}

#[test]
fn a_branch_with_a_slash_unicode_or_a_hash_is_taken_as_typed() {
    for branch in ["feature/x", "ünï", "v1#2", "a b"] {
        let parsed = run(&["source", "add", "github", "g", "o/r", "--branch", branch]).unwrap();
        assert_eq!(parsed.command, github("g", "o/r", Some(branch)), "{branch}");
    }
}

#[test]
fn branch_needs_a_value_and_only_one_is_allowed() {
    let err = run(&["source", "add", "github", "g", "o/r", "--branch"]).unwrap_err();
    assert!(err.0.contains("--branch requires a value"), "{err}");
    let err = run(&[
        "source", "add", "github", "g", "o/r", "--branch", "a", "--branch", "b",
    ])
    .unwrap_err();
    assert!(err.0.contains("one branch"), "{err}");
    for bad in ["", "-x", "a\nb"] {
        let err = run(&["source", "add", "github", "g", "o/r", "--branch", bad]).unwrap_err();
        assert!(err.0.contains("invalid branch name"), "{bad:?}: {err}");
    }
}

#[test]
fn the_source_name_rules_are_the_local_ones() {
    for bad in ["my notes", "my/notes", "my@notes", "my:notes", "my*notes"] {
        let err = run(&["source", "add", "github", bad, "o/r"]).unwrap_err();
        assert!(err.0.contains("invalid source name"), "{bad}: {err}");
    }
}

#[test]
fn f4_anything_but_one_owner_slash_repo_is_refused_with_pick_one_repo() {
    for bad in [
        "danrfletcher",
        "danrfletcher/*",
        "*/*",
        "*/atlas",
        "https://github.com/danrfletcher",
        "https://github.com/orgs/some-org",
        "https://github.com/danrfletcher/atlas",
        "github.com/danrfletcher/atlas",
        "danrfletcher/atlas/extra",
        "danrfletcher/atlas/",
        "/atlas",
        "danrfletcher/",
        "danrfletcher/atlas?tab=repos",
        "danrfletcher/atlas#readme",
        "danrfletcher atlas",
        "../atlas",
        "danrfletcher/..",
        "danrfletcher/at\nlas",
        "",
    ] {
        let err = run(&["source", "add", "github", "g", bad]).unwrap_err();
        assert!(
            err.0.contains("pick one repo"),
            "{bad:?} should be refused: {err}"
        );
        assert!(
            !err.0.contains('\n'),
            "control characters are shown as ?: {err:?}"
        );
    }
}

#[test]
fn a_refusal_decided_by_argv_alone_sends_no_request() {
    // The command never reaches `execute`: `parse_args` fails first, so no
    // client is built. The mock server proves nothing was sent either way.
    let server = MockServer::start(vec![]);
    assert!(run(&["source", "add", "github", "g", "owner/*"]).is_err());
    assert!(server.received().is_empty());
}

#[test]
fn more_than_two_positionals_or_stray_flags_are_errors() {
    let err = run(&["source", "add", "github", "g", "o/r", "extra"]).unwrap_err();
    assert!(err.0.contains("pick one repo"), "{err}");
    let err = run(&["source", "add", "github", "g", "o/r", "--nope"]).unwrap_err();
    assert!(err.0.contains("unrecognized argument: --nope"), "{err}");
}

// ---- a local source literally named `github` -------------------------------

#[test]
fn one_argument_after_github_adds_a_local_source_named_github() {
    let parsed = run(&["source", "add", "github", "/home/dan/gh-notes"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "github".to_string(),
            path: "/home/dan/gh-notes".to_string(),
        }
    );
    // Flags still follow, as for any local add.
    let parsed = run(&["source", "add", "github", "/home/dan/gh-notes", "--json"]).unwrap();
    assert!(parsed.json);
    // A relative path, even one that looks like a repo name, is still the
    // single-argument form.
    let parsed = run(&["source", "add", "github", "atlas"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "github".to_string(),
            path: "atlas".to_string(),
        }
    );
}

#[test]
fn two_arguments_after_github_are_always_the_github_form() {
    // `source add github github /some/path` is NOT a local source: it is
    // name "github", repo "/some/path", which is refused as not one repo.
    let err = run(&["source", "add", "github", "github", "/some/path"]).unwrap_err();
    assert!(err.0.contains("pick one repo"), "{err}");
    // And a GitHub source may itself be named github.
    let parsed = run(&["source", "add", "github", "github", "o/r"]).unwrap();
    assert_eq!(parsed.command, github("github", "o/r", None));
}

#[test]
fn branch_is_not_a_local_flag_and_a_bare_add_github_is_an_error() {
    let err = run(&["source", "add", "github", "/home/dan/x", "--branch", "b"]).unwrap_err();
    assert!(err.0.contains("unrecognized argument: --branch"), "{err}");
    let err = run(&["source", "add", "github"]).unwrap_err();
    assert!(err.0.contains("source add requires"), "{err}");
    let err = run(&["source", "add", "github", "--json"]).unwrap_err();
    assert!(err.0.contains("source add requires"), "{err}");
}

#[test]
fn other_local_names_are_untouched_by_the_rule() {
    let parsed = run(&["source", "add", "notes", "/home/dan/Notes"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "notes".to_string(),
            path: "/home/dan/Notes".to_string(),
        }
    );
    // `github-atlas` is just a name.
    let parsed = run(&["source", "add", "github-atlas", "/p"]).unwrap();
    assert_eq!(
        parsed.command,
        Command::SourceAdd {
            name: "github-atlas".to_string(),
            path: "/p".to_string(),
        }
    );
}

// ---- what is sent, and what is shown --------------------------------------

#[test]
fn add_sends_source_add_github_with_name_and_repo_and_no_branch_key_by_default() {
    let server = respond("source_add_github_success");
    let outcome = execute(
        &server,
        &github("github-atlas", "danrfletcher/atlas", None),
        false,
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_add_github_rendered"));
    assert_eq!(outcome.stderr, "");

    let received = server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["method"], "source.add_github");
    assert_eq!(
        received[0].body["params"],
        serde_json::json!({"name": "github-atlas", "repo": "danrfletcher/atlas"})
    );
}

#[test]
fn add_with_a_branch_sends_it() {
    let server = respond("source_add_github_success");
    let outcome = execute(
        &server,
        &github("github-atlas", "danrfletcher/atlas", Some("release")),
        false,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        server.received()[0].body["params"],
        serde_json::json!({"name": "github-atlas", "repo": "danrfletcher/atlas", "branch": "release"})
    );
}

#[test]
fn add_json_prints_the_daemons_result_byte_for_byte() {
    let server = respond("source_add_github_success");
    let outcome = execute(
        &server,
        &github("github-atlas", "danrfletcher/atlas", None),
        true,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, load_expected("source_add_github_json"));
}

#[test]
fn an_empty_repo_says_no_commits_and_no_text_to_index() {
    let server = respond("source_add_github_empty");
    let outcome = execute(
        &server,
        &github("empty-repo", "danrfletcher/empty", None),
        false,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_add_github_empty_rendered")
    );
}

#[test]
fn daemon_refusals_print_the_plain_reason_exit_1_and_nothing_on_stdout() {
    for (fixture, message) in [
        (
            "source_add_github_refused_auth_required",
            "no GitHub token is stored: run `tephractl auth github` first",
        ),
        (
            "source_add_github_refused_not_found",
            "danrfletcher/nope does not exist, or the token has no access to it (GitHub answers the same for both)",
        ),
        (
            "source_add_github_refused_unknown_branch",
            "danrfletcher/atlas has no branch \"nope\"",
        ),
        (
            "source_add_github_refused_pick_one_repo",
            "\"danrfletcher\" is not one repository: give owner/repo (pick one repo; a whole account or a list of repos cannot be added)",
        ),
    ] {
        let server = respond(fixture);
        let outcome = execute(&server, &github("g", "o/r", None), false);
        assert_eq!(outcome.exit_code, 1, "{fixture}");
        assert_eq!(outcome.stderr, format!("tephractl: {message}\n"), "{fixture}");
        assert_eq!(outcome.stdout, "", "{fixture}");
        // One call only: a refused add is never retried.
        assert_eq!(server.received().len(), 1, "{fixture}");
    }
}

// ---- source list and ls ---------------------------------------------------

#[test]
fn source_list_shows_indexing_progress_branch_cache_and_last_sync() {
    let server = respond("source_list_github_indexing");
    let outcome = execute(&server, &Command::SourceList, false);
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        load_expected("source_add_github_source_list")
    );
}

#[test]
fn source_list_shows_a_last_sync_time_and_a_status_note() {
    let rows: Vec<serde_json::Value> = vec![serde_json::json!({
        "name": "g", "path": "github:o/r", "status": "active",
        "last_scanned_at": 1_700_000_000, "kind": "github", "repo": "o/r",
        "branch": "main", "cache": true, "cache_bytes": 0, "git_bytes": 0,
        "index_done": 3, "index_total": 3, "status_reason": "waiting out a rate limit",
    })];
    let out = render::render_source_table_with(&rows, &|t| format!("T{t}"));
    assert_eq!(
        out,
        concat!(
            "NAME                 STATUS    PATH\n",
            "g                    active    github:o/r\n",
            "                     branch main, cache on, cached 0 bytes, .git 0 bytes, last sync T1700000000\n",
            "                     note: waiting out a rate limit\n",
        )
    );
}

#[test]
fn local_only_source_lists_render_exactly_as_before() {
    let rows: Vec<serde_json::Value> = vec![
        serde_json::json!({"name": "notes", "path": "/home/dan/Notes", "status": "active", "last_scanned_at": 1}),
        serde_json::json!({"name": "old", "path": "/mnt/x", "status": "offline", "last_scanned_at": null}),
    ];
    assert_eq!(
        render::render_source_table(&rows),
        concat!(
            "NAME                 STATUS    PATH\n",
            "notes                active    /home/dan/Notes\n",
            "old                  offline   /mnt/x (offline)\n",
        )
    );
}

#[test]
fn ls_of_a_remote_source_shows_state_cache_bytes_and_why_a_file_is_name_only() {
    let server = respond("unit_list_remote");
    let outcome = execute(
        &server,
        &Command::Ls {
            source: "github-atlas".to_string(),
            folder: String::new(),
            recursive: false,
        },
        false,
    );
    assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
    assert_eq!(outcome.stdout, load_expected("source_add_github_ls"));
}

#[test]
fn ls_renders_a_hostile_remote_name_safely() {
    let server = respond("unit_list_remote_hostile");
    let outcome = execute(
        &server,
        &Command::Ls {
            source: "github-atlas".to_string(),
            folder: String::new(),
            recursive: false,
        },
        false,
    );
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.stdout,
        load_expected("source_add_github_ls_hostile")
    );
    // No escape, no newline inside the row, no direction override.
    assert!(!outcome.stdout.contains('\u{1b}'));
    assert!(!outcome.stdout.contains('\u{202e}'));
    assert_eq!(outcome.stdout.matches('\n').count(), 1);
}

#[test]
fn a_local_listing_keeps_its_layout() {
    let result = serde_json::json!({
        "source": "notes", "folder": "", "recursive": false, "offline": false,
        "entries": [{"id": "@notes:a.md", "kind": "prose", "name": "a.md",
                     "rel_path": "a.md", "size": 12, "mtime": 1_700_000_000}],
    });
    let out = render::render_unit_list_with(&result, &|t| format!("T{t}"));
    assert_eq!(out, "prose              12 T1700000000      a.md\n");
}

#[test]
fn usage_documents_the_github_form_and_the_local_github_rule() {
    assert!(cli::USAGE.contains("tephractl source add github <name> <owner/repo> [--branch <b>]"));
    assert!(cli::USAGE.contains("source add github <path>"));
}
