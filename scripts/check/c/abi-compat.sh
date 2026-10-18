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
#   - ANY change abidiff reports (exit bit 4 or 8: a removed or changed function,
#     a struct that grew or moved, a renumbered enumerator) FAILS. Pre-1.0 a C
#     library only grows, so this holds whatever the minor says. An intended
#     change needs a commented suppression in bindings/c/abi.suppr. At
#     new-major > old-major the check passes (the 1.0 break).
#   - an added exported symbol (a separate symbol diff) needs new-minor > old-minor.
#
# Argument-driven: with no arguments it prints this usage and exits 2, with or
# without abidiff. tests/abi-compat-selftest.sh is the entry point for sweeps.
#
# Exit: 0 compatible, 1 violates the rule (report printed), 2 tool/usage error
# or a missing input. Without abidiff: `SKIP` (exit 0), except under CI (exit 1),
# so a runner that lost the tool cannot pass silently.
set -uo pipefail

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
echo "ok: C ABI compatible (minor $OLD_MINOR -> $NEW_MINOR, $(wc -l <"$tmp/added" | tr -d ' ') added symbols)"
