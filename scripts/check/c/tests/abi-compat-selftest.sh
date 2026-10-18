#!/usr/bin/env bash
# Self-test for abi-compat.sh: compiles small C libraries (abi-fixtures/) and
# checks the verdict for each kind of change. SKIPs without abidiff, except
# under CI where abi-compat.sh itself fails.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
RAIL="$HERE/../abi-compat.sh"

if ! command -v abidiff >/dev/null 2>&1; then
    exec bash "$RAIL" x x 0 0   # SKIP (exit 0), or FAIL under CI
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
for v in v1 v2_removed v2_struct v2_enum v2_added; do
    cc -g -shared -fPIC -o "$tmp/$v.so" "$HERE/abi-fixtures/$v.c" || { echo "FAIL: cannot compile $v"; exit 1; }
done

fail=0
expect() { # <desc> <want_rc> <args...>
    local desc="$1" want="$2"; shift 2
    bash "$RAIL" "$@" >"$tmp/out" 2>&1; local got=$?
    if [ "$got" = "$want" ]; then echo "ok: $desc"
    else echo "FAIL: $desc (got rc=$got want=$want)"; sed 's/^/    /' "$tmp/out"; fail=1; fi
}
expect "unchanged library passes"                          0 "$tmp/v1.so" "$tmp/v1.so" 3 3
expect "a removed function fails"                          1 "$tmp/v1.so" "$tmp/v2_removed.so" 3 4
expect "a struct that grew fails, even with a minor bump (the bit, not the text)"  1 "$tmp/v1.so" "$tmp/v2_struct.so" 3 4
expect "a renumbered enum constant fails"                  1 "$tmp/v1.so" "$tmp/v2_enum.so" 3 4
expect "an added function without a minor bump fails"      1 "$tmp/v1.so" "$tmp/v2_added.so" 3 3
expect "an added function with a minor bump passes"        0 "$tmp/v1.so" "$tmp/v2_added.so" 3 4
expect "an incompatible change passes with a major bump"   0 "$tmp/v1.so" "$tmp/v2_removed.so" 0 0 0 1
expect "a missing old library is a tool error"             2 "$tmp/absent.so" "$tmp/v1.so" 3 3
if command -v abidw >/dev/null 2>&1; then
    abidw --out-file "$tmp/v1.abi" "$tmp/v1.so" >/dev/null 2>&1
    expect "an abidw corpus is accepted as the old side"   0 "$tmp/v1.abi" "$tmp/v1.so" 3 3
    expect "an abidw corpus still catches a removal"       1 "$tmp/v1.abi" "$tmp/v2_removed.so" 3 4
    expect "an abidw corpus sees an added symbol"          1 "$tmp/v1.abi" "$tmp/v2_added.so" 3 3
fi
[ "$fail" = 0 ] && echo "abi-compat self-test: ALL OK"
exit "$fail"
