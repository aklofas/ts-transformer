#!/usr/bin/env bash
# The exported symbol list of libtstrans, held against the build and against
# the previous release tag.
#
# Usage: abi-symbols.sh [--write] [--lib <libtstrans.so>]
#
#   (no flag)  check: the library's dynamic symbol table must equal
#              bindings/c/abi-symbols.txt line for line, and against the
#              previous release tag the set may only grow:
#                - a name present at the tag and gone now FAILS (a removal
#                  needs the 1.0 major);
#                - a name added since the tag FAILS unless TST_ABI_VERSION_MINOR
#                  (bindings/c/core/src/lib.rs) is higher than at the tag.
#   --write    regenerate bindings/c/abi-symbols.txt from the build.
#   --lib      judge this library instead of staging one. Pass a library from
#              scripts/dev/c-package.sh --all-features; target/<profile>
#              holds whichever feature set was built last and is not a source.
#
# Why a diff and not a prefix test: every dynamic symbol is listed, so a
# forgotten `no_mangle` helper or a leaked libsrt/mbedtls name shows up as an
# added line, not only as a name without the tst_ prefix. ELF housekeeping
# (_init, _fini, __bss_start, _edata, _end) and Rust runtime names are dropped.
#
# Previous tag: the newest vMAJOR.MINOR.PATCH tag (version order) that does not
# point at HEAD (scripts/dev/prev-tag.sh, shared with the c-abi CI job). Its
# symbol set is its own bindings/c/abi-symbols.txt when it has one, else the
# declaration set of its bindings/c/include/tstrans.h, which the first release
# with this list exported exactly.
#
# Linux only (GNU nm, ELF): SKIP elsewhere. Without tags: FAIL under CI
# (run `git fetch --tags --depth=1 origin`), SKIP locally.
#
# Exit: 0 ok or skipped, 1 mismatch, 2 tool or usage error.
set -uo pipefail
# One byte order for sort, comm and diff, whatever the host locale.
export LC_ALL=C

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BASELINE="$ROOT/bindings/c/abi-symbols.txt"
LIBRS=bindings/c/core/src/lib.rs
HEADER=bindings/c/include/tstrans.h
BASELINE_REL=bindings/c/abi-symbols.txt

WRITE=0 LIB=""
while [ $# -gt 0 ]; do
    case "$1" in
        --write) WRITE=1; shift ;;
        --lib) [ $# -ge 2 ] || { echo "abi-symbols: --lib needs a path" >&2; exit 2; }; LIB="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "abi-symbols: unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ "$(uname -s)" != "Linux" ]; then
    echo "SKIP: abi-symbols needs GNU nm on ELF (Linux)"
    exit 0
fi
for t in nm git python3 awk sort comm diff grep sed; do
    command -v "$t" >/dev/null 2>&1 || { echo "abi-symbols: $t not installed" >&2; exit 2; }
done

tmp="$(mktemp -d)"
trap 'chmod -R u+w "$tmp" 2>/dev/null; rm -rf "$tmp"' EXIT

if [ -z "$LIB" ]; then
    : "${SRT_FORCE_VENDORED:=1}" "${RIST_FORCE_VENDORED:=1}"
    export SRT_FORCE_VENDORED RIST_FORCE_VENDORED
    if ! (cd "$ROOT" && bash scripts/dev/c-package.sh --all-features --out "$tmp/pkg") >"$tmp/stage.log" 2>&1; then
        cat "$tmp/stage.log" >&2
        echo "abi-symbols: could not stage the all-features package" >&2
        exit 2
    fi
    LIB="$tmp/pkg/lib/libtstrans.so"
fi
[ -f "$LIB" ] || { echo "abi-symbols: no such library: $LIB" >&2; exit 2; }

# The current set.
if ! nm -D --defined-only "$LIB" >"$tmp/nm.out" 2>"$tmp/nm.err"; then
    cat "$tmp/nm.err" >&2; echo "abi-symbols: nm failed on $LIB" >&2; exit 2
fi
awk 'NF == 3 { print $3 }' "$tmp/nm.out" \
    | grep -vE '^(_init|_fini|__bss_start|_edata|_end|rust_.*|__rust_.*)$' \
    | sort -u >"$tmp/current"
if ! grep -q '^tst_' "$tmp/current"; then
    echo "abi-symbols: no tst_ symbols in $LIB: not a tstrans library, or a stripped build" >&2
    exit 2
fi
n_cur="$(wc -l <"$tmp/current" | tr -d ' ')"

if [ "$WRITE" = 1 ]; then
    {
        echo "# Exported symbols of libtstrans (linux-x86_64, all features), one per line, sorted."
        echo "# Regenerate: bash scripts/check/c/abi-symbols.sh --write"
        echo "# Checked against the build and the previous release tag by the same script."
        cat "$tmp/current"
    } >"$BASELINE"
    echo "wrote $BASELINE_REL: $n_cur names"
    exit 0
fi

fail=0

# 1. The build against the committed list.
if [ ! -f "$BASELINE" ]; then
    echo "FAIL: no baseline at $BASELINE_REL (bash scripts/check/c/abi-symbols.sh --write)"
    exit 1
fi
grep -v '^#' "$BASELINE" >"$tmp/baseline"
if diff -u --label "$BASELINE_REL" --label "libtstrans.so" "$tmp/baseline" "$tmp/current" >"$tmp/diff"; then
    echo "ok: $n_cur exported names match $BASELINE_REL"
else
    cat "$tmp/diff"
    echo "FAIL: the exported symbols differ from $BASELINE_REL ('-' = in the file only, '+' = in the build only)"
    fail=1
fi

# 2. The build against the previous tag.
cd "$ROOT" || exit 2
prev="$(bash scripts/dev/prev-tag.sh)"
if [ -z "$prev" ]; then
    if [ -n "${CI:-}" ]; then
        echo "FAIL: no release tag in this clone: fetch tags (git fetch --tags --depth=1 origin)"
        exit 1
    fi
    echo "SKIP: no release tag in this clone; the cross-tag rules were not checked"
    exit "$fail"
fi

minor_of() { # <git object path> -> minor on stdout
    git show "$1" 2>/dev/null \
        | sed -n 's/^pub const TST_ABI_VERSION_MINOR: [A-Za-z0-9_:]* = \([0-9][0-9]*\);.*/\1/p' | head -1
}
min_prev="$(minor_of "$prev:$LIBRS")"
min_cur="$(sed -n 's/^pub const TST_ABI_VERSION_MINOR: [A-Za-z0-9_:]* = \([0-9][0-9]*\);.*/\1/p' "$ROOT/$LIBRS" | head -1)"
[ -n "$min_prev" ] || { echo "abi-symbols: no TST_ABI_VERSION_MINOR in $prev:$LIBRS" >&2; exit 2; }
[ -n "$min_cur" ] || { echo "abi-symbols: no TST_ABI_VERSION_MINOR in $LIBRS" >&2; exit 2; }

if git cat-file -e "$prev:$BASELINE_REL" 2>/dev/null; then
    source_label="$BASELINE_REL"
    git show "$prev:$BASELINE_REL" | grep -v '^#' | sort -u >"$tmp/prev"
else
    source_label="header (bootstrap)"
    git cat-file -e "$prev:$HEADER" 2>/dev/null || { echo "abi-symbols: $prev has neither $BASELINE_REL nor $HEADER" >&2; exit 2; }
    # Declaration set: every tst_/TST_ identifier followed by '(' outside comments.
    git show "$prev:$HEADER" | python3 -I -c '
import re, sys
h = sys.stdin.read()
h = re.sub(r"/\*.*?\*/", "", h, flags=re.S)
h = re.sub(r"//[^\n]*", "", h)
for n in sorted(set(re.findall(r"\b((?:tst|TST)_[A-Za-z0-9_]+)\s*\(", h))):
    print(n)
' >"$tmp/prev"
fi
[ -s "$tmp/prev" ] || { echo "abi-symbols: empty symbol set for $prev ($source_label)" >&2; exit 2; }

comm -23 "$tmp/prev" "$tmp/current" >"$tmp/removed"
comm -13 "$tmp/prev" "$tmp/current" >"$tmp/added"
n_prev="$(wc -l <"$tmp/prev" | tr -d ' ')"
n_added="$(wc -l <"$tmp/added" | tr -d ' ')"
added_list="$(paste -sd, "$tmp/added" | sed 's/,/, /g')"
summary="previous tag $prev, source: $source_label, $n_prev names, +$n_added added${added_list:+ ($added_list)}, minor $min_prev → $min_cur"

if [ -s "$tmp/removed" ]; then
    echo "Removed since $prev:"; sed 's/^/  /' "$tmp/removed"
    echo "FAIL: $summary: exported names were removed; a removal needs the 1.0 major"
    fail=1
elif [ "$n_added" -gt 0 ] && [ "$min_cur" -le "$min_prev" ]; then
    echo "Added since $prev:"; sed 's/^/  /' "$tmp/added"
    echo "FAIL: $summary: names were added without raising TST_ABI_VERSION_MINOR"
    fail=1
else
    echo "$summary: OK"
fi
exit "$fail"
