#!/usr/bin/env bash
# Installs meson from the hashed requirements file into a venv and puts meson
# alone on PATH. ninja, cmake and pkg-config must already be there (the runner
# image ships them): linking only meson keeps the venv's own ninja and
# pkg-config from shadowing the image's.
#
# Environment: RUNNER_TEMP (default /tmp) holds the venv and the bin directory;
# GITHUB_PATH, when set, receives the bin directory; CI_INSTALL_WAITS ("20 60")
# is the pause in seconds before the second and third pip attempt.
set -euo pipefail

tmp="${RUNNER_TEMP:-/tmp}"
venv="$tmp/build-tools"
bindir="$tmp/build-tools-bin"
waits="${CI_INSTALL_WAITS:-20 60}"

for tool in ninja cmake pkg-config; do
  command -v "$tool" >/dev/null 2>&1 || { echo "install-build-tools.sh: '$tool' is not on PATH" >&2; exit 1; }
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 -m venv "$venv"

attempt=1
while :; do
  if "$venv/bin/pip" install --require-hashes -r "$here/requirements-build-tools.txt"; then
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
