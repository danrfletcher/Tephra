#!/usr/bin/env bash
# Download a pinned rclone release binary into spikes/.bin/ (gitignored). Only
# needed when rclone is not on PATH. Needs the network.
#
#   spikes/s2/fetch-rclone.sh
#
# The version is pinned, and so is the sha256 of each archive (taken from the
# SHA256SUMS file rclone publishes for that release). The archive is checked
# before it is unpacked; a mismatch stops the script with exit 1 and nothing is
# installed. To move to a newer release, change `version` and the four hashes
# together from https://downloads.rclone.org/<version>/SHA256SUMS.
#
# Test hooks (crates/tephrad/tests/spike_offline_run.rs): TEPHRA_SPIKE_RCLONE_BASE
# replaces https://downloads.rclone.org, TEPHRA_SPIKE_BIN_DIR replaces spikes/.bin.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${TEPHRA_SPIKE_BIN_DIR:-$here/../.bin}"
base="${TEPHRA_SPIKE_RCLONE_BASE:-https://downloads.rclone.org}"
version="v1.76.0"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="osx-arm64"; want="e72dc90b70f4947a7b72b61bde4a4fbba5777b2874fb448951ffdde67ebd0ab7" ;;
  Darwin-x86_64) target="osx-amd64"; want="4f5835733d52b818340f97b1fc0348a70461534d123363a183ca2b660219595a" ;;
  Linux-x86_64) target="linux-amd64"; want="b00f00b3266c3764413afe599d3ec316f6f43b3e0ea8a0bc1d186d2133e46df7" ;;
  Linux-aarch64|Linux-arm64) target="linux-arm64"; want="ee169016675e23a69d77240565133ff7dbc9b33d16fa8f7053d4af39f1eaeb78" ;;
  *) echo "not run: no rclone release known for $(uname -s)-$(uname -m)"; exit 0 ;;
esac

tmp="$(mktemp -d "${TMPDIR:-/tmp}/tephra-spike.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
if ! curl -sSfL -m 180 -o "$tmp/rclone.zip" "$base/$version/rclone-$version-$target.zip"; then
  echo "not run: could not download the rclone $version release for $target"
  exit 0
fi
got="$(shasum -a 256 "$tmp/rclone.zip" | cut -d' ' -f1)"
if [ "$got" != "$want" ]; then
  echo "checksum mismatch: rclone-$version-$target.zip has sha256 $got, expected $want; nothing installed" >&2
  exit 1
fi
echo "sha256 verified for rclone-$version-$target.zip: $got"
mkdir -p "$bin"
unzip -q -o "$tmp/rclone.zip" -d "$tmp/x"
cp "$tmp"/x/rclone-*/rclone "$bin/rclone"
chmod +x "$bin/rclone"
"$bin/rclone" version | head -1
