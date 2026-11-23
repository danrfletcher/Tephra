//! Wires `cli::Command` to `transport::RpcClient` calls and
//! `render::*` output (G8). This is the only module that decides
//! *which* JSON-RPC method a command calls and with what params; it
//! owns no resolution/refactor/registry logic of its own — every
//! verb here is a thin, typed wrapper over `RpcClient::call`
//! (PR-7's fence: "no RPC methods or server-side logic"). Most
//! commands issue exactly one RPC call; the exceptions are
//! `source remove` (T1), which first calls `source.list` to check
//! the name exists client-side before calling `source.unregister`,
//! and `link refactor --apply` without `--yes`, which (R3) first
//! calls `link.preview_refactor` to render the affected files/lines
//! ahead of the confirmation prompt, then `link.apply_refactor` once
//! confirmed — `--yes` still issues exactly one RPC.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::cli::{BlockTarget, Command, InFileSearchScope};
use crate::render;
use crate::since::Since;
use crate::transport::{RpcCallError, RpcClient};

/// Everything `execute` produced: what to print to stdout/stderr and
/// the process exit code `main.rs` should use. Kept as plain data
/// (rather than printing directly) so tests can assert on exact
/// rendered text without capturing real stdout/stderr.
pub struct CommandOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutcome {
    fn ok(stdout: String) -> Self {
        CommandOutcome {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        }
    }

    fn err(message: String) -> Self {
        CommandOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("tephractl: {message}\n"),
        }
    }
}

fn json_or(value: &Value, rendered: impl FnOnce() -> String, json: bool) -> String {
    if json {
        format!(
            "{}\n",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        )
    } else {
        rendered()
    }
}

/// Executes one parsed [`Command`] against `client`, reading a
/// confirmation prompt (for `link refactor --apply` without `--yes`)
/// from `stdin` rather than the real terminal, so tests can supply a
/// deterministic answer. The confirmation prompt itself (and, when
/// shown, the refactor preview rendered ahead of it) is written to
/// the real stderr — never stdout, so `--json` output stays valid
/// JSON (R2) — via [`execute_with_prompt_writer`]. Use that function
/// directly to capture the prompt text instead (e.g. in tests).
pub fn execute<R: BufRead>(
    command: &Command,
    json: bool,
    client: &RpcClient,
    stdin: &mut R,
) -> CommandOutcome {
    execute_with_prompt_writer(command, json, client, stdin, &mut std::io::stderr())
}

/// Same as [`execute`], but writes the interactive confirmation
/// prompt/preview to `prompt_out` instead of assuming the real
/// stderr, so tests can assert on exactly what the user would see
/// before they answer (R2/R3) without capturing real stderr.
pub fn execute_with_prompt_writer<R: BufRead, W: Write>(
    command: &Command,
    json: bool,
    client: &RpcClient,
    stdin: &mut R,
    prompt_out: &mut W,
) -> CommandOutcome {
    match command {
        Command::SourceAdd { name, path } => source_add(name, path, json, client),
        Command::SourceAddDrive { name, folder } => source_add_drive(name, folder, json, client),
        Command::SourceAddGithub { name, repo, branch } => {
            source_add_github(name, repo, branch.as_deref(), json, client)
        }
        Command::SourcePromote { name, path } => source_promote(name, path, json, client),
        Command::SourceUnpromote { name } => source_unpromote(name, json, client),
        Command::SourceRename { old, new } => source_rename(old, new, json, client),
        Command::SourceRemove { name } => source_remove(name, json, client),
        Command::SourceList => source_list(json, client),
        Command::Status => status(json, client),
        Command::Ls {
            source,
            folder,
            recursive,
        } => ls(source, folder, *recursive, json, client),
        Command::Search { query, limit } => search(query, *limit, json, client),
        Command::SearchIn {
            unit,
            scope,
            query,
            limit,
        } => search_in(unit, *scope, query, *limit, json, client),
        Command::SearchAllBlocks { query, limit } => search_all_blocks(query, *limit, json, client),
        Command::BlockId { unit, target } => block_id(unit, target, json, client),
        Command::Backlinks { id } => backlinks(id, json, client),
        Command::Fetch { unit } => fetch(unit, json, client),
        Command::CacheStatus => cache_status(json, client),
        Command::CacheSetLimit { limit } => cache_set_limit(limit, json, client),
        Command::CacheSet { source, on } => cache_set(source, *on, json, client),
        Command::Fixes { since } => fixes(*since, json, client),
        Command::LinkResolve {
            target,
            default_source,
        } => link_resolve(target, default_source.as_deref(), json, client),
        Command::LinkRefactor {
            source,
            old_path,
            new_path,
            apply,
            yes,
        } => link_refactor(
            RefactorRequest {
                source,
                old_path,
                new_path,
                apply: *apply,
                yes: *yes,
            },
            json,
            client,
            stdin,
            prompt_out,
        ),
        // `main.rs` runs these before it loads the token or builds a
        // client and streams their output; this arm exists so
        // `execute` still handles every `Command`, buffering the same
        // output into an outcome.
        Command::DaemonInstall { .. } | Command::DaemonUninstall | Command::DaemonStatus => {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let exit_code = crate::daemon::run(command, json, &mut out, &mut err);
            CommandOutcome {
                exit_code,
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
            }
        }
        // Same: `main.rs` runs the `auth` commands before any token or
        // client exists.
        Command::AuthGithub { .. }
        | Command::AuthGoogle
        | Command::AuthStatus
        | Command::AuthLogout { .. } => {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let exit_code = crate::auth::run_from_environment(command, json, &mut out, &mut err);
            CommandOutcome {
                exit_code,
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
            }
        }
    }
}

fn source_add(name: &str, path: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    // Wire param is "root" (`tephrad::rpc::dispatch::source_register`),
    // not "path" — the CLI's own flag/display vocabulary ("path") is
    // deliberately kept distinct from the wire field name here.
    match client.call("source.register", json!({"name": name, "root": path})) {
        Ok(stats) => CommandOutcome::ok(json_or(
            &stats,
            || render::render_source_added(name, path, &stats),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `source add github <name> <owner/repo> [--branch <b>]`: one
/// `source.add_github` call. The daemon lists the repo's whole tree at one
/// commit before it answers; the text index then runs in the background, so
/// a refusal prints the daemon's plain reason and exits 1 with nothing
/// registered.
fn source_add_github(
    name: &str,
    repo: &str,
    branch: Option<&str>,
    json: bool,
    client: &RpcClient,
) -> CommandOutcome {
    let mut params = json!({"name": name, "repo": repo});
    if let Some(branch) = branch {
        params["branch"] = json!(branch);
    }
    match client.call("source.add_github", params) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_github_source_added(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `source add drive <name> <folder>`: one `source.add_drive` call. The
/// daemon checks the credential and the folder and lists the whole folder
/// before it answers; the text index then runs in the background. A refusal
/// prints the daemon's plain reason and exits 1 with nothing registered.
fn source_add_drive(name: &str, folder: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.add_drive", json!({"name": name, "folder": folder})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_drive_source_added(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `source promote <name> <path>` (G11): one `source.promote` call. A
/// relative `<path>` is made absolute against this process's directory
/// first, because the daemon's own working directory is not the user's.
/// A refusal prints the daemon's message and its stable `reason` and
/// exits 1; nothing has changed then.
fn source_promote(name: &str, path: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    let root = match std::path::Path::new(path) {
        p if p.is_absolute() => path.to_string(),
        p => match std::env::current_dir() {
            Ok(cwd) => cwd.join(p).display().to_string(),
            Err(_) => path.to_string(),
        },
    };
    match client.call("source.promote", json!({"name": name, "root": root})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_source_promoted(&result),
            json,
        )),
        Err(RpcCallError::Rpc { message, data, .. }) => {
            let reason = data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str);
            CommandOutcome::err(match reason {
                Some(reason) => format!("{message} (reason: {reason})"),
                None => message,
            })
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `source unpromote <name>` (G12): one `source.unpromote` call. A
/// refusal prints the daemon's message and its stable `reason` and exits
/// 1; nothing has changed then. This is a fold back, never an undo: the
/// fix log keeps the promote entry.
fn source_unpromote(name: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.unpromote", json!({"name": name})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_source_unpromoted(&result),
            json,
        )),
        Err(RpcCallError::Rpc { message, data, .. }) => {
            let reason = data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str);
            CommandOutcome::err(match reason {
                Some(reason) => format!("{message} (reason: {reason})"),
                None => message,
            })
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `source rename <old> <new>` (G13): one `source.rename` call. A refusal
/// prints the daemon's sentence and its stable `reason` and exits 1;
/// nothing has changed then. Renaming to the same name is a success that
/// changed nothing.
fn source_rename(old: &str, new: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.rename", json!({"name": old, "new_name": new})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_source_renamed(&result),
            json,
        )),
        Err(RpcCallError::Rpc { message, data, .. }) => {
            let reason = data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str);
            CommandOutcome::err(match reason {
                Some(reason) => format!("{message} (reason: {reason})"),
                None => message,
            })
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn source_remove(name: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    // T1: tephrad's `source.unregister` is intentionally idempotent
    // (`registry::Registry::unregister`'s doc comment: "calling this
    // twice on an already-removed source returns Ok(()) both times
    // rather than erroring"), so the real daemon never signals "not
    // found" over the wire for this method — the
    // `RpcCallError::Rpc`/`error_means_not_found` arm below is
    // unreachable against a real `tephrad` and only guards a future
    // or differently-behaved server. Check client-side via
    // `source.list` first, before ever calling `source.unregister`,
    // so a mistyped or already-removed name is reported as "not
    // found" instead of a false "Removed source" success.
    match client.call("source.list", json!({})) {
        Ok(result) => {
            let exists = as_rows(&result)
                .iter()
                .any(|row| row.get("name").and_then(Value::as_str) == Some(name));
            if !exists {
                return CommandOutcome {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: render::render_source_not_found(name),
                };
            }
        }
        Err(e) => return CommandOutcome::err(describe_error(&e)),
    }

    match client.call("source.unregister", json!({"name": name})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_source_removed(name),
            json,
        )),
        Err(RpcCallError::Rpc { message, .. }) if render::error_means_not_found(&message) => {
            CommandOutcome {
                exit_code: 1,
                stdout: String::new(),
                stderr: render::render_source_not_found(name),
            }
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn source_list(json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.list", json!({})) {
        Ok(result) => {
            let rows = as_rows(&result);
            CommandOutcome::ok(json_or(
                &result,
                || render::render_source_table(&rows),
                json,
            ))
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `ls` (G1): one `unit.list` call. The daemon's `no such source:
/// <name>` is the message a user sees, with a non-zero exit; every
/// other failure (including a missing folder) is the daemon's own
/// message.
fn ls(
    source: &str,
    folder: &str,
    recursive: bool,
    json: bool,
    client: &RpcClient,
) -> CommandOutcome {
    let params = json!({"source": source, "folder": folder, "recursive": recursive});
    match client.call("unit.list", params) {
        Ok(result) => {
            CommandOutcome::ok(json_or(&result, || render::render_unit_list(&result), json))
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `search` (G8): one `unit.search` call with scope `names`. The
/// daemon's refusal of a blank query (INVALID_PARAMS) is printed as it
/// comes, with a non-zero exit; no results is the plain line `no matches`
/// and exit 0. `--json` is the raw result array.
fn search(query: &str, limit: Option<u64>, json: bool, client: &RpcClient) -> CommandOutcome {
    let mut params = json!({"query": query, "scope": "names"});
    if let Some(limit) = limit {
        params["limit"] = json!(limit);
    }
    match client.call("unit.search", params) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_search_results(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `search --in` (G9): one `unit.search` call with scope `headings` or
/// `blocks` and the file's id, `query` being what followed the sigil
/// (empty lists everything). The daemon's refusals (an unknown unit, a
/// bad id) are printed as they come, with a non-zero exit. `--json` is
/// the raw result object.
fn search_in(
    unit: &str,
    scope: InFileSearchScope,
    query: &str,
    limit: Option<u64>,
    json: bool,
    client: &RpcClient,
) -> CommandOutcome {
    let mut params = json!({"query": query, "scope": scope.as_str(), "in": unit});
    if let Some(limit) = limit {
        params["limit"] = json!(limit);
    }
    match client.call("unit.search", params) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_in_file_results(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `search "^^q"` (G10): one `unit.search` call with scope `all_blocks`,
/// `query` being what followed the `^^`. The daemon's refusal of a blank
/// or too-long query (INVALID_PARAMS) is printed as it comes, with a
/// non-zero exit; no results is the plain line `no matches` and exit 0.
/// `--json` is the raw result array.
fn search_all_blocks(
    query: &str,
    limit: Option<u64>,
    json: bool,
    client: &RpcClient,
) -> CommandOutcome {
    let mut params = json!({"query": query, "scope": "all_blocks"});
    if let Some(limit) = limit {
        params["limit"] = json!(limit);
    }
    match client.call("unit.search", params) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_all_blocks_results(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn block_id(unit: &str, target: &BlockTarget, json: bool, client: &RpcClient) -> CommandOutcome {
    let params = match target {
        BlockTarget::Line(line) => json!({"unit": unit, "line": line}),
        BlockTarget::Symbol(symbol) => json!({"unit": unit, "symbol": symbol}),
    };
    match client.call("block.ensure_id", params) {
        Ok(result) => {
            CommandOutcome::ok(json_or(&result, || render::render_block_id(&result), json))
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn status(json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.list", json!({})) {
        Ok(result) => {
            let rows = as_rows(&result);
            if json {
                let combined = json!({
                    "daemon_address": client.addr(),
                    "uptime": Value::Null,
                    "sources": rows,
                });
                CommandOutcome::ok(format!(
                    "{}\n",
                    serde_json::to_string_pretty(&combined)
                        .unwrap_or_else(|_| combined.to_string())
                ))
            } else {
                CommandOutcome::ok(render::render_status(client.addr(), &rows))
            }
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn as_rows(result: &Value) -> Vec<Value> {
    result.as_array().cloned().unwrap_or_default()
}

fn backlinks(id: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("graph.get_backlinks", json!({"id": id})) {
        Ok(result) => {
            CommandOutcome::ok(json_or(&result, || render::render_backlinks(&result), json))
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `fetch <id>`: one `unit.fetch`. The daemon answers when the file is in
/// its cache, so a slow download is a slow call. Plain output is the path
/// and nothing else (a script can use it as it is); `--json` is the whole
/// answer. A refusal prints the daemon's reason on stderr and exits 1.
fn fetch(unit: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("unit.fetch", json!({"unit": unit})) {
        Ok(result) => {
            CommandOutcome::ok(json_or(&result, || render::render_fetched(&result), json))
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `cache status`: one `cache.status`, read-only.
fn cache_status(json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("cache.status", json!({})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_cache_status(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `cache set-limit <limit>`: one `cache.set_limit`. A limit the daemon
/// refuses is exit 1 with its reason (and the limit that was kept) on
/// stderr; the limit is not checked here.
fn cache_set_limit(limit: &str, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("cache.set_limit", json!({"limit": limit})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_cache_limit_set(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `cache on|off <source>`: one `source.set_cache`.
fn cache_set(source: &str, on: bool, json: bool, client: &RpcClient) -> CommandOutcome {
    match client.call("source.set_cache", json!({"name": source, "cache": on})) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_cache_flag(&result),
            json,
        )),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// `fixes`: `fix.list {since?}`, one request, read-only. `since` is
/// worked out from the clock here (epoch seconds, as the verb takes).
/// No `limit` is sent: the daemon's default (50, newest first) applies.
fn fixes(since: Option<Since>, json: bool, client: &RpcClient) -> CommandOutcome {
    let mut params = json!({});
    if let Some(since) = since {
        params["since"] = json!(since.epoch(now_epoch()));
    }
    match client.call("fix.list", params) {
        Ok(result) => CommandOutcome::ok(json_or(&result, || render::render_fixes(&result), json)),
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn link_resolve(
    target: &str,
    default_source: Option<&str>,
    json: bool,
    client: &RpcClient,
) -> CommandOutcome {
    let mut params = json!({"link": target});
    if let Some(source) = default_source {
        params["default_source"] = Value::String(source.to_string());
    }

    match client.call("unit.resolve_link", params) {
        Ok(result) => CommandOutcome::ok(json_or(
            &result,
            || render::render_resolve_result(&result, default_source),
            json,
        )),
        Err(e @ RpcCallError::Rpc { .. }) => {
            if json {
                if let RpcCallError::Rpc {
                    code,
                    message,
                    data,
                } = &e
                {
                    let body = json!({"error": {"code": code, "message": message, "data": data}});
                    return CommandOutcome {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: format!(
                            "{}\n",
                            serde_json::to_string_pretty(&body)
                                .unwrap_or_else(|_| body.to_string())
                        ),
                    };
                }
                unreachable!()
            }
            CommandOutcome {
                exit_code: 1,
                stdout: String::new(),
                stderr: render::render_resolve_error(&e),
            }
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

struct RefactorRequest<'a> {
    source: &'a str,
    old_path: &'a str,
    new_path: &'a str,
    apply: bool,
    yes: bool,
}

fn link_refactor<R: BufRead, W: Write>(
    req: RefactorRequest,
    json: bool,
    client: &RpcClient,
    stdin: &mut R,
    prompt_out: &mut W,
) -> CommandOutcome {
    let params = json!({"source": req.source, "old_path": req.old_path, "new_path": req.new_path});

    if !req.apply {
        return match client.call("link.preview_refactor", params) {
            Ok(result) => CommandOutcome::ok(json_or(
                &result,
                || render::render_refactor_preview(&result),
                json,
            )),
            Err(e) => CommandOutcome::err(describe_error(&e)),
        };
    }

    if !req.yes {
        // R3: an interactive confirmation for an irreversible,
        // multi-file on-disk rewrite must not be blind. Fetch the
        // same preview `link refactor` (without `--apply`) would show
        // and render it ahead of the prompt, so the user sees exactly
        // which files/lines are about to change before they answer —
        // in addition to naming the source/old/new paths in the
        // prompt itself. This costs one extra RPC call, but only on
        // the interactive path: `--yes` still issues exactly one RPC
        // (`link.apply_refactor`).
        let preview = match client.call("link.preview_refactor", params.clone()) {
            Ok(result) => result,
            Err(e) => return CommandOutcome::err(describe_error(&e)),
        };
        let preview_text = render::render_refactor_preview(&preview);
        if !confirm(
            req.source,
            req.old_path,
            req.new_path,
            &preview_text,
            stdin,
            prompt_out,
        ) {
            return CommandOutcome {
                exit_code: 1,
                stdout: String::new(),
                stderr: "aborted: refactor not confirmed\n".to_string(),
            };
        }
    }

    match client.call("link.apply_refactor", params) {
        Ok(result) => {
            let status = result.get("status").and_then(Value::as_str);
            // R5 review finding: a "partial" result (some files
            // rewritten, a later one failed) is not success — a
            // script driving `tephractl` must be able to tell from
            // the exit code alone. Only a clean "applied" exits 0.
            let is_failure = matches!(status, Some("aborted") | Some("partial"));
            let rendered = json_or(&result, || render::render_refactor_apply(&result), json);
            if is_failure {
                CommandOutcome {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: rendered,
                }
            } else {
                CommandOutcome::ok(rendered)
            }
        }
        Err(e) => CommandOutcome::err(describe_error(&e)),
    }
}

/// Reads one line from `stdin` and treats `y`/`yes` (any case) as
/// confirmation, anything else (including EOF) as a refusal — never
/// defaults to "yes" on an unreadable/empty answer. Prints the prompt
/// (naming `source`/`old_path`/`new_path` and the rendered
/// `preview_text` diff, R3) to `out` — never the real stdout, so
/// `--json` output is never interleaved with it (R2).
fn confirm<R: BufRead, W: Write>(
    source: &str,
    old_path: &str,
    new_path: &str,
    preview_text: &str,
    stdin: &mut R,
    out: &mut W,
) -> bool {
    print_prompt(source, old_path, new_path, preview_text, out);
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
    }
}

fn print_prompt<W: Write>(
    source: &str,
    old_path: &str,
    new_path: &str,
    preview_text: &str,
    out: &mut W,
) {
    let _ = writeln!(
        out,
        "About to rewrite links in source \"{source}\": {old_path} -> {new_path}"
    );
    let _ = write!(out, "{preview_text}");
    let _ = write!(out, "Apply this refactor? [y/N]: ");
    let _ = out.flush();
}

fn describe_error(e: &RpcCallError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn confirm_with(stdin_text: &[u8]) -> (bool, String) {
        let mut stdin = Cursor::new(stdin_text.to_vec());
        let mut out: Vec<u8> = Vec::new();
        let answer = confirm(
            "notes",
            "OldDoc.md",
            "NewDoc.md",
            "1 affected link(s):\n  @notes:Journal.md:12  - [[OldDoc]]\n  @notes:Journal.md:12  + [[NewDoc]]\n",
            &mut stdin,
            &mut out,
        );
        (
            answer,
            String::from_utf8(out).expect("prompt is valid utf8"),
        )
    }

    #[test]
    fn confirm_accepts_y_and_yes_case_insensitively() {
        assert!(confirm_with(b"y\n").0);
        assert!(confirm_with(b"Y\n").0);
        assert!(confirm_with(b"yes\n").0);
        assert!(confirm_with(b"YES\n").0);
    }

    #[test]
    fn confirm_rejects_anything_else_including_empty_and_eof() {
        assert!(!confirm_with(b"n\n").0);
        assert!(!confirm_with(b"\n").0);
        assert!(!confirm_with(b"").0);
        assert!(!confirm_with(b"maybe\n").0);
    }

    #[test]
    fn confirm_prompt_names_source_and_old_and_new_paths_and_shows_the_preview_diff() {
        // R3: the prompt for an irreversible multi-file rewrite must
        // name what is about to change, not just ask a bare
        // yes/no — and (R2) it must go to the writer passed in, never
        // the real stdout.
        let (_, prompt) = confirm_with(b"n\n");
        assert!(prompt.contains("notes"));
        assert!(prompt.contains("OldDoc.md"));
        assert!(prompt.contains("NewDoc.md"));
        assert!(prompt.contains("@notes:Journal.md:12"));
        assert!(prompt.contains("- [[OldDoc]]"));
        assert!(prompt.contains("+ [[NewDoc]]"));
        assert!(prompt.contains("Apply this refactor? [y/N]: "));
    }
}
