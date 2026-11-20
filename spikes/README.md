# Spikes S1 to S5 (throwaway)

Measurements for the remote-sources ticket. Nothing here ships: no daemon code,
no workspace crate (this folder is not a workspace member), no RPC method, no
schema change. The results and verdicts are in `docs/spikes/`.

| Spike | What it answers | Rerun command |
|---|---|---|
| S1 | Git-first GitHub reads versus REST | `spikes/run-offline.sh` (git half on a generated repo), `spikes/s1/live.sh [--unauthenticated]` (git and REST on one public repo) |
| S2 | rclone as the Drive cache and upload engine | `spikes/s2/run.sh` |
| S3 | Blobless clone plus sparse-checkout under a `notify` watcher | `spikes/s3/run.sh` (also run by `spikes/run-offline.sh`) |
| S4 | Docs API range edit, paragraph marker, 7-day token | `python3 spikes/s4/run.py run` (after `spikes/s4/mint-token.py`) |
| S5 | Drive changes feed, renames by file id | `python3 spikes/s5/run.py run` |

Two entry points:

- `spikes/run-offline.sh`: S1 git half and S3 on a generated fixture. No
  network, no credentials. `cargo test` runs it
  (`crates/tephrad/tests/spike_offline_run.rs`).
- `spikes/run-live.sh [--github-unauthenticated]`: S1 live, S2, S4, S5. Needs the
  network and, for S4 and S5, a Google sign-in. Never run by `cargo test`.

Output goes to `spikes/out/` (gitignored): `numbers.txt` (offline) and
`numbers-live.txt` (live), one `key=value` per line. Set `TEPHRA_SPIKE_OUT` to
send it somewhere else. `TEPHRA_SPIKE_TRANSCRIPTS=<dir>` makes
`run-offline.sh` save `S1.txt` and `S3.txt` terminal transcripts.

## Rules the scripts follow

- Credentials come only from the macOS Keychain (`tephra-spike-github`,
  `tephra-spike-google`) through `security`. No token is passed on a command
  line, printed, or written to a file. A missing item gives
  `not run: no credential` and exit 0. `TEPHRA_SPIKE_NO_KEYCHAIN=1` pretends
  every item is missing.
- Git runs only inside throwaway directories made with `mktemp`, and only
  clone, fetch, ls-tree, diff, cat-file, sparse-checkout, status and a local
  commit. Nothing changes a remote.
- Nothing sends a Drive delete, bin, move or rename request. Where a spike
  needs one of those to happen (S5), the script creates scratch files and asks
  Dan to do it in the Drive UI. Scratch files are left for him.
- Anything that cannot run says `not run: <reason>`; no number is made up.

`crates/tephrad/tests/spike_scripts_fence.rs` scans this folder for the
forbidden patterns.

`spikes/.bin/` (gitignored) may hold a rclone release binary:
`spikes/s2/fetch-rclone.sh`.
