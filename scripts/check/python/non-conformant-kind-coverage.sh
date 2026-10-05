#!/usr/bin/env bash
# Every NonConformantIssue variant in
# tst-core must appear explicitly in tst-py/src/mpegts.rs
# `non_conformant_kind_name` mapper, AND every output kind string the
# mapper produces must be a member of the Python `NonConformantKind`
# enum in tst-py/python/tstrans/mpegts.py.
#
# `NonConformantKind` is the public analytics surface — analysts pivot
# on it for issue triage. If a new Rust `NonConformantIssue` variant
# slips in unmapped, the binding either panics (the function returns
# `&'static str` with no fallback) or routes silently — both are
# regressions. This ratchet closes both gaps:
#
#   1. Every Rust variant -> handled in non_conformant_kind_name.
#   2. Every output string -> defined as a Python enum member.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
RUST_ENUM_FILE="$ROOT/crates/tst-core/src/mpegts/demux/event.rs"
RUST_MAPPER_FILE="$ROOT/bindings/python/src/mpegts.rs"
PY_ENUM_FILE="$ROOT/bindings/python/python/tstrans/mpegts.py"

# ----------------------------------------------------------------------
# Step 1: extract every NonConformantIssue variant name.
# ----------------------------------------------------------------------
# Portable read-into-array pattern (bash 3.2+) — no `mapfile`/`readarray`.
variants=()
while IFS= read -r v; do
    variants+=("$v")
done < <(awk '
    /^pub enum NonConformantIssue/ { in_block = 1; next }
    in_block && /^}/ { in_block = 0 }
    in_block && /^    [A-Z][A-Za-z0-9]*[ ({,]/ {
        sub(/^    /, "")
        sub(/[ {(,].*$/, "")
        print
    }
' "$RUST_ENUM_FILE")

if [ "${#variants[@]}" -eq 0 ]; then
    echo "FAIL: extracted 0 NonConformantIssue variants — awk pattern may have drifted from $RUST_ENUM_FILE" >&2
    exit 1
fi

# ----------------------------------------------------------------------
# Step 2: extract the body of `non_conformant_kind_name`.
# ----------------------------------------------------------------------
# The mapper uses `use NonConformantIssue::*;` so variants appear as
# bare identifiers (e.g. `MissingMetadataDescriptor =>`); accept either
# qualified or bare form.
mapper_body=$(awk '
    /^fn non_conformant_kind_name/ { in_fn = 1 }
    in_fn { print }
    in_fn && /^}/ { in_fn = 0 }
' "$RUST_MAPPER_FILE")

if [ -z "$mapper_body" ]; then
    echo "FAIL: could not locate fn non_conformant_kind_name in $RUST_MAPPER_FILE" >&2
    exit 1
fi

missing_in_mapper=()
for v in "${variants[@]}"; do
    # Match `NonConformantIssue::Variant`, OR a bareword `Variant` arm
    # introducer ending in `=>` (possibly with `{ .. }` / `(_)` between).
    if ! echo "$mapper_body" | grep -qE "(NonConformantIssue::)?${v}\b"; then
        missing_in_mapper+=("$v")
    fi
done

if [ "${#missing_in_mapper[@]}" -ne 0 ]; then
    echo "ERROR: NonConformantIssue variants missing arm in non_conformant_kind_name ($RUST_MAPPER_FILE):" >&2
    for v in "${missing_in_mapper[@]}"; do echo "  - $v" >&2; done
    echo "" >&2
    echo "Add an explicit arm in \`non_conformant_kind_name\` for each" >&2
    echo "new variant so the Python NonConformant event surface labels" >&2
    echo "it with a stable kind string." >&2
    exit 1
fi

# ----------------------------------------------------------------------
# Step 3: extract every output kind string produced by the mapper.
# ----------------------------------------------------------------------
kinds=()
while IFS= read -r k; do
    # Strip surrounding double quotes.
    k="${k#\"}"
    k="${k%\"}"
    kinds+=("$k")
done < <(echo "$mapper_body" | grep -oE '"[A-Z][A-Z_0-9]*"' | sort -u)

if [ "${#kinds[@]}" -eq 0 ]; then
    echo "FAIL: extracted 0 output kind strings from non_conformant_kind_name body" >&2
    exit 1
fi

# ----------------------------------------------------------------------
# Step 4: confirm each kind string exists as a Python enum member.
# ----------------------------------------------------------------------
# Extract the `NonConformantKind(enum.Enum)` block once, then look up
# each kind string by matching `    NAME = ` at the start of a line.
py_members=$(awk '
    /^class NonConformantKind/ { in_block = 1; next }
    in_block && /^[A-Za-z]/ && !/^    / { in_block = 0 }
    in_block && /^    [A-Z][A-Z_0-9]*[[:space:]]*=/ {
        sub(/[[:space:]]*=.*$/, "")
        sub(/^    /, "")
        print
    }
' "$PY_ENUM_FILE")

if [ -z "$py_members" ]; then
    echo "FAIL: could not locate class NonConformantKind in $PY_ENUM_FILE" >&2
    exit 1
fi

missing_in_py=()
for k in "${kinds[@]}"; do
    if ! echo "$py_members" | grep -qxF "$k"; then
        missing_in_py+=("$k")
    fi
done

if [ "${#missing_in_py[@]}" -ne 0 ]; then
    echo "ERROR: kind strings produced by non_conformant_kind_name not present in Python NonConformantKind enum ($PY_ENUM_FILE):" >&2
    for k in "${missing_in_py[@]}"; do echo "  - $k" >&2; done
    echo "" >&2
    echo "Add the missing member(s) to the Python NonConformantKind enum." >&2
    exit 1
fi

echo "OK: all ${#variants[@]} NonConformantIssue variants handled; all ${#kinds[@]} kind strings present in Python NonConformantKind enum"
