#!/usr/bin/env bash
# PR-8.F1 container verification: `unit.search` and `tephractl search`.
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir,
# a free loopback port it picks itself, never the default 4812), adds the two
# GP4 fixture sources, and saves each tephractl transcript to
# verify/pr-8/*.txt (override the folder with OUT_DIR):
#
#   01-search-atlas-outside.txt       GP4: the target is on line 1
#   02-search-atlas-outside-json.txt  the raw result array
#   03-search-empty.txt               "" -> INVALID_PARAMS, non-zero exit
#   04-search-hash.txt                "#x" -> "pick a file first", non-zero exit
#   05-search-offline.txt             `notes` root renamed -> offline: true
#
# Every transcript ends with an "exit: <code>" line. The script checks the
# GP4 order, the two error exits and the offline flag, and exits non-zero if
# any is wrong. It starts one process (tephrad), records its PID and stops
# only that PID on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-8}"
TARGET_ID='@notes:Obsidian Atlas/plans/Atlas outside Obsidian Chrome plugin, VSCode plugin.md'

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr8-verify.XXXXXX")"
DAEMON_PID=""

cleanup() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "pr-8.sh: FAIL: $*" >&2
  exit 1
}

echo "== build"
(cd "$ROOT" && cargo build --workspace)
TEPHRAD="$ROOT/target/debug/tephrad"
TEPHRACTL="$ROOT/target/debug/tephractl"

# A free loopback port: bind it once to prove nothing holds it.
pick_port() {
  python3 - <<'PY'
import random, socket
for _ in range(200):
    port = random.randint(20000, 60000)
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
    except OSError:
        continue
    finally:
        s.close()
    print(port)
    break
else:
    raise SystemExit("no free port found")
PY
}
PORT="$(pick_port)"
ADDR="127.0.0.1:$PORT"

# Own XDG dirs: the daemon, its config, token and cache never touch Dan's.
export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
EOF

# GP4 fixture: sources `notes` and `tephra`.
NOTES="$WORK/notes-root"
TEPHRA="$WORK/tephra-root"
mkdir -p "$NOTES/Obsidian Atlas/plans" "$TEPHRA/crates/atlas"
printf '# Atlas outside Obsidian\n' \
  >"$NOTES/Obsidian Atlas/plans/Atlas outside Obsidian Chrome plugin, VSCode plugin.md"
printf '# Atlas\n' >"$NOTES/Obsidian Atlas/Atlas.md"
printf '# Outside\n' >"$NOTES/Outside.md"
printf '# Atlas notes\n' >"$NOTES/Obsidian Atlas/plans/atlas-notes.md"
printf 'pub fn outside() {}\n' >"$TEPHRA/crates/atlas/outside.rs"

echo "== start tephrad on $ADDR"
"$TEPHRAD" --config "$XDG_CONFIG_HOME/tephra/config.toml" >"$WORK/tephrad.log" 2>&1 &
DAEMON_PID=$!
echo "tephrad pid $DAEMON_PID"

ctl() { "$TEPHRACTL" --addr "$ADDR" "$@"; }

# Wait (bounded, ~20 s) for the daemon to answer.
ready=""
for _ in $(seq 1 100); do
  kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/tephrad.log" >&2; fail "tephrad exited early"; }
  if ctl source list >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.2
done
[ -n "$ready" ] || fail "tephrad did not answer on $ADDR"

echo "== add the two sources"
ctl source add notes "$NOTES"
ctl source add tephra "$TEPHRA"

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.txt

# transcript <file> <command words...>: the command line, its output and its
# exit code, in that order.
transcript() {
  local file="$1"
  shift
  {
    local code=0
    local out
    out="$("$@" 2>&1)" || code=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$code"
  } >"$OUT_DIR/$file"
}

# `source add` returns once the scan is queued or done; wait (bounded, ~20 s)
# until both sources' files are searchable.
indexed=""
for _ in $(seq 1 100); do
  if ctl search "atlas outside" 2>/dev/null | head -n 1 | grep -qF "$TARGET_ID" \
     && ctl search "outside.rs" 2>/dev/null | grep -qF '@tephra:crates/atlas/outside.rs'; then
    indexed=1
    break
  fi
  sleep 0.2
done
[ -n "$indexed" ] || fail "sources were not indexed in time"

echo "== transcripts -> $OUT_DIR"
transcript 01-search-atlas-outside.txt ctl search "atlas outside"
transcript 02-search-atlas-outside-json.txt ctl search "atlas outside" --json
transcript 03-search-empty.txt ctl search ""
transcript 04-search-hash.txt ctl search "#x"

# Offline: the `notes` root goes missing. The daemon reports it offline from
# the cache, so no rescan is needed; the results must still be returned.
mv "$NOTES" "$NOTES.gone"
transcript 05-search-offline.txt ctl search "atlas outside" --json
mv "$NOTES.gone" "$NOTES"

echo "== checks"
head -n 1 "$OUT_DIR/01-search-atlas-outside.txt" | grep -qF "$TARGET_ID" \
  || fail "GP4: the target is not on line 1 of 01-search-atlas-outside.txt"
grep -q '^exit: 0$' "$OUT_DIR/01-search-atlas-outside.txt" || fail "GP4 search exited non-zero"
grep -q '^exit: 0$' "$OUT_DIR/02-search-atlas-outside-json.txt" || fail "--json search exited non-zero"
grep -q '^exit: [1-9]' "$OUT_DIR/03-search-empty.txt" || fail 'search "" must exit non-zero'
grep -q '^exit: [1-9]' "$OUT_DIR/04-search-hash.txt" || fail 'search "#x" must exit non-zero'
grep -qF 'pick a file first' "$OUT_DIR/04-search-hash.txt" || fail '"#x": no "pick a file first" message'
python3 - "$OUT_DIR/05-search-offline.txt" "$TARGET_ID" <<'PY' || fail "offline transcript: no offline: true result for the target"
import json, sys
text = open(sys.argv[1], encoding="utf-8").read()
body = text.rsplit("\nexit:", 1)[0]
hits = json.loads(body)
hit = next((h for h in hits if h["id"] == sys.argv[2]), None)
sys.exit(0 if hit and hit["offline"] is True else 1)
PY

echo "pr-8.sh: OK (transcripts in $OUT_DIR)"
