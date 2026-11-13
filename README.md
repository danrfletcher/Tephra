# Tephra

A federated graph and link engine for plain-text notes and code folders, written in Rust: a background daemon (`tephrad`) that indexes any number of folders into one link graph, and a command-line client (`tephractl`).

## Status

Everything described here is on `main`: the daemon with config and bearer-token auth; parsers for Markdown notes, code in 13 languages (tree-sitter) and the text of `.docx`, `.pptx` and PDF files; a SQLite cache (schema version 2); a source registry with a filesystem watcher; a link graph and resolver; block links (`[[path#^id]]`); an authenticated JSON-RPC server over HTTP and WebSocket; link refactoring on disk; search by name, heading, block and full text; and `tephractl` for all of it, including running the daemon as a macOS launchd agent.

**Not built yet:** remote sources, promoting a folder to a nested source, `source.rename`, rename detection and automatic link fixes, links to a code symbol by name, OCR for scanned documents, and clients for Obsidian, VSCode and Chrome. No benchmark has been published. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#not-built-yet) has the list.

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

## `tephractl` commands

Every command takes `--json` (raw structured output) and `--addr <addr>` (daemon address, default `127.0.0.1:4812`).

| Command | What it does |
|---|---|
| `source add <name> <path>`, `source remove <name>`, `source list` | Register, unregister and list sources. |
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
| `daemon install`, `daemon uninstall`, `daemon status` | Run `tephrad` as a launchd agent (macOS only). |

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
