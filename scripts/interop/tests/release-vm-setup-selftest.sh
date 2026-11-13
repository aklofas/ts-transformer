#!/usr/bin/env bash
# Self-test for the architecture handling in the interop harness: the setup
# script installs a browser on both x86_64 and aarch64, and the soak config
# and provenance record stamp `uname -m`. Also that the setup script fetches
# the release tags and runs the checkout's version check before it builds.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DIR=$HERE/..
fail=0

check() { # <label> <file> <fixed string>
  if ! grep -qF -- "$3" "$DIR/$2"; then
    echo "FAIL: $1 ($2 lacks: $3)"
    fail=1
  fi
}

check "setup: aarch64 branch"        release-vm-setup.sh 'aarch64)'
check "setup: aarch64 installs Chromium" release-vm-setup.sh 'snap install chromium'
check "setup: x86_64 keeps the Chrome .deb" release-vm-setup.sh 'google-chrome-stable_current_amd64.deb'
check "setup: stage is named browser" release-vm-setup.sh 'stage browser'
check "soak-config stamps the arch"  soak.sh '--arg arch "$(uname -m)"'
check "soak-config carries the key"  soak.sh 'arch: $arch'
check "provenance stamps host arch"  lib.sh '--arg arch "$(uname -m)"'
check "provenance carries host.arch" lib.sh 'arch: $arch'
check "setup: fetches the release tags" release-vm-setup.sh 'git fetch -q --tags origin'
check "setup: runs the checkout's version check" release-vm-setup.sh '"$REPO_DIR/scripts/interop/release-version-check.sh" "$REPO_DIR"'
check "setup: the stage is named version" release-vm-setup.sh 'stage version'
# The version stage comes after the checkout and before the build.
order=$(grep -n -e '^stage checkout' -e '^stage version' -e '^stage build' "$DIR/release-vm-setup.sh" | cut -d: -f2 | tr '\n' ' ')
[[ "$order" == 'stage checkout "$rc" stage version "$rc" stage build "$rc" ' ]] ||
  { echo "FAIL: setup: the stages are not checkout, version, build (got: $order)"; fail=1; }

[[ $fail -eq 0 ]] && echo "release-vm-setup-selftest: ok"
exit "$fail"
