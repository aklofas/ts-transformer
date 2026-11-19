#!/usr/bin/env bash
# Self-test of retry3 in scripts/ci/manylinux-before-build.sh, sourced with
# MANYLINUX_BEFORE_BUILD_SOURCE_ONLY=1 and driven by a stub command that
# counts its calls and succeeds from the call number in $FLAKY_OK_AT on.
# MANYLINUX_SCRIPT overrides the script under test.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="${MANYLINUX_SCRIPT:-$ROOT/scripts/ci/manylinux-before-build.sh}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir "$TMP/bin"
cat > "$TMP/bin/flaky" <<'STUB'
#!/bin/sh
n=$(( $(cat "$TMP/count") + 1 ))
echo "$n" > "$TMP/count"
[ "$n" -ge "$FLAKY_OK_AT" ]
STUB
chmod +x "$TMP/bin/flaky"
export TMP PATH="$TMP/bin:$PATH" MANYLINUX_BEFORE_BUILD_SOURCE_ONLY=1 RETRY3_DELAYS_1=0 RETRY3_DELAYS_2=0
# shellcheck disable=SC1090
source "$SCRIPT"
set +e

cases=0
check() { # name expected-rc expected-calls ok-at
  echo 0 > "$TMP/count"
  FLAKY_OK_AT="$4" retry3 flaky 2>"$TMP/err"
  rc=$?
  calls="$(cat "$TMP/count")"
  if [ "$rc" != "$2" ] || [ "$calls" != "$3" ]; then
    echo "FAIL: $1: rc=$rc calls=$calls, expected rc=$2 calls=$3" >&2
    exit 1
  fi
  cases=$((cases + 1))
}

check "first try succeeds" 0 1 1
check "second try succeeds" 0 2 2
check "gives up after three tries" 1 3 99
grep -q 'retry3: flaky failed 3 times' "$TMP/err" || { echo "FAIL: no give-up line" >&2; exit 1; }
echo "PASS: $cases cases"
