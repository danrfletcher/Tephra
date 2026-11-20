# Spike results (S1 to S5)

Throwaway measurements for the "Tephra: remote sources (GitHub and Google Drive)"
ticket. Code: `spikes/` (see `spikes/README.md`). Nothing here ships in the daemon.
Each file has Question, Method, Measurements, Verdict and What changes in the plan.
`not run: <reason>` means no number exists; such a spike is never `go`.

Run date for every row: 2026-11-19. Tools: `git --version` is git version 2.50.1
(Apple Git-155); `rclone version` is rclone v1.76.0. Raw numbers: `numbers/`;
terminal transcripts: `transcripts/` (`S1.txt` and `S3.txt` from a `rust:1-bookworm`
Linux container with git 2.39.5; `*-macos.txt` from macOS).

| Spike | Result file | Verdict | Run date | Tool versions | PRs it changes |
|---|---|---|---|---|---|
| S1 GitHub git-first vs REST | [S1-github-git-first.md](S1-github-git-first.md) | go-with-conditions | 2026-11-19 | git 2.50.1 | PR-5, PR-7 |
| S2 rclone | [S2-rclone.md](S2-rclone.md) | no-go | 2026-11-19 | rclone v1.76.0 | none (F13 stays); PR-10 and PR-11 build their own adapter |
| S3 blobless clone + sparse-checkout | [S3-partial-clone-sparse.md](S3-partial-clone-sparse.md) | go-with-conditions | 2026-11-19 | git 2.50.1, notify 6.1.1 | PR-7, PR-8 |
| S4 Docs API and 7-day token | [S4-docs-api-and-token.md](S4-docs-api-and-token.md) | no-go | 2026-11-19 | not run: no credential | PR-16 (stays gated) |
| S5 Drive changes feed | [S5-drive-changes-feed.md](S5-drive-changes-feed.md) | no-go | 2026-11-19 | not run: no credential | PR-10, PR-11 |

Verdict: S1 go-with-conditions, S2 no-go, S3 go-with-conditions, S4 no-go
(not run: no credential), S5 no-go (not run: no credential).

S4 and S5 verdicts are for unrun spikes. Neither keychain item
(`tephra-spike-github`, `tephra-spike-google`) was found on this machine
(`security` exit 44), so the GitHub half of S1 ran unauthenticated and S4 and S5
did not run at all.

Still to do by a person (this PR cannot): Dan reads the five verdicts and answers
go or no-go for each, S4 in particular (it gates PR-16); Dan answers yes or no on
rclone (a yes is recorded as a line `Dan approved: yes (<date>)` in `S2-rclone.md`);
Dan signs in to Google, runs `spikes/run-live.sh` once for the live S4 and S5
parts, and opens the scratch Doc to check bold, link and heading look unchanged;
the 168-hour token recheck (`python3 spikes/s4/run.py recheck`) runs after that.
