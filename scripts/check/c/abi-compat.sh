#!/usr/bin/env bash
# C ABI compatibility check: a libtstrans may only GROW, and only with a minor
# bump (the additive-only rule documented at TST_ABI_VERSION_MINOR in
# bindings/c/core/src/lib.rs).
#
# Usage: abi-compat.sh <old-lib-or-abi.xml> <new-lib> <old-minor> <new-minor>
#                      [<old-major> <new-major>]
#   <old> is a shared library or a libabigail corpus written by `abidw`;
#   <new> is a shared library. The majors default to 0 (pre-1.0).
#
# What it judges, over the exported tst_* functions and TST_*/tst_* variables
# (bindings/c/abi.suppr drops everything else, e.g. the Rust runtime):
#   - an INCOMPATIBLE change fails unless new-major > old-major. That is a
#     removed or changed function/variable, and any layout change of a type
#     they reach: struct size or member change, enumerator renumbered or
#     removed. libabigail only sets its "incompatible" exit bit for removals,
#     so the layout changes are recognised from the report text.
#   - any other change, or an added exported symbol, needs new-minor > old-minor.
#
# Exit: 0 compatible, 1 violates the rule (report printed), 2 tool/usage error
# or a missing input. Without abidiff: `SKIP` (exit 0), except under CI (exit 1),
# so a runner that lost the tool cannot pass silently.
set -uo pipefail

if ! command -v abidiff >/dev/null 2>&1; then
    if [ -n "${CI:-}" ]; then echo "FAIL: abidiff not installed (required under CI)" >&2; exit 1; fi
    echo "SKIP: abidiff not installed"
    exit 0
fi
if [ $# -ne 4 ] && [ $# -ne 6 ]; then
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
    exit 2
fi
OLD="$1" NEW="$2" OLD_MINOR="$3" NEW_MINOR="$4" OLD_MAJOR="${5:-0}" NEW_MAJOR="${6:-0}"
for n in "$OLD_MINOR" "$NEW_MINOR" "$OLD_MAJOR" "$NEW_MAJOR"; do
    case "$n" in ''|*[!0-9]*) echo "abi-compat: version fields must be integers: '$n'" >&2; exit 2 ;; esac
done

for f in "$OLD" "$NEW"; do
    [ -f "$f" ] || { echo "abi-compat: no such file: $f" >&2; exit 2; }
done

SUPPR="$(cd "$(dirname "$0")/../../.." && pwd)/bindings/c/abi.suppr"
[ -f "$SUPPR" ] || { echo "abi-compat: missing $SUPPR" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Exported C symbols of a library or an abidw corpus, one per line.
symbols() {
    case "$1" in
        *.abi|*.xml) grep -o "<elf-symbol name='[^']*'" "$1" | sed "s/.*name='//; s/'$//" ;;
        *) nm -D --defined-only "$1" | awk '{print $3}' ;;
    esac | grep -E '^(tst_|TST_)' | sort -u
}

abidiff --no-added-syms --suppressions "$SUPPR" "$OLD" "$NEW" >"$tmp/report" 2>&1
rc=$?
if [ $((rc & 3)) -ne 0 ]; then
    echo "abi-compat: abidiff failed (exit $rc)" >&2
    cat "$tmp/report" >&2
    exit 2
fi

symbols "$OLD" >"$tmp/old.sym" || true
symbols "$NEW" >"$tmp/new.sym" || true
comm -13 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/added"
comm -23 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/gone"

incompatible=0
[ $((rc & 8)) -ne 0 ] && incompatible=1
[ -s "$tmp/gone" ] && incompatible=1
if [ $((rc & 4)) -ne 0 ] && grep -Eq \
    'type size changed|data member|enumerator (change|deletion)|offset|return type|parameter [0-9]+ of type .* changed' \
    "$tmp/report"; then
    incompatible=1
fi
changed=0
[ $((rc & 4)) -ne 0 ] && changed=1
[ -s "$tmp/added" ] && changed=1

show() {
    cat "$tmp/report"
    if [ -s "$tmp/gone" ]; then echo "Removed exported symbols:"; sed 's/^/  /' "$tmp/gone"; fi
    if [ -s "$tmp/added" ]; then echo "Added exported symbols:"; sed 's/^/  /' "$tmp/added"; fi
}

if [ "$incompatible" -eq 1 ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ]; then
    show
    echo "FAIL: incompatible C ABI change without a major bump (major $OLD_MAJOR -> $NEW_MAJOR)"
    exit 1
fi
if [ "$changed" -eq 1 ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ] && [ "$NEW_MINOR" -le "$OLD_MINOR" ]; then
    show
    echo "FAIL: the C ABI grew without a minor bump (minor $OLD_MINOR -> $NEW_MINOR)"
    exit 1
fi
echo "ok: C ABI compatible (minor $OLD_MINOR -> $NEW_MINOR, $(wc -l <"$tmp/added" | tr -d ' ') added symbols)"
