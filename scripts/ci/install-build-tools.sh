#!/usr/bin/env bash
# Installs meson from the hashed requirements file into a venv and puts meson
# alone on PATH. ninja, cmake and pkg-config must already be there (the runner
# image ships them): linking only meson keeps the venv's own ninja and
# pkg-config from shadowing the image's.
#
# Environment: RUNNER_TEMP (default /tmp) holds the venv and the bin directory;
# GITHUB_PATH, when set, receives the bin directory. Three pip attempts, 20 s
# and 60 s apart, each bounded to 120 s (a few MB; worst case about 7 minutes).
set -euo pipefail

tmp="${RUNNER_TEMP:-/tmp}"
venv="$tmp/build-tools"
bindir="$tmp/build-tools-bin"
waits="20 60"
# A wall-clock bound for one network call, when GNU timeout is available (not
# on macOS; Windows' own timeout.exe is a different tool).
bound=()
if timeout --version 2>/dev/null | grep -q GNU; then bound=(timeout -k 10 120); fi

for tool in ninja cmake pkg-config; do
  command -v "$tool" >/dev/null 2>&1 || { echo "install-build-tools.sh: '$tool' is not on PATH" >&2; exit 1; }
done

here="$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 -m venv "$venv"

# pip asks `rustc --version` for its User-Agent and kills it after half a
# second. Run inside the checkout before the pinned toolchain exists, that
# call makes rustup start installing the toolchain and cuts it off part-way;
# the job's own install then fails on what was left ("detected conflict:
# 'bin/cargo-clippy'"). With this set, rustup answers "not installed" at once.
export RUSTUP_AUTO_INSTALL=0

attempt=1
while :; do
  if ${bound[@]+"${bound[@]}"} "$venv/bin/pip" install --require-hashes -r "$here/requirements-build-tools.txt"; then
    break
  fi
  [ "$attempt" -lt 3 ] || { echo "install-build-tools.sh: pip failed 3 times" >&2; exit 1; }
  w="$(echo "$waits" | awk -v i="$attempt" '{print $i}')"
  echo "::notice::pip install attempt $attempt failed; retrying"
  [ -z "$w" ] || sleep "$w"
  attempt=$((attempt + 1))
done

mkdir -p "$bindir"
ln -sf "$venv/bin/meson" "$bindir/meson"
[ -z "${GITHUB_PATH:-}" ] || echo "$bindir" >> "$GITHUB_PATH"
"$bindir/meson" --version
