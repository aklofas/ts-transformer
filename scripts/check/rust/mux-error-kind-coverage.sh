#!/usr/bin/env bash
# Verify every MuxError variant is matched explicitly in the
# MuxError::kind() method body in crates/tst-core/src/error.rs BEFORE
# the function's #[non_exhaustive] wildcard arm.
#
# MuxError is #[non_exhaustive] requiring a wildcard, so Rust's
# exhaustiveness checker can't catch a missing arm when the kind()
# match drops a variant. This ratchet does.
#
# Inner-tier sibling of:
# - scripts/check/rust/pipeline-kind-classification.sh (outer-tier kind_from_mux)
# - scripts/check/rust/kind-table-coverage.sh (BindingErrorKind's kind_of_mux,
#   which raw record_mux_error now delegates to)
#
# Wave 6.D shipped 2026-05-19 (see
# docs/plans/2026-05-19-wave-6-muxerror-reshape.md).

set -euo pipefail

ERROR_FILE="crates/tst-core/src/error.rs"

extract_function_body_before_wildcard() {
    local file="$1"
    local fn_name="$2"
    awk -v fn="fn $fn_name" '
        BEGIN { inside = 0; depth = 0; started = 0 }
        $0 ~ fn { inside = 1 }
        inside && started == 0 {
            for (i = 1; i <= length($0); i++) {
                c = substr($0, i, 1)
                if (c == "{") { depth++; started = 1 }
            }
            print
            next
        }
        inside && started {
            trimmed = $0
            sub(/^[ \t]+/, "", trimmed)
            if (trimmed ~ /^_[ \t]*=>/) { exit }
            print
            for (i = 1; i <= length($0); i++) {
                c = substr($0, i, 1)
                if (c == "{") depth++
                else if (c == "}") { depth--; if (depth == 0) exit }
            }
        }
    ' "$file"
}

extract_enum_variants() {
    local file="$1"
    local enum_short="$2"
    awk -v name="pub enum $enum_short" '
        BEGIN { inside = 0; depth = 0 }
        $0 ~ name { inside = 1 }
        inside {
            for (i = 1; i <= length($0); i++) {
                c = substr($0, i, 1)
                if (c == "{") depth++
                else if (c == "}") { depth--; if (depth == 0) { print; exit } }
            }
            print
        }
    ' "$file" | grep -oP '^\s*\K[A-Z][A-Za-z0-9]+(?=\s*[,({])'
}

variants=$(extract_enum_variants "$ERROR_FILE" "MuxError")

if [[ -z "$variants" ]]; then
    echo "FAIL: no MuxError variants found in $ERROR_FILE"
    exit 1
fi

fn_body=$(extract_function_body_before_wildcard "$ERROR_FILE" "kind")

if [[ -z "$fn_body" ]]; then
    echo "FAIL: could not locate fn kind in $ERROR_FILE"
    exit 1
fi

missing=0
total=0
for v in $variants; do
    total=$((total + 1))
    if ! echo "$fn_body" | grep -q "MuxError::$v\b"; then
        echo "MISSING: MuxError::$v not handled in MuxError::kind() (before wildcard)"
        missing=$((missing + 1))
    fi
done

if [[ $missing -gt 0 ]]; then
    echo ""
    echo "FAIL: $missing MuxError variant(s) not classified in MuxError::kind()"
    echo "Add an explicit arm to fn kind() in $ERROR_FILE BEFORE the wildcard _ => arm."
    echo "Route each new variant to the appropriate MuxErrorKind category — see"
    echo "docs/plans/2026-05-19-wave-6-muxerror-reshape.md Pre-flight Fact D4 for"
    echo "the canonical routing table."
    exit 1
fi

echo "OK: all $total MuxError variant(s) classified in MuxError::kind()"
