# Tephra

A federated graph and link engine for plain-text notes and code folders, written in Rust: a background daemon (`tephrad`) that indexes any number of folders into one link graph, and a command-line client (`tephractl`).

## Status

**Where the code is.** `main` currently holds only these docs. The code is in seven stacked, open pull requests ([#1](https://github.com/danrfletcher/Tephra/pull/1) to [#7](https://github.com/danrfletcher/Tephra/pull/7)), not yet merged. The branch `vector/atlas-3/pr-7` contains all seven.

**Works today (on that branch):** the daemon with config and bearer-token auth; a Markdown parser (wikilinks, `[[@source:path]]`, links, tags, blocks); a SQLite cache; a source registry with a filesystem watcher; a link graph and resolver; an authenticated JSON-RPC server over HTTP and WebSocket; and `tephractl` for `source add/remove/list`, `status` and `link resolve`.

**Not built yet:** link refactoring on disk (pull request 8, in review; `tephractl link refactor` exists but the daemon does not serve it yet), links to anything inside non-Markdown files, remote sources, and clients for Obsidian, VSCode and Chrome. Only Markdown files are indexed today. No benchmark has been published.

## Build and test

```sh
git clone https://github.com/danrfletcher/Tephra.git && cd Tephra
git checkout vector/atlas-3/pr-7
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Every pull request had to pass those last three before it was opened.

## Try it

```sh
./target/release/tephrad &                                   # writes a config and auth token on first run
./target/release/tephractl source add notes /path/to/notes   # then: status, source list, link resolve
./target/release/tephractl link resolve "[[@notes:Some Note]]"
```

## How it was built

Tephra was built by an agent pipeline that Dan Fletcher runs: a Python state machine hands each pull request to a coder agent, then to a separate reviewer agent, then to a tester agent that runs the real daemon in a container. A pull request only opens after both pass. Dan wrote none of the Rust. From the first commit (12 Oct 2026, 21:59 BST) to the opening of pull request 7 (14 Oct, 13:12 BST) took 39 hours of wall-clock time. For 23 of those hours the pipeline sat blocked until a human resumed it, and for 43 minutes more on another pull request, so about 15 hours was active build time. Pull requests 1 to 7 went through 38 reviewer runs and 10 tester runs. What those gates caught is in [docs/FINDINGS.md](docs/FINDINGS.md).

## Docs

- [Architecture](docs/ARCHITECTURE.md)
- [What the review and test gates caught](docs/FINDINGS.md)
- Docs site: https://danrfletcher.github.io/Tephra/ (goes live once GitHub Pages is enabled)
