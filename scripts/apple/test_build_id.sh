#!/usr/bin/env bash
# Self-test for build_id in _lib.sh: a clean tree has a stable id, a dirty tree
# never repeats one. Runs on Linux (no Apple toolchain needed).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=_lib.sh
source "${HERE}/_lib.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

git init -q "$tmp/repo"
git -C "$tmp/repo" config user.email t@example.com
git -C "$tmp/repo" config user.name t
echo a > "$tmp/repo/file"
git -C "$tmp/repo" add file
git -C "$tmp/repo" commit -qm init

WORKSPACE_ROOT="$tmp/repo"
PROFILE=release
FEATURES=srt

c1="$(build_id)"; c2="$(build_id)"
[ "$c1" = "$c2" ] || fail "clean tree: ids differ"
printf '%s\n' "$c1" | grep -qx clean || fail "clean tree: no 'clean' line"

echo x >> "$tmp/repo/file"
d1="$(build_id)"; d2="$(build_id)"
[ "$d1" != "$d2" ] || fail "dirty tree: two ids match"
printf '%s\n' "$d1" | grep -q '^dirty ' || fail "dirty tree: no 'dirty' line"

echo y >> "$tmp/repo/file"
d3="$(build_id)"
[ "$d3" != "$d2" ] || fail "re-edited dirty file: id unchanged"

# A dirty submodule must count as dirty; building a real one is too heavy here.
grep -q -- '--ignore-submodules=none' "${HERE}/_lib.sh" \
  || fail "build_id does not pass --ignore-submodules=none"

echo "ok"
