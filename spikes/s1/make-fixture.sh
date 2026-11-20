#!/usr/bin/env bash
# Generate the S1/S3 fixture: a bare repo with 50 text files, then one commit
# holding a pure rename and a rename with a small edit.
#
#   make-fixture.sh <dir>            create <dir>/fixture.git at the first commit
#   make-fixture.sh <dir> advance    move <dir>/fixture.git to the second commit
#
# The bare repo is filled by fetching from <dir>/work into it, so a clone made
# before `advance` sees the second commit on its next fetch and nothing is
# ever sent from a clone to a remote.
# Layout: docs/ 20 files, src/ 20 files, notes/ 6 files, 4 files at the root.
# Commit 2: docs/doc-01.md -> docs/guide-01.md (identical content) and
# src/mod-01.txt -> src/engine-01.txt (2 of 30 lines changed).
set -euo pipefail

dir="${1:?usage: make-fixture.sh <dir> [advance]}"
mode="${2:-create}"
mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
work="$dir/work"
bare="$dir/fixture.git"

g() { git -c user.name=spike -c user.email=spike@example.invalid "$@"; }

body() { # body <name> <lines>: deterministic text, unique per file (one awk process per file)
  awk -v name="$1" -v n="$2" 'BEGIN {
    chars = "abcdefghijklmnopqrstuvwxyz0123456789-._ABCDEFGHIJKLMNOPQRSTUVWXYZ"
    for (i = 1; i <= n; i++) {
      s = name "-" i; h = 0
      for (j = 1; j <= length(s); j++) h = (h * 31 + index(chars, substr(s, j, 1))) % 4294967291
      printf "line %02d of %s: %d\n", i, name, h
    }
  }'
}

if [ "$mode" = "advance" ]; then
  g -C "$bare" fetch -q "$work" refs/tags/fixture-c2:refs/heads/main
  echo "fixture advanced to $(git -C "$bare" rev-parse --short main)"
  exit 0
fi

rm -rf "$work" "$bare"
g init -q -b main "$work"
mkdir -p "$work/docs" "$work/src" "$work/notes"
for i in $(seq -w 1 20); do body "doc-$i" 30 > "$work/docs/doc-$i.md"; done
for i in $(seq -w 1 20); do body "mod-$i" 30 > "$work/src/mod-$i.txt"; done
for i in $(seq -f '%02g' 1 6); do body "note-$i" 30 > "$work/notes/note-$i.md"; done
for f in README.md LICENSE.txt CHANGELOG.md TODO.txt; do body "$f" 30 > "$work/$f"; done

export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
g -C "$work" add -A
g -C "$work" commit -q -m "initial: 50 files"
g -C "$work" tag fixture-c1

export GIT_AUTHOR_DATE="2026-01-02T00:00:00Z" GIT_COMMITTER_DATE="2026-01-02T00:00:00Z"
g -C "$work" mv docs/doc-01.md docs/guide-01.md
g -C "$work" mv src/mod-01.txt src/engine-01.txt
sed -i.bak -e '5s/.*/line 05 edited/' -e '17s/.*/line 17 edited/' "$work/src/engine-01.txt"
rm -f "$work/src/engine-01.txt.bak"
g -C "$work" add -A
g -C "$work" commit -q -m "rename one file, rename and edit another"
g -C "$work" tag fixture-c2

g init -q --bare -b main "$bare"
g -C "$bare" config uploadpack.allowFilter true
g -C "$bare" config uploadpack.allowAnySHA1InWant true
g -C "$bare" fetch -q "$work" refs/tags/fixture-c1:refs/heads/main
echo "fixture: $(g -C "$work" ls-tree -r --name-only fixture-c1 | wc -l | tr -d ' ') files at c1, bare repo at $(git -C "$bare" rev-parse --short main)"
