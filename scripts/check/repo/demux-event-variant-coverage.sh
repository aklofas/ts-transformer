#!/usr/bin/env bash
# `DemuxEvent` is `#[non_exhaustive]`, so the three binding matchers (C, Python,
# JVM) carry a wildcard arm and the compiler no longer proves that every variant
# is projected. This rail restores the proof textually:
#
#   1. Every `DemuxEvent` variant declared in tst-core has an explicit
#      `DemuxEvent::<Variant>` arm in each matcher.
#   2. Each matcher has exactly one wildcard arm (the match-level `_ =>` at the
#      arm indent), so a future variant fails loudly instead of being dropped.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
RUST_ENUM_FILE="$ROOT/crates/tst-core/src/mpegts/demux/event.rs"

# file|function-signature prefix (the matcher lives in this function).
MATCHERS=(
    "bindings/c/core/src/event.rs|pub(crate) fn convert("
    "bindings/python/src/mpegts.rs|pub(crate) fn convert_event("
    "bindings/jvm/src/mpegts/mod.rs|pub(crate) fn convert_event"
)

# Portable read-into-array pattern (bash 3.2+) — no `mapfile`/`readarray`.
variants=()
while IFS= read -r v; do
    variants+=("$v")
done < <(awk '
    /^pub enum DemuxEvent / { in_block = 1; next }
    in_block && /^}/ { in_block = 0 }
    in_block && /^    [A-Z][A-Za-z0-9]*[ ({,]/ {
        sub(/^    /, "")
        sub(/[ {(,].*$/, "")
        print
    }
' "$RUST_ENUM_FILE")

if [ "${#variants[@]}" -eq 0 ]; then
    echo "FAIL: extracted 0 DemuxEvent variants — awk pattern may have drifted from $RUST_ENUM_FILE" >&2
    exit 1
fi

fail=0
for entry in "${MATCHERS[@]}"; do
    rel="${entry%%|*}"
    sig="${entry#*|}"
    file="$ROOT/$rel"
    body=$(awk -v sig="$sig" '
        index($0, sig) == 1 { in_fn = 1 }
        in_fn { print }
        in_fn && /^}/ { in_fn = 0 }
    ' "$file")
    if [ -z "$body" ]; then
        echo "FAIL: could not locate '$sig' in $rel" >&2
        fail=1
        continue
    fi
    for v in "${variants[@]}"; do
        if ! echo "$body" | grep -qE "DemuxEvent::${v}\b"; then
            echo "ERROR: $rel: no arm for DemuxEvent::${v}" >&2
            fail=1
        fi
    done
    wild=$(echo "$body" | grep -cE '^        _ =>' || true)
    if [ "$wild" -ne 1 ]; then
        echo "ERROR: $rel: expected exactly one wildcard arm for the non-exhaustive DemuxEvent, found $wild (no wildcard arm)" >&2
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "OK: all ${#variants[@]} DemuxEvent variants handled in ${#MATCHERS[@]} binding matchers, each with one wildcard arm"
