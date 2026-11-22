# Tephra

A federated graph and link engine for plain-text notes and code folders, written in Rust: a background daemon (`tephrad`) that indexes any number of folders into one link graph, and a command-line client (`tephractl`).

## Status

Everything described here is on `main`: the daemon with config and bearer-token auth; parsers for Markdown notes, code in 13 languages (tree-sitter) and the text of `.docx`, `.pptx` and PDF files; a SQLite cache (schema version 2); a source registry with a filesystem watcher; a link graph and resolver; block links (`[[path#^id]]`); an authenticated JSON-RPC server over HTTP and WebSocket; link refactoring on disk; search by name, heading, block and full text; and `tephractl` for all of it, including running the daemon as a macOS launchd agent.

**Not built yet:** remote sources, rename detection and automatic link fixes, links to a code symbol by name, OCR for scanned documents, and clients for Obsidian, VSCode and Chrome. No benchmark has been published. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#not-built-yet) has the list.

## Build and test

```sh
git clone https://github.com/danrfletcher/Tephra.git && cd Tephra
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Every pull request had to pass those last three before it was opened.

## Try it

```sh
./target/release/tephrad &                                   # writes a config and auth token on first run
./target/release/tephractl source add notes /path/to/notes
./target/release/tephractl status
./target/release/tephractl link resolve "[[@notes:Some Note]]"
```

On a Mac, `tephractl daemon install` runs `tephrad` as a launchd agent instead (see [the daemon section](docs/ARCHITECTURE.md#running-the-daemon-on-macos)).

## Linux `tephrad` for Atlas's tests

Atlas's tests run against the real `tephrad` inside the Linux `desktop-<slug>` webtop container (aarch64 on an Apple-silicon Mac), where `inotify` is the file watcher. `scripts/linux-tephrad.sh` builds and runs it there. It needs Docker and nothing else on the Mac, and it changes nothing in the Atlas repo.

```sh
scripts/linux-tephrad.sh build               # cargo build --release in the official rust image -> target/linux/ (gitignored)
scripts/linux-tephrad.sh start <slug>        # copy into desktop-<slug>, write a config, start tephrad in the background
scripts/linux-tephrad.sh status <slug>       # up (exit 0) or down (exit 3)
scripts/linux-tephrad.sh stop <slug>         # stops only that run's tephrad
scripts/linux-tephrad.sh test [<slug>] [--repeat 50] [--all]   # the rename race tests, in the container
```

- `build` uses a named volume for the cargo cache (`tephra-linux-cargo`) and one for the build output (`tephra-linux-target`). The image is `rust:1-bookworm`; `TEPHRA_RUST_IMAGE` names another.
- `start` keeps everything in `/tmp/tephra-linux/<slug>` on the container's own filesystem: its own config, `XDG_DATA_HOME` (so its own token and cache), `HOME`, log and pid file. The container user's real `~/.local/share/tephra` is never touched. `tephrad` listens on a free loopback port it picks itself (`--port <n>` asks for one; a port in use is reported, never reused). Run twice, it stops its own daemon by pid and starts one new one: there are never two. By default it seeds a vault with the golden-path notes and registers it as source `notes`; `--source <dir>` registers another folder instead (refused on a bind mount of the host, where inotify misses host-made changes) and `--no-source` registers none.
- Before anything is replaced `start` runs `tephrad --help` in the container, so a binary that needs a newer glibc than the container has fails with the glibc message. It also refuses a container whose `fs.inotify.max_user_watches` is below 8192 and names that setting.
- What Atlas's tests use to reach it: `start` prints it. From the host, with the address it printed:

  ```sh
  docker exec -e XDG_DATA_HOME=/tmp/tephra-linux/<slug>/xdg-data desktop-<slug> \
    /tmp/tephra-linux/<slug>/bin/tephractl --addr 127.0.0.1:<port> source list
  ```

  The JSON-RPC address is `127.0.0.1:<port>` inside the container (HTTP `POST /rpc` or WebSocket `/rpc`), the bearer token is the file `/tmp/tephra-linux/<slug>/xdg-data/tephra/token`, and a raw call is `docker exec desktop-<slug> sh -c 'curl -s -H "Authorization: Bearer $(cat <token file>)" -d <json-rpc> http://127.0.0.1:<port>/rpc'`.

The race tests are `crates/tephrad/tests/rename_races.rs`, six tests that start the real `tephrad` binary and race its watcher against the real `link.rename_event` RPC: event first, watcher first, both at once, event after the fix, event queued while down, and a conflicting event against a hash guess. They use real `rename(2)` calls, so the watcher side is real inotify; no test-only method exists. The file is Linux-only: on macOS `cargo test -p tephrad --test rename_races` compiles it and runs zero tests. The pairing and dedupe rules themselves (`rename_detect`) are OS-independent and their unit tests run the same on the Mac; only the adapter that turns inotify events into marks is Linux-specific. `test --repeat 50` runs every test fifty times over, each on a new daemon and vault (the flake check); `test --all` adds the edge and fence tests (E24, F5, F12). A FSEvents rename on a real Mac, a Finder rename and a `git pull` rename cannot be proved on Linux and are checked by hand.

## `tephractl` commands

Every command takes `--json` (raw structured output) and `--addr <addr>` (daemon address, default `127.0.0.1:4812`).

| Command | What it does |
|---|---|
| `source add <name> <path>`, `source remove <name>`, `source list` | Register, unregister and list sources. Adding or removing a source sends connected WebSocket clients a `graph:changed` push with a `sources_changed` list (`[{kind: "added"|"removed", name}]`), so a client need not wait for its next check. |
| `source add github <name> <owner/repo> [--branch <b>]` | Register ONE GitHub repository on ONE branch (default: the repo's default branch; `source.add_github`). Needs `tephractl auth github` first. The whole file list shows within seconds, every file `remote` with 0 bytes cached and nothing written to disk; text files are then read once through the API in the background (`source list` shows `indexing done/total`, then `active`) for links, headings, blocks and symbols, and the bytes are dropped. Only `owner/repo` is accepted: an owner alone, `owner/*`, `*/*`, a URL or a path inside the repo is refused ("pick one repo"). Refused with nothing registered when there is no token, the token is rejected, the repo is not found (no access looks the same), the branch does not exist or the repo has moved. A vault note can link `[[@<name>:README.md]]`; backlinks work both ways. Once indexed, a GitHub source is kept current: about every 60 s the daemon makes ONE conditional request per source (`If-None-Match` with the stored ETag; a 304 writes nothing but the last-sync time). A changed branch is read as a whole tree and applied in one step: added, changed and removed files reach the index and `ls`, changed text files are re-read through the API and re-indexed (bytes dropped, nothing written under `remote/`), and connected clients get exactly one `graph:changed` push. A file deleted on GitHub leaves the index (links to it become unresolved, as for a deleted local file); an unmodified cached copy under `remote/<source>/clone/` is deleted, a modified one is kept as `needs_attention` with the reason `deleted on GitHub`. A rename on GitHub is a removal plus an addition (no link is rewritten). `source list` shows `offline` (cannot reach GitHub: the index, links and `ls` keep answering, nothing downloads, and the next good poll returns the source to `active` with no re-index), `syncing` with the retry time (a rate limit), `auth_required` (GitHub rejected the token: run `tephractl auth github`; nothing is removed) or `needs_attention` (the repo or branch is gone, or the repo moved: the new location is shown, nothing is followed and no item is removed), each with a `note:` line. A first index that loses the network part-way shows `offline` and carries on from the files it has not read when GitHub is reachable again (no restart needed). Each file is written in one step, so a daemon killed mid-index leaves no half-written file, and the next start indexes the source again from scratch (every text file is read again; the file list is kept). A completed index survives a restart: the daemon reads no tree and no file, and the first poll sends the stored ETag. A local source literally named `github` is added with ONE argument after `github` (`source add github <path>`); TWO arguments after `github` are always the GitHub form. |
| `fetch <id>` | Download one file of a remote source into Tephra's cache (`unit.fetch`) and print its path, and nothing else. `<id>` is `@source:path/to/file`. Nothing is fetched until you run this. A file already fetched is not downloaded again; a file with edits waiting to be pushed is printed as it is. Exits 1 with the reason on stderr when there is no file (offline, sign-in needed, failed download, full disk, or an item that is never fetched: Docs, Sheets, Slides, Forms, Drawings, shortcuts, symlinks, submodules and Git LFS pointers). |
| `source promote <name> <path>` | Make a folder of a registered source its own source (`source.promote`). The folder's files leave the parent, links that must change to keep resolving are rewritten for you (`@source:` prefixes), and connected clients get one `graph:changed` push with `sources_changed: [{kind: "promoted", name}]`. Refused, with nothing changed, when the folder is a source root or in no source, is a file, its source is offline, or the name is invalid or taken. `source add` still refuses a root nested in another. |
| `source unpromote <name>` | Fold a promoted source back into the source it was promoted out of (`source.unpromote`). Its files return to the parent, the links the promote rewrote are rewritten back, and connected clients get one `graph:changed` push with `sources_changed: [{kind: "unpromoted", name}]`. `source remove` of a promoted source does the same thing. It is not an undo: the promote stays in `fixes`, with the unpromote above it. Refused, with nothing changed, when the source is unknown, was not promoted (or its parent was removed), or its parent is offline. |
| `source rename <old> <new>` | Rename a source (`source.rename`). Every `@old:` link in every source is rewritten to `@new:` (anchors and aliases kept; code files change in comment text only; plain Markdown links are never touched; a link in a PDF or Office document is logged `needs_manual_fix` and not written), the registry row, the cache and the watcher move to the new name, `fixes` shows one `source_rename` entry, and connected clients get one `graph:changed` push with `sources_changed: [{kind: "renamed", name, old_name}]`. Renaming to the same name does nothing and succeeds. Refused, with nothing changed and the reason in `error.data.reason` (`invalid_name`, `name_taken`, `unknown_source` or `source_offline`; the same strings, with `not_inside_a_source`, `is_a_source_root`, `not_a_folder`, `nested_source` and `not_promoted`, are what `source add`, `source promote` and `source unpromote` refuse with), when the new name is empty, has a space, `/`, `@`, `:` or `*`, or is taken (compared without regard to case). |
| `status` | Daemon and source status. |
| `ls <source> [folder] [-r]` | List one folder of a source (works for an offline source, from the cache). |
| `search <query> [--limit n]` | Find files by name, title or path across every source. |
| `search --in <id> "#<query>"` | Find headings in one file. A bare `#` lists them all. |
| `search --in <id> "^<query>"` | Find blocks in one file. A bare `^` lists them all. |
| `search "^^<query>"` | Full-text search of the blocks of every file of every source. |
| `link resolve [--source <name>] <target>` | Resolve a link. A block link prints the block's ID and line, and `moved` if it was found in another file. |
| `link refactor --source <name> --old <path> --new <path> [--apply] [--yes]` | Preview or apply the link rewrites a file move needs. |
| `block-id <unit-id> --line <n>` or `--symbol <name>` | Give a block an ID and print the link to it. |
| `backlinks <id>` | The links to one file (`@s:path`) or one block (`@s:path#^id`), one row per link with its line. |
| `fixes [--since <30m\|2h\|7d\|date>] [--json]` | The log of automatic link fixes (`fix.list`), newest first: `time kind old -> new (n files) status`; an `uncertain` line ends with ` - why`. It only reads. |
| `daemon install`, `daemon uninstall`, `daemon status` | Run `tephrad` as a launchd agent (macOS only). |
| `auth github [-]` | Sign in to GitHub. It reads a token with echo off (or one line from stdin with `-`; a token is never an argument, so it cannot reach shell history or `ps`), checks it with `GET /user`, and only on a 200 stores it in the macOS Keychain and prints `signed in as <login>`. A rejected, empty or whitespace-containing token, a rate limit or a network error exits 1 and stores nothing. Use a fine-grained read-only token. |
| `auth google` | Sign in to Google Drive in your browser (loopback redirect on `127.0.0.1`, PKCE). It prints the consent URL, opens it, waits up to 5 minutes, and stores the token set, refresh token included, in the Keychain. It needs `google_client_json` in `config.toml`: the path to the JSON of your own Google Cloud OAuth client of type Desktop app, with the consent screen published In production (a client left in Testing mode gets refresh tokens that expire after 7 days). |
| `auth status` | One line per provider, `signed in` or `auth_required`, read from the Keychain only (nothing is sent; a credential the provider has since revoked still reads `signed in`). `--json` gives `{"github":{"state":...},"google":{"state":...}}`. It never prints a token. |
| `auth logout github` or `auth logout google` | Delete that provider's Keychain item and leave the other alone. Local only: the token is not revoked at the provider. Not signed in is not an error. |

The `auth` commands need neither the daemon nor the bearer-token file, and add no RPC method. Credentials live only in the macOS Keychain, one item per provider; if the Keychain cannot be used the command exits 1 with `Keychain unavailable` and nothing is written anywhere else. `config.toml` keys read by `tephractl`: `google_client_json`, and, for tests only, `github_api_url` (a loopback URL) and `test_credential_file` (honoured only by a build with the `test-credential-store` feature).

Searches never change how a link resolves: an unprefixed `[[Name]]` resolves only inside its own source.

## What Tephra writes to your files

Tephra reads your files and never changes them, except in three places. Each writes a temp file, syncs it and renames it over the original, and writes nothing if the file changed since it was read.

1. `link.apply_refactor` (`tephractl link refactor --apply`): rewrites the links that a moved file breaks.
2. `block.ensure_id` (`tephractl block-id`): adds a block ID to the one block you pick, and nothing else.
3. The duplicate block ID rename: when an edit makes an ID that a file already had appear a second time, the new copy is renamed (`^summary` to `^summary-1`, a Tephra-format ID to a fresh one) and the original keeps its ID, so links stay on it. Only the ID token changes. A file that already had duplicates when Tephra first scanned it is never written, and the same ID in two different files is allowed.

## How it was built

Tephra is a proof-of-concept test for, and was built by Dan's autonomous build pipeline: a Python state machine hands each pull request to a coder agent, then to a separate reviewer agent, then to a tester agent that runs the real daemon in a container. A pull request only opens after both pass. Dan wrote no Rust for this project. From the first commit (12 Oct 2026, 21:59 BST) to the opening of pull request 7 (14 Oct, 13:12 BST) took 39 hours of wall-clock time. For 24 of those hours the pipeline sat blocked overnight awaiting a human to resume it. Legitimate build time was about 15 hours, fully active and autonomous. Pull requests 1 to 7 went through 38 reviewer runs and 10 tester runs. What those gates caught is in [docs/FINDINGS.md](docs/FINDINGS.md).

## Docs

- [Architecture](docs/ARCHITECTURE.md)
- [What the review and test gates caught](docs/FINDINGS.md)
- Docs site: https://danrfletcher.github.io/Tephra/ (goes live once GitHub Pages is enabled)
