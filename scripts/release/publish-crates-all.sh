#!/usr/bin/env bash
# Rehearse the whole crates.io release train: every dependency layer, in
# order, through publish-crates-layer.sh as `cargo publish --dry-run`.
#
#   usage: PUBLISH=0 publish-crates-all.sh
#
# The layer lists have one source: the `run: bash scripts/release/
# publish-crates-layer.sh <pkg>...` steps of the tag-only publish job in
# .github/workflows/crates-io.yml (scripts/check/repo/roster-sync.sh checks
# those against the roster). Reading them from there makes the rehearsal
# package exactly what the tag publish ships, in the same order.
#
# Rehearsal only: PUBLISH must be 0 or unset. The real publish mints one
# short-lived token per layer inside the publish job, so a single run of
# every layer never holds a publishing token.
#
# Like the layer script, a rehearsal at an unpublished version needs the
# `[patch.crates-io]` table from dry-run-patch-config.sh first.
# CRATES_IO_WORKFLOW overrides the workflow path (the script's own test).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
wf="${CRATES_IO_WORKFLOW:-$root/.github/workflows/crates-io.yml}"

case "${PUBLISH:-0}" in
  0) ;;
  *) echo "publish-crates-all.sh rehearses only (PUBLISH=${PUBLISH}); the tag publish runs one layer per step with its own token" >&2; exit 2 ;;
esac

layers="$(sed -nE 's/^[[:space:]]*run: bash scripts\/release\/publish-crates-layer\.sh(( [a-z0-9-]+)+)[[:space:]]*$/\1/p' "$wf")"
[ -n "$layers" ] || { echo "no publish-crates-layer.sh layer steps in $wf" >&2; exit 2; }

n=0
while read -r pkgs; do
  n=$((n + 1))
  echo "== layer $n: $pkgs"
  # shellcheck disable=SC2086 # one word per crate
  PUBLISH=0 bash "$root/scripts/release/publish-crates-layer.sh" $pkgs
done <<<"$layers"
echo "== rehearsed $n layers"
