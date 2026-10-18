#!/usr/bin/env bash
# C ABI compatibility check: a libtstrans may only GROW, and only with a minor
# bump (the additive-only rule documented at TST_ABI_VERSION_MINOR in
# bindings/c/core/src/lib.rs).
#
# Usage: abi-compat.sh [--header <tstrans.h>] <old-lib-or-abi.xml> <new-lib>
#                      <old-minor> <new-minor> [<old-major> <new-major>]
#   <old> is a shared library or a libabigail corpus written by `abidw`;
#   <new> is a shared library. The majors default to 0 (pre-1.0).
#
# What it judges, over the exported tst_* functions and TST_*/tst_* variables
# (bindings/c/abi.suppr drops everything else, e.g. the Rust runtime):
#   - ANY change abidiff reports (exit bit 4 or 8: a removed or changed function,
#     a struct that grew or moved, a renumbered enumerator) FAILS. Pre-1.0 a C
#     library only grows, so this holds whatever the minor says. An intended
#     change needs a commented suppression in bindings/c/abi.suppr. At
#     new-major > old-major the check passes (the 1.0 break).
#   - an added exported symbol (a separate symbol diff) needs new-minor > old-minor.
#
# Opaque handles: every `typedef struct X X;` in the header (declared without a
# body) is a type whose layout C never sees; its changes are suppressed (also
# under its Rust spelling: tst_sender_t is TstSender in the debug info). The
# list is derived from --header on every run (default: bindings/c/include/
# tstrans.h of this tree, which holds only the default feature set; pass the
# header of the staged all-features package, as the c-abi job does).
#
# Argument-driven: with no arguments it prints this usage and exits 2, with or
# without abidiff. tests/abi-compat-selftest.sh is the entry point for sweeps.
#
# Exit: 0 compatible, 1 violates the rule (report printed), 2 tool/usage error
# or a missing input. Without abidiff: `SKIP` (exit 0), except under CI (exit 1),
# so a runner that lost the tool cannot pass silently.
set -uo pipefail

HEADER=""
args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --header) [ $# -ge 2 ] || { echo "abi-compat: --header needs a path" >&2; exit 2; }; HEADER="$2"; shift 2 ;;
        *) args+=("$1"); shift ;;
    esac
done
set -- ${args[@]+"${args[@]}"}
if [ $# -ne 4 ] && [ $# -ne 6 ]; then
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
    exit 2
fi
if ! command -v abidiff >/dev/null 2>&1; then
    if [ -n "${CI:-}" ]; then echo "FAIL: abidiff not installed (required under CI)" >&2; exit 1; fi
    echo "SKIP: abidiff not installed"
    exit 0
fi
OLD="$1" NEW="$2" OLD_MINOR="$3" NEW_MINOR="$4" OLD_MAJOR="${5:-0}" NEW_MAJOR="${6:-0}"
for n in "$OLD_MINOR" "$NEW_MINOR" "$OLD_MAJOR" "$NEW_MAJOR"; do
    case "$n" in ''|*[!0-9]*) echo "abi-compat: version fields must be integers: '$n'" >&2; exit 2 ;; esac
done

command -v nm >/dev/null 2>&1 || { echo "abi-compat: nm not installed (binutils)" >&2; exit 2; }
for f in "$OLD" "$NEW"; do
    [ -f "$f" ] || { echo "abi-compat: no such file: $f" >&2; exit 2; }
done

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SUPPR="$ROOT/bindings/c/abi.suppr"
[ -f "$SUPPR" ] || { echo "abi-compat: missing $SUPPR" >&2; exit 2; }
[ -n "$HEADER" ] || HEADER="$ROOT/bindings/c/include/tstrans.h"
[ -r "$HEADER" ] || { echo "abi-compat: cannot read header: $HEADER" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Opaque handle types, derived from the header, appended to the suppressions.
# DWARF names the Rust struct (TstSender) where the header says tst_sender_t,
# so each tst_<snake>_t name also yields its Tst<Camel> spelling.
sed -n 's/^typedef struct \([A-Za-z0-9_][A-Za-z0-9_]*\) \1;.*/\1/p' "$HEADER" \
    | awk '{ print; if ($0 ~ /^tst_.*_t$/) { n = split(substr($0, 5, length($0) - 6), w, "_"); c = "Tst"; for (i = 1; i <= n; i++) c = c toupper(substr(w[i], 1, 1)) substr(w[i], 2); print c } }' \
    | sort -u >"$tmp/opaque"
n_opaque="$(wc -l <"$tmp/opaque" | tr -d ' ')"
cp "$SUPPR" "$tmp/suppr"
if [ "$n_opaque" -gt 0 ]; then
    { printf '\n[suppress_type]\n  type_kind = struct\n  name_regexp = (^|::)(%s)$\n' "$(paste -sd'|' "$tmp/opaque")"; } >>"$tmp/suppr"
fi
echo "abi-compat: $n_opaque opaque handle types suppressed (derived from $HEADER)"

# Exported C symbols of a library or an abidw corpus, one per line; a failing
# listing is an error (exit 2), never an empty set.
symbols() { # <file> <out>
    local raw="$tmp/raw.$$"
    case "$1" in
        *.abi|*.xml) grep -o "<elf-symbol name='[^']*'" "$1" | sed "s/.*name='//; s/'$//" >"$raw" || true ;;
        *) nm -D --defined-only "$1" 2>"$tmp/nm.err" | awk '{print $3}' >"$raw"
           if [ "${PIPESTATUS[0]}" -ne 0 ]; then
               echo "abi-compat: nm failed on $1" >&2; cat "$tmp/nm.err" >&2; exit 2
           fi ;;
    esac
    grep -E '^(tst_|TST_)' "$raw" | sort -u >"$2" || true
    if [ ! -s "$2" ]; then
        echo "abi-compat: no tst_ symbols in $1: not a tstrans library, or a stripped/stale build" >&2
        exit 2
    fi
}

abidiff --no-added-syms --suppressions "$tmp/suppr" "$OLD" "$NEW" >"$tmp/report" 2>&1
rc=$?
if [ $((rc & 3)) -ne 0 ]; then
    echo "abi-compat: abidiff failed (exit $rc)" >&2
    cat "$tmp/report" >&2
    exit 2
fi

symbols "$OLD" "$tmp/old.sym"
symbols "$NEW" "$tmp/new.sym"
comm -13 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/added"
comm -23 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/gone"

echo "abi-compat: $(wc -l <"$tmp/old.sym" | tr -d ' ') old / $(wc -l <"$tmp/new.sym" | tr -d ' ') new tst_ symbols, $(wc -l <"$tmp/added" | tr -d ' ') added, $(wc -l <"$tmp/gone" | tr -d ' ') removed"
changed=0
[ $((rc & 12)) -ne 0 ] && changed=1
[ -s "$tmp/gone" ] && changed=1

show() {
    cat "$tmp/report"
    if [ -s "$tmp/gone" ]; then echo "Removed exported symbols:"; sed 's/^/  /' "$tmp/gone"; fi
    if [ -s "$tmp/added" ]; then echo "Added exported symbols:"; sed 's/^/  /' "$tmp/added"; fi
}

if [ "$changed" -eq 1 ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ]; then
    show
    echo "FAIL: abidiff reports a change; a pre-1.0 C library only grows - an intended change needs a commented suppression in bindings/c/abi.suppr"
    exit 1
fi
if [ -s "$tmp/added" ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ] && [ "$NEW_MINOR" -le "$OLD_MINOR" ]; then
    show
    echo "FAIL: the C ABI grew without a minor bump (minor $OLD_MINOR -> $NEW_MINOR)"
    exit 1
fi
echo "ok: C ABI compatible (minor $OLD_MINOR -> $NEW_MINOR)"
