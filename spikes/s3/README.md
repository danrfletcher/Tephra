# S3: blobless clone plus sparse-checkout under a notify watcher

Question, method and results: `docs/spikes/S3-partial-clone-sparse.md`.

- `run.sh`: clones the S1 fixture with `--filter=blob:none --sparse`, runs a
  watcher over the clone, and logs the events from `sparse-checkout set`, a
  fetch, `sparse-checkout add`, a read of a file outside the set, and the
  protect checks.
- `watcher/`: a tiny program (its own cargo project, not a workspace member)
  that logs raw `notify` events, `.git` included, with no ignore filter. It
  uses the same `notify` version as `tephrad`.

Rerun: `spikes/s3/run.sh`. Needs git 2.25 or newer and cargo; otherwise it says
`not run`.
