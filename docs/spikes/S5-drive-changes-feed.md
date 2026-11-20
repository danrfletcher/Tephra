# S5: Drive changes feed, renames by file id

Run date: 2026-11-19 (offline parts only). Tools: python3 with urllib, git 2.50.1
(Apple Git-155). Raw numbers: `numbers/live.txt` (`s5.*`).

## Question

After `changes.getStartPageToken`, does `changes.list` report a rename with the
same file id, and how does it describe a move, a trash and a permanent delete?
How is a whole-drive feed narrowed to one picked folder? What does an idle 60 s
poll cost? How are shortcuts and two files with one name shown? Does the page
token survive a script restart without re-listing?

## Method

`python3 spikes/s5/run.py run` (rerun command; Dan runs it after signing in with
`spikes/s4/mint-token.py`). The script creates scratch files only (a folder, an
`other` sub-folder, five text files including two named `same-name.txt`, and a
shortcut) and takes the start page token. It never sends a rename, move, trash or
delete request itself: Dan does those four in the Drive web UI when the script
asks, and the scratch files are left in his Drive. Then:

    changes.list from the saved token, includeRemoved=true, all pages
    files.list   on the scratch folder (the two same-named files)
    changes.list again after a 60 s wait (idle poll: requests, bytes, seconds)
    python3 spikes/s5/run.py resume   (a new process, token read from the state file, no files.list)

`python3 spikes/s5/run.py selftest` checks the entry classifier offline.

## Measurements

not run: no credential. The Keychain item `tephra-spike-google` was not found
(`security` exit 44), so no scratch file, feed entry or poll exists, and nothing
is known here about file ids, trash, delete, folder narrowing, shortcuts,
same-name files or token survival. The offline `selftest` passes (it checks the
classifier on hand-made entries, not Google's behaviour).

## Verdict

Verdict: no-go

Not a measured no-go: not run: no credential, so it cannot be `go`. It stays
`no-go` until Dan runs `spikes/run-live.sh`.

## What changes in the plan

Nothing is changed by this result. PR-10 (Drive listing and keeping current) and
PR-11 should not rely on rename-by-file-id from the changes feed until the live
run has recorded it.
