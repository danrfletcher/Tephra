# S5: Drive changes feed and renames by file id

Question, method and results: `docs/spikes/S5-drive-changes-feed.md`.

Dan runs this; nothing here runs under `cargo test`. Needs the token from
`spikes/s4/mint-token.py`.

`python3 spikes/s5/run.py run` creates scratch files (two with one name, a
shortcut, an `other` folder), takes the page token, then asks you to rename one
file, move one, send one to the bin and delete one for good in the Drive UI.
It then reads the feed, polls once after 60 s, and reads again from the saved
token in a fresh process. The script never sends a delete, bin, move or rename
request itself. Scratch files are left in Drive.

`python3 spikes/s5/run.py selftest` checks the classifier offline.
