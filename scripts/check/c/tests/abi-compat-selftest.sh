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
for v in v1 v2_removed v2_struct v2_enum v2_added v1_opaque v2_opaque; do
    cc -g -shared -fPIC -o "$tmp/$v.so" "$HERE/abi-fixtures/$v.c" || { echo "FAIL: cannot compile $v"; exit 1; }
done

printf 'int helper_not_tst(void) { return 0; }\n' >"$tmp/plain.c"
cc -g -shared -fPIC -o "$tmp/plain.so" "$tmp/plain.c" || { echo "FAIL: cannot compile plain"; exit 1; }
fail=0
# Rows that name no --old-header get the tree's header, so the rail's CI rule
# (an old header is required) is not what they exercise.
TREE_H="$HERE/../../../../bindings/c/include/tstrans.h"
expect() { # <desc> <want_rc> <args...>
    local desc="$1" want="$2"; shift 2
    case " $* " in *" --old-header "*) ;; *) set -- --old-header "$TREE_H" "$@" ;; esac
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
expect "an old library with no tst_ symbols is a tool error" 2 "$tmp/plain.so" "$tmp/v1.so" 3 4
# Opaque handles are derived from the header: a layout change behind a bodiless
# typedef is suppressed, the same change with the body in the header fails.
expect "a changed opaque handle (bodiless in the header) passes" 0 --old-header "$HERE/abi-fixtures/opaque.h" --header "$HERE/abi-fixtures/opaque.h" "$tmp/v1_opaque.so" "$tmp/v2_opaque.so" 3 3
expect "a changed struct with its body in the header fails"      1 --old-header "$HERE/abi-fixtures/open.h" --header "$HERE/abi-fixtures/open.h" "$tmp/v1_opaque.so" "$tmp/v2_opaque.so" 3 3
# A struct that lost its body is a break even though the new header calls it
# opaque; bodiless in both stays suppressed; bodied in both is judged.
expect "a struct that lost its body fails"                       1 --old-header "$HERE/abi-fixtures/open.h" --header "$HERE/abi-fixtures/opaque.h" "$tmp/v1_opaque.so" "$tmp/v2_opaque.so" 3 3
expect "a handle bodiless in both headers passes"                0 --old-header "$HERE/abi-fixtures/opaque.h" --header "$HERE/abi-fixtures/opaque.h" "$tmp/v1_opaque.so" "$tmp/v2_opaque.so" 3 3
expect "a struct bodied in both headers with a new layout fails" 1 --old-header "$HERE/abi-fixtures/open.h" --header "$HERE/abi-fixtures/open.h" "$tmp/v1_opaque.so" "$tmp/v2_opaque.so" 3 3
expect "an unreadable old header is a tool error"                2 --old-header "$tmp/absent.h" "$tmp/v1.so" "$tmp/v1.so" 3 3
CI=1 bash "$RAIL" --header "$HERE/abi-fixtures/opaque.h" "$tmp/v1.so" "$tmp/v1.so" 3 3 >"$tmp/out" 2>&1; got=$?
if [ "$got" = 2 ]; then echo "ok: no --old-header under CI is a tool error"; else echo "FAIL: no --old-header under CI (got rc=$got want=2)"; fail=1; fi
env -u CI bash "$RAIL" --header "$HERE/abi-fixtures/opaque.h" "$tmp/v1.so" "$tmp/v1.so" 3 3 >"$tmp/out" 2>&1; got=$?
if [ "$got" = 0 ] && grep -q WARNING "$tmp/out"; then echo "ok: no --old-header locally warns and passes"; else echo "FAIL: no --old-header locally (got rc=$got)"; fail=1; fi
# No debug info on either side compares symbols only: refused, on both sides.
cc -shared -fPIC -o "$tmp/v1_nodbg.so" "$HERE/abi-fixtures/v1.c" || { echo "FAIL: cannot compile v1_nodbg"; exit 1; }
cp "$tmp/v1.so" "$tmp/v1_stripped.so" && strip --strip-debug "$tmp/v1_stripped.so"
expect "a stripped old library is a tool error"                  2 "$tmp/v1_stripped.so" "$tmp/v2_struct.so" 3 4
expect "a stripped new library is a tool error"                  2 "$tmp/v1.so" "$tmp/v1_stripped.so" 3 4
expect "a library built without -g is a tool error"              2 "$tmp/v1_nodbg.so" "$tmp/v2_struct.so" 3 4
expect "an unreadable header is a tool error"                    2 --header "$tmp/absent.h" "$tmp/v1.so" "$tmp/v1.so" 3 3
# nm hidden from PATH (a PATH of symlinks to everything but nm). This tool
# list must grow when abi-compat.sh starts using another tool, or this row
# fails for the wrong reason.
mkdir "$tmp/nonm"
for t in bash env dirname mktemp rm cat cp paste sed grep awk sort comm wc tr readelf abidiff abidw cc; do
    p="$(command -v "$t" 2>/dev/null)" && ln -s "$p" "$tmp/nonm/$t"
done
expect "nm missing is a tool error" 2 env PATH="$tmp/nonm" bash "$RAIL" "$tmp/v1.so" "$tmp/v2_added.so" 3 3
if command -v abidw >/dev/null 2>&1; then
    abidw --out-file "$tmp/v1.abi" "$tmp/v1.so" >/dev/null 2>&1
    expect "an abidw corpus is accepted as the old side"   0 "$tmp/v1.abi" "$tmp/v1.so" 3 3
    expect "an abidw corpus still catches a removal"       1 "$tmp/v1.abi" "$tmp/v2_removed.so" 3 4
    expect "an abidw corpus sees an added symbol"          1 "$tmp/v1.abi" "$tmp/v2_added.so" 3 3
    abidw --out-file "$tmp/v1_stripped.abi" "$tmp/v1_stripped.so" >/dev/null 2>&1
    expect "a corpus of a stripped library is a tool error" 2 "$tmp/v1_stripped.abi" "$tmp/v1.so" 3 3
fi
[ "$fail" = 0 ] && echo "abi-compat self-test: ALL OK"
exit "$fail"
