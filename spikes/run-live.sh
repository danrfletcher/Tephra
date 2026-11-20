#!/usr/bin/env bash
# Live spike run: S1 live half (network), S2 (rclone), S4 and S5 (Google).
# Dan runs this once, signed in, from a terminal.
#
#   spikes/run-live.sh [--github-unauthenticated]
#
# Credentials are read only from the Keychain (`tephra-spike-github`,
# `tephra-spike-google`). A missing item gives `not run: no credential` for
# that part and the script still exits 0. Nothing here runs under `cargo
# test`. Numbers go to spikes/out/numbers-live.txt.
#
# S4 and S5 need the Google token first: spikes/s4/mint-token.py.
# S5 stops to ask you to change four files in the Drive UI.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
. "$here/lib/common.sh"
export NUMBERS="$SPIKE_OUT/numbers-live.txt"
: > "$NUMBERS"

gh_flag=""
[ "${1-}" = "--github-unauthenticated" ] && gh_flag="--unauthenticated"

# shellcheck disable=SC2086
"$here/s1/live.sh" $gh_flag
"$here/s2/run.sh"
python3 "$here/s4/run.py" run
python3 "$here/s5/run.py" run
echo "numbers: $NUMBERS"
