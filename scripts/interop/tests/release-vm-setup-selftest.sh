#!/usr/bin/env bash
# Self-test for the architecture handling in the interop harness: the setup
# script installs a browser on both x86_64 and aarch64, and the soak config
# and provenance record stamp `uname -m`. Also that the setup script fetches
# the release tags and runs the checkout's version check before it builds;
# the version stage's own lines are executed against throwaway checkouts.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DIR=$HERE/..
fail=0
unset TSTRANS_NOT_A_RELEASE

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

# The version stage, executed: the lines of release-vm-setup.sh between its
# `stage checkout` and `stage version` lines, run with REPO_DIR pointing at a
# throwaway checkout. The rest of the script needs a host it may reconfigure.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
version_stage=$(awk '/^stage checkout/ { on = 1; next } on; /^stage version/ { exit }' "$DIR/release-vm-setup.sh")
[[ "$version_stage" == *'stage version "$rc"' ]] ||
  { echo "FAIL: setup: the version stage's lines were not found"; fail=1; }
n=0
# checkout <workspace version> <with|without>: a repository whose first
# commit is tagged v0.10.0 and whose HEAD, one commit later, declares the
# version, with or without the version check script.
checkout() {
  local g
  n=$((n + 1))
  REPO_DIR=$tmp/r$n
  g=(git -C "$REPO_DIR" -c user.name=t -c user.email=t@example.com -c commit.gpgsign=false)
  mkdir -p "$REPO_DIR/scripts/interop"
  git -C "$REPO_DIR" init -q
  "${g[@]}" commit -q --allow-empty -m "the release"
  "${g[@]}" tag v0.10.0
  printf '[workspace.package]\nversion = "%s"\n' "$1" > "$REPO_DIR/Cargo.toml"
  [[ $2 == with ]] && cp "$DIR/release-version-check.sh" "$REPO_DIR/scripts/interop/"
  "${g[@]}" add -A
  "${g[@]}" commit -q -m "version $1"
}
run_stage() { # <label> <want rc> <output pattern>
  local out
  out=$(
    # shellcheck disable=SC2034  # SHA and rc are read by the stage's lines
    SHA=$(git -C "$REPO_DIR" rev-parse HEAD) rc=0
    stage() { echo "$1 rc=$2"; }
    eval "$version_stage" 2>&1
  )
  if ! grep -qx "version rc=$2" <<<"$out" || ! grep -q -- "$3" <<<"$out"; then
    echo "FAIL: $1 (wanted 'version rc=$2' and '$3')"; sed 's/^/    /' <<<"$out"; fail=1
  fi
}
checkout 0.11.0 with
run_stage "setup: a swept checkout passes the version stage" 0 'ok (workspace 0.11.0, newest release tag v0.10.0)'
checkout 0.10.0 with
run_stage "setup: an unswept checkout stops at the version stage" 1 'has not had its version sweep'
checkout 0.11.0 without
run_stage "setup: a commit without the check is refused for a release run" 5 'has no scripts/interop/release-version-check.sh'
export TSTRANS_NOT_A_RELEASE=yes
run_stage "setup: any other value of TSTRANS_NOT_A_RELEASE does not skip" 5 'has no scripts/interop/release-version-check.sh'
export TSTRANS_NOT_A_RELEASE=1
run_stage "setup: TSTRANS_NOT_A_RELEASE=1 admits a commit without the check, and says so" 0 'skipped (TSTRANS_NOT_A_RELEASE=1)'
checkout 0.10.0 with
run_stage "setup: TSTRANS_NOT_A_RELEASE=1 skips the check on an unswept checkout" 0 'skipped (TSTRANS_NOT_A_RELEASE=1)'
unset TSTRANS_NOT_A_RELEASE

[[ $fail -eq 0 ]] && echo "release-vm-setup-selftest: ok"
exit "$fail"
