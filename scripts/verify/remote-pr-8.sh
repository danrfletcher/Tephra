#!/usr/bin/env bash
# PR-8 container verification: the cache limit, eviction, and the per-source
# cache flag (G12, G20, GP8, E4, E5, E22, F2, F12).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812) against the
# PR-3 fake GitHub (`tephra-remote/examples/poll_fake`, loopback only, driven
# through a pipe), and saves each transcript to verify/remote-pr-8/*.txt
# (override the folder with OUT_DIR). The daemon is told `cache_idle_seconds =
# 8` (a test-only config key) so the 10-minute window takes 8 seconds; the
# real window is covered by the unit tests with a fake clock.
#
#   01-limit.txt    `cache set-limit 1MB` with 6 MB fetched in three files:
#                   `over limit` while they are young, and after the window the
#                   least recently used file is back to `remote`, under the
#                   limit, with `ls` listing the same names
#   02-pins.txt     a pinned folder and a file with an unsent edit stay on
#                   disk under `cache set-limit 0`; the unpinned file goes
#   03-off.txt      `cache off g`: a fetched file is dropped after the window
#                   although the cache is far under the limit; pinned files
#                   stay; `cache on g` ends the drops
#   04-refused.txt  a limit that does not parse, and an unknown source: exit 1
#                   with the reason, the old limit kept
#   05-restart.txt  the limit and the flag survive a restart
#   06-nothing-else.txt eviction sent nothing to the server: no request that
#                   was not a GET, and no `.git` in the cache
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the fake),
# records their PIDs and stops only those PIDs on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/remote-pr-8}"
# Waiting for a poll: one interval plus the time a poll takes.
POLL_WAIT="${POLL_WAIT:-100}"
# The idle window the daemon is told to use (the real one is 600 s).
IDLE="${IDLE:-8}"
# A made-up sign-in: the fake accepts any token. It must appear nowhere.
TOKEN="ghp_verifyScriptSentinelNotARealToken0000"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/remote-pr8-verify.XXXXXX")"
DAEMON_PID=""
FAKE_PID=""

cleanup() {
  exec 7>&- 2>/dev/null || true
  for pid in "$DAEMON_PID" "$FAKE_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "remote-pr-8.sh: FAIL: $*" >&2
  exit 1
}

echo "== build"
FEATURES="tephrad/test-credential-store,tephractl/test-credential-store,tephra-remote/test-support"
(cd "$ROOT" && cargo build -p tephrad -p tephractl -p tephra-remote --features "$FEATURES" --bins --example poll_fake)
# Copies, so a `cargo test` run in the same checkout while this script waits
# cannot replace the daemon (it would be built without the test credential
# store) before the restart of step 05.
mkdir -p "$WORK/bin"
cp "$ROOT/target/debug/tephrad" "$ROOT/target/debug/tephractl" "$ROOT/target/debug/examples/poll_fake" "$WORK/bin/"
TEPHRAD="$WORK/bin/tephrad"
TEPHRACTL="$WORK/bin/tephractl"
FAKE="$WORK/bin/poll_fake"

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
[ "$PORT" != "4812" ] || fail "picked the product's default port"
ADDR="127.0.0.1:$PORT"

# ---- the fake, steered through a pipe ---------------------------------------

mkfifo "$WORK/fake.in"
"$FAKE" "$WORK/url" "$WORK/requests.log" <"$WORK/fake.in" >"$WORK/fake.out" 2>"$WORK/fake.err" &
FAKE_PID=$!
exec 7>"$WORK/fake.in"
echo "fake pid $FAKE_PID"
for _ in $(seq 1 100); do
  [ -s "$WORK/url" ] && break
  kill -0 "$FAKE_PID" 2>/dev/null || { cat "$WORK/fake.err" >&2; fail "the fake exited early"; }
  sleep 0.1
done
[ -s "$WORK/url" ] || fail "the fake never wrote its URL"
GITHUB_URL="$(cat "$WORK/url")"
case "$GITHUB_URL" in
  http://127.0.0.1:*) ;;
  *) fail "the fake is not on loopback: $GITHUB_URL" ;;
esac

# fake <command...>: one command, and wait (bounded) for its answer line.
fake() {
  local before after
  before="$(wc -l <"$WORK/fake.out")"
  echo "$*" >&7
  for _ in $(seq 1 100); do
    after="$(wc -l <"$WORK/fake.out")"
    [ "$after" -gt "$before" ] && break
    sleep 0.1
  done
  [ "$after" -gt "$before" ] || fail "the fake did not answer: $*"
  tail -n 1 "$WORK/fake.out" | grep -q '^ok ' || fail "the fake refused: $(tail -n 1 "$WORK/fake.out")"
}

# ---- tephrad ----------------------------------------------------------------

export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
github_api_url = "$GITHUB_URL"
test_credential_file = "credentials.json"
cache_idle_seconds = $IDLE
EOF

ctl() { "$TEPHRACTL" --addr "$ADDR" "$@"; }

start_daemon() {
  "$TEPHRAD" --config "$XDG_CONFIG_HOME/tephra/config.toml" >>"$WORK/tephrad.log" 2>&1 &
  DAEMON_PID=$!
  echo "tephrad pid $DAEMON_PID"
  local ready=""
  for _ in $(seq 1 100); do
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/tephrad.log" >&2; fail "tephrad exited early"; }
    if ctl source list >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.2
  done
  [ -n "$ready" ] || fail "tephrad did not answer on $ADDR"
}

stop_daemon() {
  kill "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}

echo "== start tephrad on $ADDR"
start_daemon
# `auth` commands take no --addr: they read the same config.toml.
printf '%s\n' "$TOKEN" | "$TEPHRACTL" auth github - >"$WORK/auth.out" 2>&1 || { cat "$WORK/auth.out" >&2; fail "sign-in failed"; }

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.txt

# transcript <file> <command words...>: the command line, its output and its
# exit code, in that order, appended to <file>.
transcript() {
  local file="$1"
  shift
  {
    local code=0
    local out
    printf '$ %s\n' "$*"
    out="$("$@" 2>&1)" || code=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$code"
  } >>"$OUT_DIR/$file"
  return 0
}

# until_list <seconds> <extended regexp> : wait (bounded) until `source list`
# matches.
until_list() {
  local seconds="$1" pattern="$2" i
  for i in $(seq 1 "$seconds"); do
    if ctl source list 2>/dev/null | grep -Eq "$pattern"; then return 0; fi
    sleep 1
  done
  return 1
}

# until_ls <seconds> <present|absent> <name> : wait (bounded) until `ls g -r`
# lists (or no longer lists) a file (`ls -r` prints names, not paths).
until_ls() {
  local seconds="$1" want="$2" name="$3" i
  for i in $(seq 1 "$seconds"); do
    if ctl ls g -r 2>/dev/null | grep -q "$name"; then
      [ "$want" = present ] && return 0
    else
      [ "$want" = absent ] && return 0
    fi
    sleep 1
  done
  return 1
}


# ---- helpers for this PR ----------------------------------------------------

REMOTE_DIR="$XDG_DATA_HOME/tephra/remote/g"
DB="$XDG_DATA_HOME/tephra/cache.db"
state_of() { ctl ls g -r | grep -F "$1" | grep -Eo '(remote|downloading|cached|pending_upload)' | head -n 1; }
# until_state <seconds> <name> <state>: wait (bounded) for a file's state.
until_state() {
  local seconds="$1" name="$2" want="$3" i
  for i in $(seq 1 "$seconds"); do
    [ "$(state_of "$name")" = "$want" ] && return 0
    sleep 1
  done
  return 1
}

transcript 00-setup.txt ctl source add github g danrfletcher/atlas
until_list 60 '^g[[:space:]]+active' || { ctl source list >&2; fail "g did not become active"; }
rm -f "$OUT_DIR/00-setup.txt"

# ---- 01: the limit ----------------------------------------------------------

: >"$OUT_DIR/01-limit.txt"
LISTING_BEFORE="$(ctl ls g -r | awk '{print $NF}' | LC_ALL=C sort)"
# data/big.bin (6 MB) first, so it is the least recently used.
for f in data/big.bin README.md src/lib.rs; do ctl fetch "@g:$f" >/dev/null || fail "fetch $f failed"; sleep 1; done
transcript 01-limit.txt ctl cache set-limit 1MB
transcript 01-limit.txt ctl cache status
ctl cache status | grep -q '^over limit' || fail "cache status does not say over limit while the files are young"
[ "$(state_of big.bin)" = "cached" ] || fail "a file used moments ago was dropped"
echo "(waiting for the idle window: $IDLE s)" >>"$OUT_DIR/01-limit.txt"
sleep $((IDLE + 2))
transcript 01-limit.txt ctl cache set-limit 1MB
transcript 01-limit.txt ctl cache status
transcript 01-limit.txt ctl ls g -r
ctl cache status | grep -q '^over limit' && fail "still over limit after the window"
until_state 20 big.bin remote || fail "big.bin is not back to remote: $(ctl ls g -r)"
[ ! -e "$REMOTE_DIR/clone/data/big.bin" ] || fail "big.bin is still on disk"
[ -z "$(find "$REMOTE_DIR/clone/data" -maxdepth 0 -empty 2>/dev/null)" ] || fail "the empty folder data/ was left behind"
[ "$(ctl ls g -r | awk '{print $NF}' | LC_ALL=C sort)" = "$LISTING_BEFORE" ] || fail "the listing changed"
ctl cache status | grep -Eq '^\.git +0 bytes' || fail "the .git line is missing"
ctl fetch @g:data/big.bin >/dev/null || fail "a dropped file could not be fetched again"
[ -f "$REMOTE_DIR/clone/data/big.bin" ] || fail "the refetch left no file"
printf 'exit: 0\n' >>"$OUT_DIR/01-limit.txt"

# ---- 02: pins and unsent edits ------------------------------------------------

: >"$OUT_DIR/02-pins.txt"
ctl cache set-limit 10GB >/dev/null
ctl fetch @g:docs/Guide.md >/dev/null
sqlite3 "$DB" "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('g', 'docs', 1, 1795132000); UPDATE remote_items SET state = 'pending_upload' WHERE source = 'g' AND path = 'README.md';" || fail "could not seed a pin and an unsent edit"
sleep $((IDLE + 2))
transcript 02-pins.txt ctl cache set-limit 0
transcript 02-pins.txt ctl cache status
transcript 02-pins.txt ctl ls g -r
until_state 20 big.bin remote || fail "the unpinned file was not dropped"
[ -f "$REMOTE_DIR/clone/docs/Guide.md" ] || fail "a file under a pinned folder was dropped"
[ -f "$REMOTE_DIR/clone/README.md" ] || fail "a file with an unsent edit was dropped"
[ "$(state_of README.md)" = "pending_upload" ] || fail "the unsent edit changed state"
ctl cache status | grep -q '^warning:' || fail "no warning although pinned files are over the limit"
printf 'exit: 0\n' >>"$OUT_DIR/02-pins.txt"
sqlite3 "$DB" "DELETE FROM remote_pins WHERE source = 'g'; UPDATE remote_items SET state = 'cached' WHERE source = 'g' AND path = 'README.md';"
ctl cache set-limit 10GB >/dev/null

# ---- 03: cache off ----------------------------------------------------------

: >"$OUT_DIR/03-off.txt"
ctl fetch @g:src/lib.rs >/dev/null
transcript 03-off.txt ctl cache off g
transcript 03-off.txt ctl source list
ctl source list | grep -q 'cache off' || fail "source list does not show cache off"
# The pass runs every 30 s; wait for one.
until_state 50 lib.rs remote || fail "lib.rs was not dropped with the cache off: $(ctl ls g -r)"
transcript 03-off.txt ctl ls g -r
ctl cache status | grep -q '^over limit' && fail "over limit with a 10GB limit"
transcript 03-off.txt ctl cache on g
ctl fetch @g:src/lib.rs >/dev/null
sleep $((IDLE + 35))
[ "$(state_of lib.rs)" = "cached" ] || fail "lib.rs was dropped with the cache on and the cache under its limit"
transcript 03-off.txt ctl ls g -r
printf 'exit: 0\n' >>"$OUT_DIR/03-off.txt"

# ---- 04: refusals -------------------------------------------------------------

: >"$OUT_DIR/04-refused.txt"
for bad in abc -5MB 1.5.2GB 10; do
  CODE=0; OUT="$(ctl cache set-limit "$bad" 2>&1)" || CODE=$?
  { printf '$ tephractl cache set-limit %s\n%s\nexit: %s\n' "$bad" "$OUT" "$CODE"; } >>"$OUT_DIR/04-refused.txt"
  [ "$CODE" != "0" ] || fail "set-limit $bad exited 0"
done
ctl cache status | grep -q '^limit  *10GB' || fail "the old limit was not kept: $(ctl cache status)"
CODE=0; OUT="$(ctl cache off nope 2>&1)" || CODE=$?
{ printf '$ tephractl cache off nope\n%s\nexit: %s\n' "$OUT" "$CODE"; } >>"$OUT_DIR/04-refused.txt"
[ "$CODE" != "0" ] || fail "cache off nope exited 0"
printf 'exit: 0\n' >>"$OUT_DIR/04-refused.txt"

# ---- 05: restart --------------------------------------------------------------

: >"$OUT_DIR/05-restart.txt"
ctl cache set-limit "2GB" >/dev/null
ctl cache off g >/dev/null
stop_daemon
start_daemon
transcript 05-restart.txt ctl cache status
transcript 05-restart.txt ctl source list
ctl cache status | grep -q '^limit  *2GB' || fail "the limit did not survive the restart"
ctl source list | grep -q 'cache off' || fail "the flag did not survive the restart"
grep -q '^cache_limit' "$XDG_CONFIG_HOME/tephra/config.toml" || fail "the limit is not in config.toml"
printf 'exit: 0\n' >>"$OUT_DIR/05-restart.txt"

# ---- 06: nothing was sent to the remote --------------------------------------------

: >"$OUT_DIR/06-nothing-else.txt"
fake dump
{
  echo "requests that were not a GET:"
  grep -v '^GET ' "$WORK/requests.log" || true
  echo "(none expected)"
  echo ".git folders under the cache:"
  find "$XDG_DATA_HOME/tephra/remote" -name '.git' || true
} >>"$OUT_DIR/06-nothing-else.txt"
if grep -v '^GET ' "$WORK/requests.log" | grep -q .; then fail "a request that was not a GET reached the server"; fi
[ -z "$(find "$XDG_DATA_HOME/tephra/remote" -name '.git')" ] || fail "a .git folder was made"
printf 'exit: 0\n' >>"$OUT_DIR/06-nothing-else.txt"

# ---- the token is nowhere -----------------------------------------------------

if grep -rq "$TOKEN" "$OUT_DIR" "$WORK/tephrad.log" "$WORK/fake.out" "$WORK/requests.log" "$WORK/auth.out" 2>/dev/null; then
  fail "the token appears in a log, a transcript or an answer"
fi
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-8 verification passed; transcripts in $OUT_DIR"
