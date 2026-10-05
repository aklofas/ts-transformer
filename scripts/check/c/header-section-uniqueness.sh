#!/usr/bin/env bash
# Verify that bindings/c/include/tstrans.h has between 7 and 10 sections,
# marked by `// ─── NAME ──────` dividers, and that every divider name is
# unique.
#
# add_section_dividers's post-process is specified to emit 7 required
# sections (INTROSPECTION, MUX SENDER, TS SENDER, RAW SENDER, DEMUX
# RECEIVER, TS RECEIVER, RAW RECEIVER) plus up to 3 conditional sections
# (KLV, LIFETIME, OTHER) that only emit when non-empty. KLV (ST 0601 KLV
# decode surface, ABI 21) bumped the upper bound from 9 to 10.
#
# A regression here means add_section_dividers reverted to a line-by-line
# transition-emission shape, which produces 16 dividers with 7 sections
# duplicated against cbindgen's name-sorted output.

set -euo pipefail

cd "$(dirname "$0")/../../.."

HEADER="bindings/c/include/tstrans.h"
if [ ! -f "$HEADER" ]; then
    echo "FAIL: header not found at $HEADER"
    exit 1
fi

DIVIDERS=$(grep -E '^// ─── ' "$HEADER" || true)
COUNT=$(printf '%s\n' "$DIVIDERS" | grep -c . || true)

if [ "$COUNT" -lt 7 ] || [ "$COUNT" -gt 10 ]; then
    echo "FAIL: $HEADER has $COUNT section dividers; expected 7-10."
    echo "Plan B specifies 7 required + up to 3 conditional sections."
    echo "Observed dividers:"
    printf '%s\n' "$DIVIDERS" | sed 's/^/  /'
    exit 1
fi

NAMES=$(printf '%s\n' "$DIVIDERS" | sed 's/^\/\/ ─── //' | sed 's/ ─.*$//')
UNIQUE=$(printf '%s\n' "$NAMES" | sort -u | grep -c . || true)

if [ "$UNIQUE" -ne "$COUNT" ]; then
    DUPES=$(printf '%s\n' "$NAMES" | sort | uniq -d)
    echo "FAIL: $HEADER has duplicate section divider names:"
    printf '%s\n' "$DUPES" | sed 's/^/  /'
    echo
    echo "add_section_dividers in bindings/c/build.rs should emit each"
    echo "section at most once. A duplicate name signals the post-process"
    echo "regressed to line-by-line transition emission against sort_by=Name."
    exit 1
fi

echo "OK: $HEADER has $COUNT unique section dividers"
