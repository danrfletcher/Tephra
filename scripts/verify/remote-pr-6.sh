#!/usr/bin/env bash
# PR-6 container verification: a GitHub source kept current by polling, and
# kept usable offline (G5, G6, G18, E8, E13).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir,
# a free loopback port it picks itself, never the default 4812) against the
# PR-3 fake GitHub (`tephra-remote/examples/poll_fake`, loopback only, driven
# through a pipe), and saves each transcript to verify/remote-pr-6/*.txt
# (override the folder with OUT_DIR):
#
#   01-add.txt         the fake repo is added; `source list` shows active,
#                      0 bytes cached, and every file `remote`
#   02-change.txt      a file is changed on the server; within one poll
#                      interval the index shows its new heading and a
#                      WebSocket client received exactly one graph:changed
#   03-offline.txt     the server's connection is cut: `source list` shows
#                      offline with the reason while `ls` still lists; once it
#                      is back the source is active again with no tree or
#                      blob read
#   04-delete.txt      a file is deleted on the server: it leaves `ls` and a
#                      link to it is unresolved
#   05-restart.txt     tephrad is restarted after a complete index: the list
#                      answers at once and the request log has no tree and no
#                      blob read, and the first poll carries the stored ETag
#   06-moved.txt       the server answers 301 for the repo: `source list`
#                      shows needs_attention with the new location, every file
#                      still listed
#
# The poll interval is 60 s of real time, so a whole run takes about six
# minutes. Every transcript ends with an "exit: <code>" line. The script exits
# non-zero if any result is wrong. It starts three processes (tephrad, the
# fake and the WebSocket listener), records their PIDs and stops only those
# PIDs on exit. With RUN_IGNORED=1 it also runs the 10,000-file first index
# test (`cargo test -p tephrad --test remote_resume -- --ignored`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/remote-pr-6}"
# Waiting for a poll: one interval plus the time a poll takes.
POLL_WAIT="${POLL_WAIT:-100}"
# A made-up sign-in: the fake accepts any token. It must appear nowhere.
TOKEN="ghp_verifyScriptSentinelNotARealToken0000"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/remote-pr6-verify.XXXXXX")"
DAEMON_PID=""
LISTENER_PID=""
FAKE_PID=""

cleanup() {
  exec 7>&- 2>/dev/null || true
  for pid in "$LISTENER_PID" "$DAEMON_PID" "$FAKE_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "remote-pr-6.sh: FAIL: $*" >&2
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

# ---- the WebSocket listener: every graph:changed, one JSON line each ------------------

cat >"$WORK/listen.py" <<'PY'
import base64, json, os, socket, struct, sys, time

host, port = sys.argv[1].split(":")
token = open(sys.argv[2]).read().strip()
ready, out = sys.argv[3], sys.argv[4]

s = socket.create_connection((host, int(port)))
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((
    f"GET /rpc HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
    f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
    f"Sec-WebSocket-Version: 13\r\nAuthorization: Bearer {token}\r\n\r\n"
).encode())
head = b""
while b"\r\n\r\n" not in head:
    chunk = s.recv(4096)
    if not chunk:
        raise SystemExit("handshake closed")
    head += chunk
head, rest = head.split(b"\r\n\r\n", 1)
if b" 101 " not in head.split(b"\r\n")[0]:
    raise SystemExit("handshake refused: " + head.decode(errors="replace"))
open(ready, "w").write("ready")

buf = rest
def need(n):
    global buf
    while len(buf) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise SystemExit("closed")
        buf += chunk
    data, buf = buf[:n], buf[n:]
    return data

while True:
    b1, b2 = need(2)
    opcode, length = b1 & 0x0F, b2 & 0x7F
    if length == 126:
        length = struct.unpack(">H", need(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", need(8))[0]
    if b2 & 0x80:
        need(4)
    payload = need(length)
    if opcode != 1:
        continue
    frame = json.loads(payload)
    if frame.get("method") == "graph:changed":
        with open(out, "a") as f:
            f.write(json.dumps({"received_at": time.time(), "frame": frame}) + "\n")
PY
TOKEN_FILE="$XDG_DATA_HOME/tephra/token"
[ -f "$TOKEN_FILE" ] || fail "the daemon wrote no bearer token file at $TOKEN_FILE"
start_listener() {
  rm -f "$WORK/ws-ready"
  python3 "$WORK/listen.py" "$ADDR" "$TOKEN_FILE" "$WORK/ws-ready" "$WORK/push.jsonl" \
    >"$WORK/listen.log" 2>&1 &
  LISTENER_PID=$!
  echo "listener pid $LISTENER_PID"
  for _ in $(seq 1 50); do
    [ -f "$WORK/ws-ready" ] && break
    kill -0 "$LISTENER_PID" 2>/dev/null || { cat "$WORK/listen.log" >&2; fail "the WebSocket listener exited early"; }
    sleep 0.1
  done
  [ -f "$WORK/ws-ready" ] || fail "the WebSocket listener never connected"
}
pushes() { [ -f "$WORK/push.jsonl" ] && wc -l <"$WORK/push.jsonl" | tr -d ' ' || echo 0; }

# ---- 01: add ----------------------------------------------------------------

: >"$OUT_DIR/01-add.txt"
transcript 01-add.txt ctl source add github g danrfletcher/atlas
grep -q '^exit: 0$' "$OUT_DIR/01-add.txt" || fail "source add github exited non-zero"
until_list 60 '^g[[:space:]]+active' || { ctl source list >&2; fail "g did not become active"; }
start_listener
transcript 01-add.txt ctl source list
transcript 01-add.txt ctl ls g -r
LIST="$(ctl source list)"
printf '%s\n' "$LIST" | grep -q 'cached 0 bytes' || fail "the source holds bytes: $LIST"
LS="$(ctl ls g -r)"
for f in README.md Guide.md lib.rs; do
  printf '%s\n' "$LS" | grep -q "$f" || fail "$f is not listed: $LS"
done
# `ls -r` prints a folder row (kind `folder`) and a file row (its state, here
# `remote`) per entry: no file row may be in any other state.
if printf '%s\n' "$LS" | grep -v '^folder' | grep -v 'remote'; then
  fail "a file is not remote: $LS"
fi
[ ! -e "$XDG_DATA_HOME/tephra/remote" ] || [ -z "$(find "$XDG_DATA_HOME/tephra/remote" -type f 2>/dev/null)" ] || fail "a file was written under remote/"
sleep 2

# ---- 02: a change on the server --------------------------------------------

: >"$OUT_DIR/02-change.txt"
BEFORE_PUSHES="$(pushes)"
fake put docs/Guide.md "# Guide, second edition [[README]]"
transcript 02-change.txt ctl search --in @g:docs/Guide.md '#'
STARTED="$(python3 -c 'import time; print(time.time())')"
found=""
for _ in $(seq 1 "$POLL_WAIT"); do
  if ctl search --in @g:docs/Guide.md '#' 2>/dev/null | grep -q 'second edition'; then found=1; break; fi
  sleep 1
done
[ -n "$found" ] || fail "the changed heading never reached the index within $POLL_WAIT s"
transcript 02-change.txt ctl search --in @g:docs/Guide.md '#'
sleep 3
AFTER_PUSHES="$(pushes)"
{
  echo "graph:changed pushes received for the change: $((AFTER_PUSHES - BEFORE_PUSHES))"
  tail -n +"$((BEFORE_PUSHES + 1))" "$WORK/push.jsonl" | python3 -c '
import json, sys
for line in sys.stdin:
    print(json.dumps(json.loads(line)["frame"], sort_keys=True))
'
} >>"$OUT_DIR/02-change.txt"
[ "$((AFTER_PUSHES - BEFORE_PUSHES))" = "1" ] || fail "expected exactly one graph:changed, got $((AFTER_PUSHES - BEFORE_PUSHES))"
transcript 02-change.txt ctl source list
ctl source list | grep -q 'cached 0 bytes' || fail "the poll cached bytes"

# ---- 03: offline and back ---------------------------------------------------

: >"$OUT_DIR/03-offline.txt"
fake clear
fake down
until_list "$POLL_WAIT" '^g[[:space:]]+offline' || { ctl source list >&2; fail "g did not become offline within $POLL_WAIT s"; }
transcript 03-offline.txt ctl source list
transcript 03-offline.txt ctl ls g -r
ctl source list | grep -q 'note: cannot reach GitHub' || fail "no reason shown for offline"
ctl source list | grep -Eq 'last sync [0-9]' || fail "no last sync shown for offline"
ctl ls g -r | grep -q 'README.md' || fail "ls does not answer while offline"
ctl link resolve --source g '[[docs/Guide.md]]' >/dev/null || fail "a link does not resolve while offline"
fake up
until_list "$POLL_WAIT" '^g[[:space:]]+active' || { ctl source list >&2; fail "g did not return to active within $POLL_WAIT s"; }
transcript 03-offline.txt ctl source list
fake dump
{
  echo "requests the server saw from the moment the connection was cut:"
  cat "$WORK/requests.log"
} >>"$OUT_DIR/03-offline.txt"
if grep -Eq '/git/trees|/git/blobs|/contents/' "$WORK/requests.log"; then fail "recovery read a tree or a blob"; fi
grep -q '/commits/' "$WORK/requests.log" || fail "no poll reached the server after recovery"
printf 'exit: 0\n' >>"$OUT_DIR/03-offline.txt"

# ---- 04: a delete on the server ---------------------------------------------

: >"$OUT_DIR/04-delete.txt"
transcript 04-delete.txt ctl link resolve --source g '[[docs/Guide.md]]'
ctl link resolve --source g '[[docs/Guide.md]]' >/dev/null || fail "the link did not resolve before the delete"
fake remove docs/Guide.md
until_ls "$POLL_WAIT" absent 'Guide.md' || fail "docs/Guide.md is still listed after $POLL_WAIT s"
transcript 04-delete.txt ctl ls g -r
transcript 04-delete.txt ctl link resolve --source g '[[docs/Guide.md]]'
if ctl link resolve --source g '[[docs/Guide.md]]' >/dev/null 2>&1; then fail "the link to the deleted file still resolves"; fi
transcript 04-delete.txt ctl source list
ctl source list | grep -Eq '^g[[:space:]]+active' || fail "g is not active after a delete"
ctl ls g -r | grep -q 'README.md' || fail "README.md left the listing"

# ---- 05: restart ------------------------------------------------------------

: >"$OUT_DIR/05-restart.txt"
stop_daemon
kill "$LISTENER_PID" 2>/dev/null || true
wait "$LISTENER_PID" 2>/dev/null || true
LISTENER_PID=""
fake clear
start_daemon
transcript 05-restart.txt ctl source list
transcript 05-restart.txt ctl ls g -r
ctl ls g -r | grep -q 'README.md' || fail "the listing did not answer at once after the restart"
ctl source list | grep -Eq '^g[[:space:]]+active' || fail "g is not active at once after the restart"
sleep 3
fake dump
if grep -Eq '/git/trees|/git/blobs|/contents/' "$WORK/requests.log"; then fail "the restart read a tree or a blob"; fi
# The first poll: conditional, with the stored ETag, and still no read.
polled=""
for _ in $(seq 1 "$POLL_WAIT"); do
  fake dump
  if grep -q '/commits/.* if-none-match' "$WORK/requests.log"; then polled=1; break; fi
  sleep 1
done
[ -n "$polled" ] || fail "no conditional poll reached the server within $POLL_WAIT s"
{
  echo "requests the server saw since the restart:"
  cat "$WORK/requests.log"
} >>"$OUT_DIR/05-restart.txt"
if grep -Eq '/git/trees|/git/blobs|/contents/' "$WORK/requests.log"; then fail "the first poll read a tree or a blob"; fi
if grep -v '^GET ' "$WORK/requests.log" | grep -q .; then fail "a request that was not a GET reached the server"; fi
printf 'exit: 0\n' >>"$OUT_DIR/05-restart.txt"

# ---- 06: the repo moved ------------------------------------------------------

: >"$OUT_DIR/06-moved.txt"
fake move "https://api.github.com/repositories/9"
until_list "$POLL_WAIT" '^g[[:space:]]+needs_attention' || { ctl source list >&2; fail "g did not become needs_attention within $POLL_WAIT s"; }
transcript 06-moved.txt ctl source list
transcript 06-moved.txt ctl ls g -r
ctl source list | grep -q 'repositories/9' || fail "the new location is not shown"
ctl ls g -r | grep -q 'README.md' || fail "a file left the listing when the repo moved"
fake dump
if grep -v '^GET ' "$WORK/requests.log" | grep -q .; then fail "a request that was not a GET reached the server"; fi
fake unmove

# ---- the token is nowhere ---------------------------------------------------

if grep -rq "$TOKEN" "$OUT_DIR" "$WORK/tephrad.log" "$WORK/fake.out" "$WORK/requests.log" "$WORK/auth.out" "$WORK/push.jsonl" 2>/dev/null; then
  fail "the token appears in a log, a transcript or an answer"
fi

# A transcript is only good if it ends with its exit line.
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

if [ "${RUN_IGNORED:-}" = "1" ]; then
  echo "== the 10,000-file first index"
  (cd "$ROOT" && cargo test -p tephrad --test remote_resume -- --ignored --nocapture) >"$OUT_DIR/07-ten-thousand.txt" 2>&1 || fail "the 10,000-file test failed"
  printf 'exit: 0\n' >>"$OUT_DIR/07-ten-thousand.txt"
fi

echo "== PR-6 verification passed; transcripts in $OUT_DIR"
