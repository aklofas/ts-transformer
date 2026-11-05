#!/usr/bin/env bash
# Self-test for the architecture handling in the interop harness: the setup
# script installs a browser on both x86_64 and aarch64, and the soak config
# and provenance record stamp `uname -m`.
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

[[ $fail -eq 0 ]] && echo "release-vm-setup-selftest: ok"
exit "$fail"
