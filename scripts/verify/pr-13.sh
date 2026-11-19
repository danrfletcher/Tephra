#!/usr/bin/env bash
# PR-13 container verification: `source.unpromote` and
# `tephractl source unpromote` (GP11, G12).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812), registers
# `notes` (a vault note that links into the repo) and `tephra` (a copy of the
# GP10 fixture tree), promotes `crates/tephrad`, and saves each transcript to
# verify/pr-13/*.txt (override the folder with OUT_DIR):
#
#   01-promoted.txt     before: `source list` shows tephrad; the vault note
#                       holds [[@tephrad:src/registry.rs#^k3f9a2]]
#   02-unpromote.txt    `tephractl source unpromote tephrad`, exit 0, files
#                       changed reported
#   03-list.txt         `source list` without tephrad; `ls tephra
#                       crates/tephrad` shows its files in tephra again
#   04-vault-link.txt   the vault note before and after: the original
#                       [[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]
#   05-fixes.txt        `tephractl fixes`: the unpromote entry newest, the
#                       promote entry below it
#   06-push.txt         the one graph:changed a WebSocket client received for
#                       the unpromote, with sources_changed: unpromoted
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the WebSocket
# listener), records both PIDs and stops only those PIDs on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-13}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr13-verify.XXXXXX")"
DAEMON_PID=""
LISTENER_PID=""

cleanup() {
  for pid in "$LISTENER_PID" "$DAEMON_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "pr-13.sh: FAIL: $*" >&2
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
[ "$PORT" != "4812" ] || fail "picked the product's default port"
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

# The GP10 fixture: the Tephra layout, and a vault note that links into it.
TEPHRA="$WORK/tephra-root"
NOTES="$WORK/notes-root"
mkdir -p "$TEPHRA/crates/tephrad/src" "$TEPHRA/crates/tephra-cache/src" "$NOTES"
printf '# Tephra\n\nSee the registry in [[crates/tephrad/src/registry.rs]].\n' >"$TEPHRA/README.md"
printf '// the registry ^k3f9a2\n// scanning is in [[scan.rs]], the docs in [[README]], the cache in [[crates/tephra-cache/src/lib.rs]]\npub fn registry() {}\n' \
  >"$TEPHRA/crates/tephrad/src/registry.rs"
printf '// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]; outward [[README]]\npub fn scan() {}\n' \
  >"$TEPHRA/crates/tephrad/src/scan.rs"
printf '// the cache ^cc0001\npub fn cache() {}\n' >"$TEPHRA/crates/tephra-cache/src/lib.rs"
printf '# Design note\n\nThe registry lives at [[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]].\n' >"$NOTES/Design.md"

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

echo "== add the two sources and promote crates/tephrad"
ctl source add tephra "$TEPHRA"
ctl source add notes "$NOTES"

# Wait (bounded) until the vault link resolves, i.e. both sources are indexed.
indexed=""
for _ in $(seq 1 100); do
  if ctl link resolve --source notes '[[@tephra:crates/tephrad/src/registry.rs]]' >/dev/null 2>&1; then
    indexed=1
    break
  fi
  sleep 0.2
done
[ -n "$indexed" ] || fail "sources were not indexed in time"

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.txt

ORIGINAL_VAULT="$(cat "$NOTES/Design.md")"
ORIGINAL_REGISTRY="$(cat "$TEPHRA/crates/tephrad/src/registry.rs")"
ORIGINAL_SCAN="$(cat "$TEPHRA/crates/tephrad/src/scan.rs")"

ctl source promote tephrad "$TEPHRA/crates/tephrad" >"$WORK/promote.out" 2>&1 || { cat "$WORK/promote.out" >&2; fail "the setup promote failed"; }

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

# ---- 01: before -------------------------------------------------------------

PROMOTED_VAULT="$(cat "$NOTES/Design.md")"
: >"$OUT_DIR/01-promoted.txt"
transcript 01-promoted.txt ctl source list
{
  echo "vault note (Design.md):"
  echo "$PROMOTED_VAULT"
  echo "exit: 0"
} >>"$OUT_DIR/01-promoted.txt"
ctl source list | grep -q 'tephrad' || fail "tephrad is not in source list before the unpromote"
case "$PROMOTED_VAULT" in
  *'[[@tephrad:src/registry.rs#^k3f9a2]]'*) ;;
  *) fail "the vault note does not hold the promoted link: $PROMOTED_VAULT" ;;
esac

# ---- the WebSocket listener, then the unpromote -----------------------------

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

# Every source-list push is appended, one JSON line each; the script counts
# them. The script stops this process when it is done.
while True:
    b1, b2 = need(2)
    opcode, length = b1 & 0x0F, b2 & 0x7F
    if length == 126:
        length = struct.unpack(">H", need(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", need(8))[0]
    if b2 & 0x80:
        need(4)  # a server never masks; skip if it did
    payload = need(length)
    if opcode != 1:
        continue
    frame = json.loads(payload)
    if frame.get("method") == "graph:changed" and "sources_changed" in frame.get("params", {}):
        with open(out, "a") as f:
            f.write(json.dumps({"received_at": time.time(), "frame": frame}) + "\n")
PY
python3 "$WORK/listen.py" "$ADDR" "$XDG_DATA_HOME/tephra/token" "$WORK/ws-ready" "$WORK/push.jsonl" \
  >"$WORK/listen.log" 2>&1 &
LISTENER_PID=$!
echo "listener pid $LISTENER_PID"
for _ in $(seq 1 50); do
  [ -f "$WORK/ws-ready" ] && break
  kill -0 "$LISTENER_PID" 2>/dev/null || { cat "$WORK/listen.log" >&2; fail "the WebSocket listener exited early"; }
  sleep 0.1
done
[ -f "$WORK/ws-ready" ] || fail "the WebSocket listener never connected"

# ---- 02: the unpromote ------------------------------------------------------

: >"$OUT_DIR/02-unpromote.txt"
STARTED="$(python3 -c 'import time; print(time.time())')"
transcript 02-unpromote.txt ctl source unpromote tephrad
grep -q '^exit: 0$' "$OUT_DIR/02-unpromote.txt" || fail "the unpromote exited non-zero"
grep -q 'files with links rewritten: [1-9]' "$OUT_DIR/02-unpromote.txt" || fail "the unpromote reported no files changed"

# ---- 03: the list -----------------------------------------------------------

: >"$OUT_DIR/03-list.txt"
transcript 03-list.txt ctl source list
transcript 03-list.txt ctl ls tephra crates/tephrad
LIST_AFTER="$(ctl source list)"
LS_AFTER="$(ctl ls tephra crates/tephrad)"
case "$LIST_AFTER" in *tephrad*) fail "tephrad is still in source list: $LIST_AFTER" ;; esac
case "$LS_AFTER" in *src*) ;; *) fail "ls tephra crates/tephrad lost its files: $LS_AFTER" ;; esac
ctl ls tephra crates/tephrad/src | grep -q 'registry.rs' || fail "registry.rs is not in tephra again"

# ---- 04: the vault link -----------------------------------------------------

AFTER_VAULT="$(cat "$NOTES/Design.md")"
{
  echo "before the promote:"
  echo "$ORIGINAL_VAULT"
  echo
  echo "after the unpromote:"
  echo "$AFTER_VAULT"
  echo
} >"$OUT_DIR/04-vault-link.txt"
transcript 04-vault-link.txt ctl link resolve --source notes '[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]'
[ "$AFTER_VAULT" = "$ORIGINAL_VAULT" ] || fail "the vault note is not as it was: $AFTER_VAULT"
case "$AFTER_VAULT" in *'@tephrad'*) fail "@tephrad is left in the vault note" ;; esac
[ "$(cat "$TEPHRA/crates/tephrad/src/registry.rs")" = "$ORIGINAL_REGISTRY" ] || fail "registry.rs is not as it was"
[ "$(cat "$TEPHRA/crates/tephrad/src/scan.rs")" = "$ORIGINAL_SCAN" ] || fail "scan.rs is not as it was"
ctl link resolve --source notes '[[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]]' >/dev/null || fail "the restored link does not resolve"

# ---- 05: the fixes ----------------------------------------------------------

: >"$OUT_DIR/05-fixes.txt"
transcript 05-fixes.txt ctl fixes
FIXES="$(ctl fixes)"
UNPROMOTE_LINE="$(printf '%s\n' "$FIXES" | grep -n ' unpromote ' | head -n 1 | cut -d: -f1)"
PROMOTE_LINE="$(printf '%s\n' "$FIXES" | grep -n ' promote ' | head -n 1 | cut -d: -f1)"
[ -n "$UNPROMOTE_LINE" ] || fail "no unpromote entry in fixes: $FIXES"
[ -n "$PROMOTE_LINE" ] || fail "no promote entry in fixes: $FIXES"
[ "$UNPROMOTE_LINE" -lt "$PROMOTE_LINE" ] || fail "the unpromote entry is not above the promote entry: $FIXES"

# ---- 06: the push -----------------------------------------------------------

# Wait (bounded) for the push, then a little longer to be sure there is only one.
for _ in $(seq 1 100); do
  [ -s "$WORK/push.jsonl" ] && break
  sleep 0.1
done
[ -s "$WORK/push.jsonl" ] || fail "no source-list push reached the WebSocket client"
sleep 2
python3 - "$WORK/push.jsonl" "$STARTED" >"$OUT_DIR/06-push.txt" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
started = float(sys.argv[2])
if len(rows) != 1:
    for r in rows:
        print(json.dumps(r["frame"], indent=2, sort_keys=True))
    print("exit: 1")
    sys.exit("expected exactly one source-list push, got %d" % len(rows))
got = rows[0]
elapsed = got["received_at"] - started
params = got["frame"]["params"]
print(json.dumps(got["frame"], indent=2, sort_keys=True))
print(f"seconds from the start of the unpromote to the push: {elapsed:.2f}")
if params["sources_changed"] != [{"kind": "unpromoted", "name": "tephrad"}]:
    print("exit: 1")
    sys.exit("wrong sources_changed: %r" % (params["sources_changed"],))
fixes = params.get("fixes") or []
if len(fixes) != 1 or fixes[0].get("kind") != "unpromote" or "files_changed" not in fixes[0]:
    print("exit: 1")
    sys.exit("wrong fixes: %r" % (fixes,))
if elapsed > 10:
    print("exit: 1")
    sys.exit("the push took %.1f s" % elapsed)
print("exit: 0")
PY

# A transcript is only good if it ends with its exit line.
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-13 verification passed; transcripts in $OUT_DIR"
