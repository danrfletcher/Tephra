# S2: rclone as the Drive cache and upload engine

Question, method and results: `docs/spikes/S2-rclone.md`. The verdict is
`no-go` unless that file holds the line `Dan approved: yes (<date>)`.

Rerun: `spikes/s2/run.sh`. It uses `rclone` from PATH or `spikes/.bin/rclone`
(`spikes/s2/fetch-rclone.sh` downloads the pinned release and checks its sha256). Without either it says
`not run` and the no-go stands. The checks use rclone's local backend; what
needs a Drive sign-in is marked `not run`.
