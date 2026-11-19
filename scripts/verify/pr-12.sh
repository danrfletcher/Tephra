#!/usr/bin/env bash
# PR-12 container verification: `source.promote` and `tephractl source promote`
# (GP10, E13, F10).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812), registers
# `tephra` (a copy of the GP10 fixture tree) and `vault` (a note that links into
# it), and saves each transcript to verify/pr-12/*.txt (override the folder
# with OUT_DIR):
#
#   01-promote.txt        GP10 command output, exit 0, then `source list`
#   02-ls.txt             `ls tephra crates` without tephrad; `ls tephrad src`
#                         with registry.rs
#   03-vault-link.txt     the vault note before and after; `link resolve` works
#   04-inside-links.txt   a file inside the folder before and after
#   05-push.txt           the graph:changed frame a WebSocket client received,
#                         and the seconds elapsed
#   06-refusals.txt       each E13 refusal with the tree unchanged (checksums
#                         equal) and `source add` of a nested root still refused
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the WebSocket
# listener), records both PIDs and stops only those PIDs on exit.
#
# `source_offline` (an E13 reason) cannot be shown from the CLI: a source whose
# root is gone has no folder left to name, so the path is refused as
# `not_a_folder` first. The Rust test `promote_refusals::source_offline` covers
# it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-12}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr12-verify.XXXXXX")"
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
  echo "pr-12.sh: FAIL: $*" >&2
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

# The GP10 fixture: the Tephra layout, and a vault note that links into it.
TEPHRA="$WORK/tephra-root"
VAULT="$WORK/vault-root"
mkdir -p "$TEPHRA/crates/tephrad/src" "$TEPHRA/crates/tephra-cache/src" "$VAULT"
printf '# Tephra\n\nSee the registry in [[crates/tephrad/src/registry.rs]].\n' >"$TEPHRA/README.md"
printf '// the registry ^k3f9a2\n// scanning is in [[scan.rs]], the docs in [[README]], the cache in [[crates/tephra-cache/src/lib.rs]]\npub fn registry() {}\n' \
  >"$TEPHRA/crates/tephrad/src/registry.rs"
printf '// the scan, back to [[registry.rs]] and [[registry.rs#^k3f9a2]]; outward [[README]]\npub fn scan() {}\n' \
  >"$TEPHRA/crates/tephrad/src/scan.rs"
printf '// the cache ^cc0001\npub fn cache() {}\n' >"$TEPHRA/crates/tephra-cache/src/lib.rs"
printf '# Design note\n\nThe registry lives at [[@tephra:crates/tephrad/src/registry.rs#^k3f9a2]].\n' >"$VAULT/Design.md"

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
ctl source add tephra "$TEPHRA"
ctl source add vault "$VAULT"

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

# Everything below must see the files as they were before the promote.
tree_sums() { (cd "$WORK" && find tephra-root vault-root -type f | LC_ALL=C sort | xargs shasum); }

# Wait (bounded) until the vault link resolves, i.e. both sources are indexed.
indexed=""
for _ in $(seq 1 100); do
  if ctl link resolve --source vault '[[@tephra:crates/tephrad/src/registry.rs]]' >/dev/null 2>&1; then
    indexed=1
    break
  fi
  sleep 0.2
done
[ -n "$indexed" ] || fail "sources were not indexed in time"

BEFORE_VAULT="$(cat "$VAULT/Design.md")"
BEFORE_REGISTRY="$(cat "$TEPHRA/crates/tephrad/src/registry.rs")"
BEFORE_SCAN="$(cat "$TEPHRA/crates/tephrad/src/scan.rs")"

# ---- 06 (first half): refusals, before anything changes -------------------

: >"$OUT_DIR/06-refusals.txt"
SUMS_BEFORE="$(tree_sums)"
LIST_BEFORE="$(ctl source list)"
{
  echo "tree before:"
  echo "$SUMS_BEFORE"
  echo
} >>"$OUT_DIR/06-refusals.txt"

refuse() { # <reason> <command words...>
  local reason="$1"
  shift
  local out code=0
  out="$("$@" 2>&1)" || code=$?
  {
    printf '$ %s\n%s\nexit: %s\n\n' "$*" "$out" "$code"
  } >>"$OUT_DIR/06-refusals.txt"
  [ "$code" -ne 0 ] || fail "$reason: the refusal exited 0"
  case "$out" in
    *"(reason: $reason)"*) ;;
    *) fail "$reason: not in the output: $out" ;;
  esac
}

mkdir -p "$WORK/elsewhere/folder"
refuse is_a_source_root ctl source promote x "$VAULT"
refuse is_a_source_root ctl source promote x "$TEPHRA"
refuse not_inside_a_source ctl source promote x "$WORK/elsewhere/folder"
refuse not_a_folder ctl source promote x "$TEPHRA/README.md"
refuse not_a_folder ctl source promote x "$TEPHRA/crates/missing"
refuse name_taken ctl source promote Vault "$TEPHRA/crates/tephrad"
refuse name_taken ctl source promote TEPHRA "$TEPHRA/crates/tephrad"

# A name the CLI accepts but the daemon refuses is not reachable (both check
# the same rules); the daemon's own answer is shown through the raw call.
TOKEN="$(cat "$XDG_DATA_HOME/tephra/token")"
raw_out="$(curl -sS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"source.promote\",\"params\":{\"name\":\"has space\",\"root\":\"$TEPHRA/crates/tephrad\"}}" \
  "http://$ADDR/rpc")"
printf '$ source.promote {"name": "has space", ...} over HTTP\n%s\n\n' "$raw_out" >>"$OUT_DIR/06-refusals.txt"
case "$raw_out" in
  *'"reason":"invalid_name"'*) ;;
  *) fail "invalid_name: not in the reply: $raw_out" ;;
esac

SUMS_AFTER_REFUSALS="$(tree_sums)"
[ "$SUMS_BEFORE" = "$SUMS_AFTER_REFUSALS" ] || fail "a refusal changed a file"
[ "$LIST_BEFORE" = "$(ctl source list)" ] || fail "a refusal changed the source list"

# ---- the WebSocket listener, then the promote -----------------------------

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

s.settimeout(60)
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
        open(out, "w").write(json.dumps({"received_at": time.time(), "frame": frame}))
        break
PY
python3 "$WORK/listen.py" "$ADDR" "$XDG_DATA_HOME/tephra/token" "$WORK/ws-ready" "$WORK/push.json" \
  >"$WORK/listen.log" 2>&1 &
LISTENER_PID=$!
for _ in $(seq 1 50); do
  [ -f "$WORK/ws-ready" ] && break
  kill -0 "$LISTENER_PID" 2>/dev/null || { cat "$WORK/listen.log" >&2; fail "the WebSocket listener exited early"; }
  sleep 0.1
done
[ -f "$WORK/ws-ready" ] || fail "the WebSocket listener never connected"

# ---- 01: the promote -------------------------------------------------------

: >"$OUT_DIR/01-promote.txt"
STARTED="$(python3 -c 'import time; print(time.time())')"
transcript 01-promote.txt ctl source promote tephrad "$TEPHRA/crates/tephrad"
transcript 01-promote.txt ctl source list
grep -q '^exit: 0$' <(sed -n '1,/^exit:/p' "$OUT_DIR/01-promote.txt") || fail "the promote exited non-zero"
grep -q 'tephrad' <(sed -n '/^\$ .* source list$/,$p' "$OUT_DIR/01-promote.txt") || fail "@tephrad is not in source list"
grep -q 'Promoted' "$OUT_DIR/01-promote.txt" || fail "no success line"

# ---- 02: ls ----------------------------------------------------------------

: >"$OUT_DIR/02-ls.txt"
transcript 02-ls.txt ctl ls tephra crates
transcript 02-ls.txt ctl ls tephrad src
LS_PARENT="$(ctl ls tephra crates)"
LS_CHILD="$(ctl ls tephrad src)"
case "$LS_PARENT" in *tephrad*) fail "ls tephra crates still lists tephrad: $LS_PARENT" ;; esac
case "$LS_PARENT" in *tephra-cache*) ;; *) fail "ls tephra crates lost tephra-cache: $LS_PARENT" ;; esac
case "$LS_CHILD" in *registry.rs*) ;; *) fail "ls tephrad src does not list registry.rs: $LS_CHILD" ;; esac

# ---- 03: the vault link ----------------------------------------------------

AFTER_VAULT="$(cat "$VAULT/Design.md")"
{
  echo "before:"
  echo "$BEFORE_VAULT"
  echo
  echo "after:"
  echo "$AFTER_VAULT"
  echo
} >"$OUT_DIR/03-vault-link.txt"
transcript 03-vault-link.txt ctl link resolve '[[@tephrad:src/registry.rs#^k3f9a2]]'
case "$AFTER_VAULT" in
  *'[[@tephrad:src/registry.rs#^k3f9a2]]'*) ;;
  *) fail "the vault link was not rewritten: $AFTER_VAULT" ;;
esac
case "$AFTER_VAULT" in *'@tephra:'*) fail "the old prefix is still there" ;; esac
ctl link resolve '[[@tephrad:src/registry.rs#^k3f9a2]]' >/dev/null || fail "the rewritten link does not resolve"

# ---- 04: links inside the folder ------------------------------------------

AFTER_REGISTRY="$(cat "$TEPHRA/crates/tephrad/src/registry.rs")"
AFTER_SCAN="$(cat "$TEPHRA/crates/tephrad/src/scan.rs")"
{
  echo "registry.rs before:"
  echo "$BEFORE_REGISTRY"
  echo
  echo "registry.rs after:"
  echo "$AFTER_REGISTRY"
  echo
  echo "scan.rs before:"
  echo "$BEFORE_SCAN"
  echo
  echo "scan.rs after:"
  echo "$AFTER_SCAN"
  echo
  echo "exit: 0"
} >"$OUT_DIR/04-inside-links.txt"
case "$AFTER_REGISTRY" in *'[[@tephra:README]]'*) ;; *) fail "the outward link did not gain @tephra:: $AFTER_REGISTRY" ;; esac
case "$AFTER_REGISTRY" in *'[[scan.rs]]'*) ;; *) fail "a self-link changed: $AFTER_REGISTRY" ;; esac
case "$AFTER_REGISTRY" in *'crates/tephrad/'*) fail "a path from the old root is left: $AFTER_REGISTRY" ;; esac
case "$AFTER_REGISTRY" in *'@tephra:crates/tephra-cache/src/lib.rs'*) ;; *) fail "the link to the cache did not gain @tephra:: $AFTER_REGISTRY" ;; esac
[ "$BEFORE_SCAN" != "$AFTER_SCAN" ] || true
case "$AFTER_SCAN" in *'[[registry.rs]] and [[registry.rs#^k3f9a2]]'*) ;; *) fail "scan.rs self-links changed: $AFTER_SCAN" ;; esac
case "$AFTER_SCAN" in *'[[@tephra:README]]'*) ;; *) fail "scan.rs outward link did not gain @tephra:: $AFTER_SCAN" ;; esac

# ---- 05: the push ----------------------------------------------------------

for _ in $(seq 1 100); do
  [ -f "$WORK/push.json" ] && break
  sleep 0.1
done
[ -f "$WORK/push.json" ] || fail "no source-list push reached the WebSocket client"
python3 - "$WORK/push.json" "$STARTED" >"$OUT_DIR/05-push.txt" <<'PY'
import json, sys
got = json.load(open(sys.argv[1]))
elapsed = got["received_at"] - float(sys.argv[2])
params = got["frame"]["params"]
print(json.dumps(got["frame"], indent=2, sort_keys=True))
print(f"seconds from the start of the promote to the push: {elapsed:.2f}")
if params["sources_changed"] != [{"kind": "promoted", "name": "tephrad"}]:
    print("exit: 1")
    sys.exit("wrong sources_changed: %r" % (params["sources_changed"],))
fixes = params.get("fixes") or []
if len(fixes) != 1 or fixes[0].get("kind") != "promote" or "files_changed" not in fixes[0]:
    print("exit: 1")
    sys.exit("wrong fixes: %r" % (fixes,))
if elapsed > 10:
    print("exit: 1")
    sys.exit("the push took %.1f s" % elapsed)
print("exit: 0")
PY

# ---- 06 (second half): F10 and a second promote ----------------------------

SUMS_NOW="$(tree_sums)"
{
  echo "tree after the promote (the files differ only where a link was rewritten):"
  echo "$SUMS_NOW"
  echo
} >>"$OUT_DIR/06-refusals.txt"
# After the promote, a root nested in a registered one is still refused by
# `source add`, as are the folder's own subfolders and the ancestor of both.
for nested in "$TEPHRA/crates/tephra-cache" "$TEPHRA/crates/tephrad/src" "$WORK"; do
  code=0
  out="$(ctl source add nested "$nested" 2>&1)" || code=$?
  printf '$ source add nested %s\n%s\nexit: %s\n\n' "$nested" "$out" "$code" >>"$OUT_DIR/06-refusals.txt"
  [ "$code" -ne 0 ] || fail "source add of a nested root succeeded: $nested"
done
refuse is_a_source_root ctl source promote again "$TEPHRA/crates/tephrad"
echo "exit: 0" >>"$OUT_DIR/06-refusals.txt"

# A transcript is only good if it ends with its exit line.
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-12 verification passed; transcripts in $OUT_DIR"
