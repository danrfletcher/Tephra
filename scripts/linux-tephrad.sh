#!/usr/bin/env bash
# Linux `tephrad` and `tephractl` for Atlas's tests (G19).
#
# Builds both binaries in the official `rust` Docker image (the same CPU
# architecture as the host: aarch64 on an Apple-silicon Mac), starts them
# inside the Linux `desktop-<slug>` webtop container that Atlas builds use,
# and runs the rename race tests there on real inotify.
#
# Nothing here edits the Atlas repo or the container user's own
# ~/.local/share/tephra: a run lives in /tmp/tephra-linux/<slug> on the
# container's own filesystem, with its own XDG_DATA_HOME, config and token.
#
# Works with the macOS bash 3.2: no associative arrays, no ${var,,}.

set -u

SELF_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$SELF_DIR/.." && pwd)
# Where `build` leaves the binaries (inside `target/`, so gitignored).
OUT_DIR=${LINUX_TEPHRAD_OUT:-$REPO/target/linux}
IMAGE=${TEPHRA_RUST_IMAGE:-rust:1-bookworm}
CARGO_VOLUME=${TEPHRA_CARGO_VOLUME:-tephra-linux-cargo}
TARGET_VOLUME=${TEPHRA_TARGET_VOLUME:-tephra-linux-target}
# A run's files, inside the container.
RUN_ROOT=/tmp/tephra-linux
# The fewest inotify watches a run needs; below it `start` refuses.
MIN_WATCHES=8192
# Longest `start` waits for the daemon to say it is listening, in tenths
# of a second (a local process, not an outside service).
START_POLLS=150

EX_USAGE=2
EX_DOWN=3

usage() {
  cat <<'EOF'
linux-tephrad.sh - Linux tephrad and tephractl for Atlas's tests

USAGE:
    scripts/linux-tephrad.sh build
    scripts/linux-tephrad.sh start <slug> [--port <n>] [--source <dir> | --no-source]
    scripts/linux-tephrad.sh stop <slug>
    scripts/linux-tephrad.sh status <slug>
    scripts/linux-tephrad.sh test [<slug>] [--repeat <n>] [--all]
    scripts/linux-tephrad.sh --help

COMMANDS:
    build    Compile `tephrad` and `tephractl` with `--release` inside the official
             `rust` image (a named volume holds the cargo cache) and write the binaries
             to target/linux/ (gitignored).
    start    Copy the binaries into the container `desktop-<slug>`, write a config, use
             a data dir of its own (XDG_DATA_HOME under /tmp/tephra-linux/<slug>), and
             start `tephrad` in the background with a token and a loopback listen
             address. Runs `tephrad --help` there first, so a binary that needs a newer
             glibc fails with that message. Run twice, it restarts its own daemon: there
             are never two. By default it also registers a test vault, source `notes`.
             Prints the exact command Atlas's tests use to reach it.
    stop     Stop only that run's `tephrad` (matched by its pid file and command line).
    status   Print whether that run's `tephrad` is up (exit 0) or down (exit 3).
    test     Build the race tests (crates/tephrad/tests/rename_races.rs) in the `rust`
             image, copy them into the container and run them there on real inotify.
             Runs the six `race_*` tests; --all also runs the edge and fence tests.
             --repeat <n> runs each test n times over (the flake check).
             The container is <slug>, or $LINUX_TEPHRAD_SLUG when no slug is given.

OPTIONS (start):
    --port <n>      Listen on this loopback port. Without it tephrad takes a free one and
                    the script reports it. A port in use is reported, never reused.
    --source <dir>  Register this folder (a path inside the container) as source `notes`
                    instead of the seeded test vault. Refused when it is on a bind mount
                    of the host: inotify does not reliably see host-made changes there.
    --no-source     Register no source.

EXIT CODES:
    0 done, 1 it ran and failed (one line says why), 2 a usage error (nothing was
    done), 3 `status`: down.
EOF
}

die() {
  echo "linux-tephrad: $*" >&2
  exit 1
}

usage_error() {
  echo "linux-tephrad: $*" >&2
  echo "try: scripts/linux-tephrad.sh --help" >&2
  exit $EX_USAGE
}

valid_slug() {
  case "$1" in
    "" | *[!A-Za-z0-9._-]* | [.-]*) return 1 ;;
  esac
  return 0
}

need_docker() {
  docker info >/dev/null 2>&1 || die "Docker is not running (docker info failed); nothing was changed"
}

ensure_image() {
  if docker image inspect "$IMAGE" >/dev/null 2>&1; then
    return 0
  fi
  docker pull -q "$IMAGE" >/dev/null 2>&1 ||
    die "cannot pull the image $IMAGE (no network, or the registry refused); nothing was changed"
}

# --- inside the container -------------------------------------------------

CONTAINER=""
RUN=""

set_run() {
  CONTAINER="desktop-$1"
  RUN="$RUN_ROOT/$1"
}

need_container() {
  local state
  state=$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null) || state=""
  [ "$state" = "true" ] || die "the container $CONTAINER is not running; nothing was changed"
}

# Runs a shell snippet in the container.
dsh() {
  docker exec "$CONTAINER" sh -c "$1"
}

# The pid of this run's tephrad when it is up: the pid file names a live
# process whose command line is exactly this run's binary and config.
running_pid() {
  dsh "p=\$(cat $RUN/tephrad.pid 2>/dev/null) || exit 1; [ -n \"\$p\" ] || exit 1; tr '\\0' ' ' < /proc/\$p/cmdline 2>/dev/null | grep -q -- '$RUN/bin/tephrad --config $RUN/xdg-config/tephra/config.toml' || exit 1; echo \$p" 2>/dev/null
}

# The address from the daemon's own "listening on" line.
listening_addr() {
  dsh "grep -o 'listening on [0-9.:]*' $RUN/tephrad.log 2>/dev/null | tail -n 1 | sed 's/listening on //'" 2>/dev/null
}

# Stops this run's tephrad by pid, only when it is ours. Prints nothing.
stop_own_daemon() {
  local pid
  pid=$(running_pid) || return 0
  [ -n "$pid" ] || return 0
  dsh "kill $pid" >/dev/null 2>&1 || true
  local i=0
  while [ $i -lt 100 ]; do
    running_pid >/dev/null || {
      dsh "rm -f $RUN/tephrad.pid" >/dev/null 2>&1
      return 0
    }
    sleep 0.1
    i=$((i + 1))
  done
  return 1
}

# Refuses a path on a mount shared from the host: host-made changes do not
# reliably raise inotify events there, so a test over it proves nothing.
refuse_host_share() {
  local path=$1 fstype
  # The nearest existing ancestor decides: the path may not be made yet.
  fstype=$(dsh "p='$path'; while [ ! -e \"\$p\" ]; do p=\$(dirname \"\$p\"); done; df -PT \"\$p\" 2>/dev/null | awk 'NR==2 {print \$2}'") || fstype=""
  case "$fstype" in
    virtiofs | 9p | grpcfuse | osxfs | vboxsf | nfs | nfs4 | cifs | fuse*)
      die "$path is on a $fstype mount (a bind mount of the host); tests need the container's own filesystem, for example /tmp; nothing was changed"
      ;;
  esac
}

# Runs `<binary> <arg>` in the container; on failure prints one line, the
# glibc message when that is why.
check_binary_runs() {
  local bin=$1 arg=$2 out
  if out=$(docker exec "$CONTAINER" "$bin" "$arg" 2>&1); then
    return 0
  fi
  local line
  line=$(printf '%s\n' "$out" | grep -i 'glibc' | head -n 1)
  if [ -n "$line" ]; then
    die "the built binary needs a newer glibc than $CONTAINER has: $line; build with an older image (TEPHRA_RUST_IMAGE) or use a newer container"
  fi
  line=$(printf '%s\n' "$out" | head -n 1)
  die "the built binary does not run in $CONTAINER: ${line:-no output} (is its CPU architecture the container's?)"
}

check_inotify_limit() {
  local watches
  watches=$(dsh "cat /proc/sys/fs/inotify/max_user_watches" 2>/dev/null) || watches=""
  case "$watches" in
    "" | *[!0-9]*) die "cannot read fs.inotify.max_user_watches in $CONTAINER; nothing was changed" ;;
  esac
  if [ "$watches" -lt $MIN_WATCHES ]; then
    die "fs.inotify.max_user_watches is $watches in $CONTAINER (need at least $MIN_WATCHES): raise it on the Docker host with sysctl; nothing was changed"
  fi
}

# --- build ----------------------------------------------------------------

# `cargo` in the rust image with the cache and target volumes; $1 is the
# shell command to run in /src (the repo, read only).
in_rust_image() {
  docker run --rm \
    -v "$REPO":/src:ro \
    -v "$CARGO_VOLUME":/cargo-cache \
    -v "$TARGET_VOLUME":/target \
    -v "$OUT_DIR":/out \
    -e CARGO_HOME=/cargo-cache \
    -e CARGO_TARGET_DIR=/target \
    -w /src \
    "$IMAGE" sh -c "$1"
}

cmd_build() {
  need_docker
  ensure_image
  mkdir -p "$OUT_DIR" || die "cannot create $OUT_DIR"
  in_rust_image 'set -e; cargo build --release --locked -p tephrad -p tephractl; cp /target/release/tephrad /target/release/tephractl /out/' ||
    die "the build failed in the $IMAGE image (the cargo output is above)"
  echo "built tephrad and tephractl (release, $(uname -m)) in $OUT_DIR"
}

# --- start / stop / status ------------------------------------------------

seed_vault() {
  # The GP1 notes: `Training Plan.md` and three files that link to it.
  dsh "mkdir -p $RUN/vault/Ideas &&
    [ -e '$RUN/vault/Training Plan.md' ] || {
      printf '# Training Plan\n\n## Goals\n\nRun.\n' > '$RUN/vault/Training Plan.md'
      printf '# Weekly Review\n\nThis week: [[Training Plan]].\n' > '$RUN/vault/Weekly Review.md'
      printf '# Goals\n\nSee [[Training Plan|plan]] first.\n' > '$RUN/vault/Goals.md'
      printf '# Rename notes\n\n![[Training Plan]]\n' > '$RUN/vault/Ideas/Rename Notes.md'
    }"
}

ctl() {
  docker exec -e "XDG_DATA_HOME=$RUN/xdg-data" "$CONTAINER" "$RUN/bin/tephractl" --addr "$ADDR" "$@"
}

cmd_start() {
  local slug=$1 port=0 source="" no_source=0
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --port)
        [ $# -ge 2 ] || usage_error "--port needs a number"
        port=$2
        shift 2
        ;;
      --source)
        [ $# -ge 2 ] || usage_error "--source needs a folder"
        source=$2
        shift 2
        ;;
      --no-source)
        no_source=1
        shift
        ;;
      *) usage_error "start: unknown option '$1'" ;;
    esac
  done
  case "$port" in
    "" | *[!0-9]*) usage_error "--port must be a number from 1 to 65535" ;;
  esac
  if [ "$port" != 0 ] && { [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; }; then
    usage_error "--port must be a number from 1 to 65535"
  fi
  if [ -n "$source" ] && [ $no_source = 1 ]; then
    usage_error "--source and --no-source cannot be used together"
  fi
  case "$source" in
    "" | /*) ;;
    *) usage_error "--source must be an absolute path inside the container" ;;
  esac

  set_run "$slug"
  need_docker
  need_container
  [ -x "$OUT_DIR/tephrad" ] && [ -x "$OUT_DIR/tephractl" ] ||
    die "no Linux binaries in $OUT_DIR: run 'scripts/linux-tephrad.sh build' first; nothing was changed"
  [ -z "$source" ] || refuse_host_share "$source"
  refuse_host_share "$RUN"
  check_inotify_limit

  # The new binaries go next to the old ones and are tried before anything
  # is replaced, so a binary that cannot run changes nothing.
  dsh "mkdir -p $RUN/bin" || die "cannot create $RUN in $CONTAINER"
  docker cp "$OUT_DIR/tephrad" "$CONTAINER:$RUN/bin/tephrad.new" >/dev/null 2>&1 ||
    die "cannot copy tephrad into $CONTAINER"
  if ! out=$(check_binary_runs "$RUN/bin/tephrad.new" --help 2>&1); then
    dsh "rm -f $RUN/bin/tephrad.new" >/dev/null 2>&1
    printf '%s\n' "$out" >&2
    exit 1
  fi
  docker cp "$OUT_DIR/tephractl" "$CONTAINER:$RUN/bin/tephractl.new" >/dev/null 2>&1 ||
    die "cannot copy tephractl into $CONTAINER"

  # Run twice, it restarts: this run's own daemon is stopped first, so
  # there are never two. Nothing else is signalled.
  stop_own_daemon || die "the earlier tephrad of this run did not stop; not starting a second"
  dsh "mv -f $RUN/bin/tephrad.new $RUN/bin/tephrad && mv -f $RUN/bin/tephractl.new $RUN/bin/tephractl && chmod +x $RUN/bin/tephrad $RUN/bin/tephractl" ||
    die "cannot install the binaries in $RUN/bin"

  # Its own config, data dir (so its own token) and HOME: the container
  # user's real ~/.local/share/tephra is never read or written.
  dsh "mkdir -p $RUN/xdg-config/tephra $RUN/xdg-data $RUN/home && rm -f $RUN/tephrad.log &&
    printf 'listen_addr = \"127.0.0.1:%s\"\ndata_dir = \"%s\"\nlog_level = \"info\"\n' '$port' '$RUN/xdg-data/tephra' > $RUN/xdg-config/tephra/config.toml" ||
    die "cannot write the config in $RUN"

  docker exec -d \
    -e "HOME=$RUN/home" -e "XDG_DATA_HOME=$RUN/xdg-data" -e "XDG_CONFIG_HOME=$RUN/xdg-config" \
    "$CONTAINER" sh -c "echo \$\$ > $RUN/tephrad.pid; exec $RUN/bin/tephrad --config $RUN/xdg-config/tephra/config.toml > $RUN/tephrad.log 2>&1" ||
    die "cannot start tephrad in $CONTAINER"

  ADDR=""
  local i=0
  while [ $i -lt $START_POLLS ]; do
    ADDR=$(listening_addr)
    [ -z "$ADDR" ] || break
    if ! running_pid >/dev/null; then
      local why
      why=$(dsh "tail -n 3 $RUN/tephrad.log 2>/dev/null" | tr '\n' ' ')
      case "$why" in
        *"in use"*) die "port $port is in use in $CONTAINER; pick another with --port, or leave it out for a free one" ;;
        *) die "tephrad exited at start: ${why:-no output} (log: $RUN/tephrad.log)" ;;
      esac
    fi
    sleep 0.2
    i=$((i + 1))
  done
  [ -n "$ADDR" ] || die "tephrad did not say it was listening (log: $CONTAINER:$RUN/tephrad.log)"
  local pid
  pid=$(running_pid) || pid="?"

  local source_note="no source registered"
  if [ $no_source = 0 ]; then
    if [ -z "$source" ]; then
      source="$RUN/vault"
      seed_vault || die "cannot seed the test vault in $RUN/vault"
    fi
    if ! ctl source list 2>/dev/null | grep -q 'notes'; then
      ctl source add notes "$source" >/dev/null 2>&1 || die "tephrad is up on $ADDR but 'source add notes $source' failed"
    fi
    source_note="source notes = $source"
  fi

  local token="$RUN/xdg-data/tephra/token"
  echo "tephrad is up in $CONTAINER: pid $pid, $ADDR, $source_note"
  echo "  address     $ADDR (loopback, inside the container)"
  echo "  token file  $token (inside the container)"
  echo "  data dir    $RUN/xdg-data/tephra, log $RUN/tephrad.log"
  echo "  reach it    docker exec -e XDG_DATA_HOME=$RUN/xdg-data $CONTAINER $RUN/bin/tephractl --addr $ADDR source list"
  echo "  raw RPC     docker exec $CONTAINER sh -c 'curl -s -H \"Authorization: Bearer \$(cat $token)\" -d \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":1,\\\"method\\\":\\\"source.list\\\",\\\"params\\\":{}}\" http://$ADDR/rpc'"
  echo "  stop it     scripts/linux-tephrad.sh stop $slug"
}

cmd_stop() {
  set_run "$1"
  need_docker
  need_container
  local pid
  pid=$(running_pid) || pid=""
  if [ -z "$pid" ]; then
    echo "tephrad is not running in $CONTAINER (nothing to stop)"
    return 0
  fi
  stop_own_daemon || die "tephrad (pid $pid) in $CONTAINER did not stop"
  echo "stopped tephrad (pid $pid) in $CONTAINER"
}

cmd_status() {
  set_run "$1"
  need_docker
  need_container
  local pid
  pid=$(running_pid) || pid=""
  if [ -z "$pid" ]; then
    echo "tephrad is down in $CONTAINER"
    return $EX_DOWN
  fi
  echo "tephrad is up in $CONTAINER: pid $pid, $(listening_addr)"
}

# --- test -----------------------------------------------------------------

cmd_test() {
  local slug="" repeat=1 all=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --repeat)
        [ $# -ge 2 ] || usage_error "--repeat needs a number"
        repeat=$2
        shift 2
        ;;
      --all)
        all=1
        shift
        ;;
      -*) usage_error "test: unknown option '$1'" ;;
      *)
        [ -z "$slug" ] || usage_error "test: one slug only"
        slug=$1
        shift
        ;;
    esac
  done
  case "$repeat" in
    "" | *[!0-9]*) usage_error "--repeat must be a whole number, 1 or more" ;;
  esac
  [ "$repeat" -ge 1 ] || usage_error "--repeat must be a whole number, 1 or more"
  [ -n "$slug" ] || slug=${LINUX_TEPHRAD_SLUG:-}
  [ -n "$slug" ] || usage_error "test: no container named; give a slug or set LINUX_TEPHRAD_SLUG"
  valid_slug "$slug" || usage_error "'$slug' is not a valid slug"

  set_run "$slug"
  need_docker
  ensure_image
  need_container
  check_inotify_limit
  refuse_host_share "$RUN/tmp"
  mkdir -p "$OUT_DIR" || die "cannot create $OUT_DIR"

  # cargo names the test executable on its "Executable" line; its folder
  # depends on the cargo version, so it is read from there.
  local snippet
  read -r -d '' snippet <<'EOF'
set -e
cargo build --release --locked -p tephrad
log=$(mktemp)
cargo test --release --locked -p tephrad --test rename_races --no-run 2>&1 | tee "$log"
exe=$(sed -n 's/^ *Executable tests\/rename_races\.rs (\(.*\))$/\1/p' "$log" | tail -n 1)
[ -n "$exe" ] && [ -f "$exe" ]
cp "$exe" /out/rename_races
cp /target/release/tephrad /out/tephrad
EOF
  in_rust_image "$snippet" ||
    die "building the race tests failed in the $IMAGE image (the cargo output is above)"

  dsh "mkdir -p $RUN/tests $RUN/tmp" || die "cannot create $RUN in $CONTAINER"
  docker cp "$OUT_DIR/rename_races" "$CONTAINER:$RUN/tests/rename_races" >/dev/null 2>&1 &&
    docker cp "$OUT_DIR/tephrad" "$CONTAINER:$RUN/tests/tephrad" >/dev/null 2>&1 ||
    die "cannot copy the test binaries into $CONTAINER"
  dsh "chmod +x $RUN/tests/rename_races $RUN/tests/tephrad" || die "cannot make the tests executable"
  check_binary_runs "$RUN/tests/tephrad" --help
  check_binary_runs "$RUN/tests/rename_races" --list >/dev/null

  local filter="race_"
  [ $all = 0 ] || filter=""
  docker exec \
    -e "TEPHRAD_BIN=$RUN/tests/tephrad" -e "RACE_REPEAT=$repeat" -e "TMPDIR=$RUN/tmp" \
    "$CONTAINER" "$RUN/tests/rename_races" $filter
  local code=$?
  if [ $code -ne 0 ]; then
    echo "linux-tephrad: the race tests failed (exit $code; repeat $repeat)" >&2
    return 1
  fi
}

# --- main -----------------------------------------------------------------

[ $# -ge 1 ] || {
  usage >&2
  exit $EX_USAGE
}

command=$1
shift
case "$command" in
  -h | --help | help)
    usage
    ;;
  build)
    [ $# -eq 0 ] || usage_error "build takes no arguments"
    cmd_build
    ;;
  start | stop | status)
    [ $# -ge 1 ] || usage_error "$command needs a slug (the container is desktop-<slug>)"
    slug=$1
    shift
    valid_slug "$slug" || usage_error "'$slug' is not a valid slug"
    if [ "$command" = start ]; then
      cmd_start "$slug" "$@"
    else
      [ $# -eq 0 ] || usage_error "$command takes only a slug"
      "cmd_$command" "$slug"
    fi
    ;;
  test)
    cmd_test "$@"
    ;;
  *)
    usage_error "unknown command '$command'"
    ;;
esac
