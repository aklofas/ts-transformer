#!/usr/bin/env bash
# advisory-db-pin: tests/coverage/advisory-db-pin.toml holds exactly one
# `rev = "..."` line whose value is empty or a 40-hex advisory-db commit, the
# two forms scripts/dev/cargo-deny-all.sh accepts. A set pin passes: during a
# release freeze it is set on main.
# Overridable: ADVISORY_DB_PIN (a pin file to check instead of this tree's).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
PIN="${ADVISORY_DB_PIN:-$ROOT/tests/coverage/advisory-db-pin.toml}"
bad() { echo "FAIL: advisory-db-pin: $*"; exit 1; }

[ -f "$PIN" ] || bad "$PIN is missing"
n="$(grep -cE '^rev[[:space:]]*=' "$PIN" || true)"
[ "$n" = 1 ] || bad "$PIN has $n rev lines, want one"
line="$(grep -E '^rev[[:space:]]*=' "$PIN")"
printf '%s\n' "$line" | grep -qE '^rev[[:space:]]*=[[:space:]]*"([0-9a-f]{40})?"[[:space:]]*$' \
  || bad "rev $(printf '%s' "$line" | sed -E "s/^rev[[:space:]]*=[[:space:]]*\"?([^\"]*)\"?.*/'\1'/") is neither empty nor a 40-hex commit"
echo "advisory-db-pin: OK"
