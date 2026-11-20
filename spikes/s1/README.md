# S1: git-first GitHub reads versus REST

Question, method and results: `docs/spikes/S1-github-git-first.md`.

- `make-fixture.sh <dir> [advance]`: a bare repo with 50 text files, then a
  second commit holding one pure rename and one rename with a small edit.
- `git-half.sh`: blobless clone, fetch, ls-tree, `diff -M --name-status` and a
  first-index read, with `.git` size, pack count and blob count after each.
- `rest.py`: the same listing and change detection through the REST API, with
  request counts, bytes and rate-limit units, including one idle 60 s
  conditional poll.
- `live.sh`: both halves against one named public repo and the rename
  comparison.

Rerun: `spikes/run-offline.sh` (fixture, no network) or
`spikes/s1/live.sh --unauthenticated` (network, public repo, 60 REST requests
per hour).
