#!/usr/bin/env bash
# api-stability-coverage: every level-1 public module of every ratcheted
# crate must be covered by an EXACT row in docs/reference/api-stability.md
# (or a (crate) row), and every table row must still name a real `pub mod`
# line. Level-2 completeness is additionally enforced under tst_core::klv
# and tst_core::codec (tiering is per-dialect there). Coverage is
# deliberately strict, not prefix-based: a level-1 row must not be able to
# silently stand in for a missing level-2 row, or vice versa.
#
# Known coarsenesses (accepted):
# - Module extraction is scoped to `pub mod <lib>::<name>` lines anchored
#   at end-of-line, so it only ever sees real module paths — it does NOT
#   fall back to a bare `<lib>::[a-z_0-9]+` grep, which would also match
#   lowercase type/field names inside trait-impl or bindgen union lines
#   (e.g. `rist_sys::rist_stats_receiver_flow`) and demand table rows for
#   things that aren't modules at all.
# - Root-level re-exports (`tst_core::Transport`, `tst_rtp::RtspClient`)
#   never produce a module path here (the regex requires a lowercase
#   first character after `<lib>::`, and re-exported items are typically
#   PascalCase types or already covered by the module that defines them)
#   — correct, per the page's "Root-level re-exports" section: a
#   root item's tier is whatever its defining module's row says.
set -euo pipefail
[ "$(uname -s)" = "Linux" ] || { echo "api-stability-coverage: SKIP (linux-only)"; exit 0; }
cd "$(dirname "$0")/../../.."
TABLE=docs/reference/api-stability.md
fail=0

# pkg:dir:libname for every crate with a public-api.txt baseline (tstrans-srt-sys
# has never been ratcheted, so it is absent and its (crate) row is documentation-only).
CRATES="$(bash scripts/lib/roster.sh api-stability)"

# rows: "pkg<TAB>module" (module may be "(crate)")
# -F'|' on `| Package | Module | Tier | Why |` gives $1="" (before the
# first pipe), $2=Package, $3=Module, $4=Tier, $5=Why — verified against
# the real table before wiring this rail.
rows=$(awk -F'|' '/^\|/ && $4 ~ /Stable|Provisional|Experimental|Internal/ {
  gsub(/ /,"",$2); gsub(/ /,"",$3); print $2 "\t" $3 }' "$TABLE")

covered() { # covered <pkg> <modpath> → 0 only on an EXACT row match or (crate)
  # Strict on purpose: a prefix match here would let a level-1 row silently
  # stand in for a missing level-2 row (or vice versa), making the
  # completeness loops below vacuous. Every module this function is asked
  # about must have its own row, full stop.
  local pkg=$1 mod=$2
  while IFS=$'\t' read -r rpkg rmod; do
    [ "$rpkg" = "$pkg" ] || continue
    [ "$rmod" = "(crate)" ] && return 0
    [ "$rmod" = "$mod" ] && return 0
  done <<<"$rows"
  return 1
}

for triple in $CRATES; do
  IFS=: read -r pkg dir lib <<<"$triple"
  base="$dir/public-api.txt"
  [ -f "$base" ] || { echo "FAIL: missing baseline $base"; fail=1; continue; }
  mods=$(grep -oE "^pub mod ${lib}::[a-z_0-9]+\$" "$base" | sed "s/^pub mod ${lib}:://" | sort -u || true)
  for m in $mods; do
    covered "$pkg" "$m" || { echo "FAIL: $pkg::$m has no stability row"; fail=1; }
  done
  # level-2 completeness for the per-dialect trees
  if [ "$pkg" = "tst-core" ]; then
    for top in klv codec; do
      l2=$(grep -oE "^pub mod ${lib}::${top}::[a-z_0-9]+\$" "$base" | sed "s/^pub mod ${lib}:://" | sort -u || true)
      for m in $l2; do
        covered "$pkg" "$m" || { echo "FAIL: $pkg::$m (level-2) has no stability row"; fail=1; }
      done
    done
  fi
done

# reverse direction: every row's module must be a real `pub mod` line in its
# baseline — anchored the same way as the forward extraction, so a row
# naming a struct/enum/impl (e.g. `error::CotError`) can't pass by matching
# incidental text. This also correctly validates override rows that live
# below the top level, like `mpegts::demux::low_level`.
#
# (crate) rows are skipped before the package lookup below, so
# documentation-only packages like tstrans-srt-sys (which has no
# `public-api.txt` baseline and is deliberately absent from $CRATES) never
# reach it. A per-module row for a baseline-less package DOES fail,
# deliberately — it would be unverifiable.
while IFS=$'\t' read -r rpkg rmod; do
  [ "$rmod" = "(crate)" ] && continue
  triple=$(grep "^${rpkg}:" <<<"$CRATES") || { echo "FAIL: table row for unknown package $rpkg"; fail=1; continue; }
  IFS=: read -r _ dir lib <<<"$triple"
  grep -qE "^pub mod ${lib}::${rmod}\$" "$dir/public-api.txt" \
    || { echo "FAIL: table row $rpkg::$rmod matches nothing in $dir/public-api.txt"; fail=1; }
done <<<"$rows"

[ "$fail" -eq 0 ] && echo "api-stability-coverage: OK"
exit "$fail"
