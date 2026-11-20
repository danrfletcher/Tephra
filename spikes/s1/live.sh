#!/usr/bin/env bash
# S1 live half: the git figures and the REST figures for one named public
# repo, side by side, then the rename comparison.
#
#   spikes/s1/live.sh [--unauthenticated]
#
# Needs the network. With no flag the GitHub credential is read from the
# Keychain item `tephra-spike-github`; when it is missing the script prints
# `not run: no credential` and exits 0. `--unauthenticated` runs against the
# public repo with no token (60 REST requests per hour); a rate-limit
# refusal is recorded with its reset time and no partial numbers. The rename
# comparison runs only when the git half and the REST half both finished.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/common.sh
. "$here/../lib/common.sh"
export NUMBERS="$SPIKE_OUT/numbers-live.txt"

# The named public repo and one pinned range with a pure rename (5 files,
# 0 lines changed) and renames with edits (27 and 62 lines changed).
repo="danrfletcher/Tephra"
base="30b5b5750b1c470753b07cd730587c856d9fd8d0"
head="39f5a8e697aa6d90beb295f342f16ee78e6ff0f3"
label="s1.public"

# Drop the lists an earlier run left behind: the comparison below must see only
# what this run's two halves wrote.
spike_clear_rename_lists "$label"

auth_flag=""
[ "${1-}" = "--unauthenticated" ] && auth_flag="--unauthenticated"

if [ -z "$auth_flag" ] && ! spike_credential_present tephra-spike-github; then
  echo "not run: no credential (Keychain item tephra-spike-github is missing; pass --unauthenticated for the public repo)"
  printf '%s.rest.status=not run: no credential\n' "$label" >> "$NUMBERS"
  exit 0
fi

"$here/git-half.sh" "$label" "https://github.com/$repo.git" "$head" "$base"
# git-half.sh writes to NUMBERS via lib/common.sh; the REST half appends to the same file.
# shellcheck disable=SC2086
NUMBERS="$NUMBERS" python3 "$here/rest.py" --repo "$repo" --base "$base" --head "$head" --branch main --label "$label" $auth_flag

spike_compare_renames "$label"
