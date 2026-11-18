#!/usr/bin/env bash
# Self-test for extern-c-ffi-catch-coverage.sh: the rail must flag exactly the
# violation functions of the fixture and pass the clean one.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
RAIL="$HERE/../extern-c-ffi-catch-coverage.sh"
FIXTURE="$HERE/ffi-catch-fixtures/violations.rs.txt"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

bash "$RAIL" "$FIXTURE" >"$tmp/out" 2>&1
rc=$?
fail=0

if [ "$rc" != 1 ]; then
    echo "FAIL: the rail exited $rc on the fixture (want 1)"
    fail=1
fi
for want in tst_fix_slice_before tst_fix_format_before tst_fix_call_after tst_fix_head_call; do
    if grep -q "^OUTSIDE: $want " "$tmp/out"; then echo "ok: $want is flagged"
    else echo "FAIL: $want is not flagged"; fail=1; fi
done
if grep -q "tst_fix_clean" "$tmp/out"; then
    echo "FAIL: the clean function is flagged"
    fail=1
else
    echo "ok: tst_fix_clean passes"
fi
got=$(grep -c '^OUTSIDE: ' "$tmp/out")
if [ "$got" != 4 ]; then
    echo "FAIL: $got offenders reported (want exactly 4, one per violation function)"
    fail=1
fi
[ "$fail" = 0 ] || sed 's/^/    /' "$tmp/out"
exit "$fail"
