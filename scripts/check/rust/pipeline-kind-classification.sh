#!/usr/bin/env bash
# Verify every variant of each inner error enum (MuxError, DemuxError,
# TransportError, TsFramingError) is matched explicitly in the
# corresponding kind_from_* helper in shell_error.rs BEFORE the
# helper's wildcard arm.
#
# Each inner enum is #[non_exhaustive] requiring a wildcard, so Rust's
# exhaustiveness checker can't catch a missing arm. This ratchet does.
#
# Replaces the variant-level monolithic tst-c error-coverage ratchet
# (deleted in Plan A Task 10) for the kind-derivation layer.

set -euo pipefail

SHELL_ERROR_FILE="crates/tst-pipeline/src/shell_error.rs"

# Each row: enum-file | enum-short-name | helper-fn-name
ENUMS=(
    "crates/tst-core/src/error.rs|MuxError|kind_from_mux"
    "crates/tst-core/src/transport.rs|TransportError|kind_from_transport"
    "crates/tst-core/src/error.rs|DemuxError|kind_from_demux"
    "crates/tst-pipeline/src/sender/framing.rs|TsFramingError|kind_from_framing"
)

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

missing=0
total=0
for spec in "${ENUMS[@]}"; do
    enum_file="${spec%%|*}"
    rest="${spec#*|}"
    enum_short="${rest%%|*}"
    fn_name="${rest##*|}"

    variants=$(extract_enum_variants "$enum_file" "$enum_short")
    if [[ -z "$variants" ]]; then
        echo "WARN: zero variants for $enum_short in $enum_file"
        continue
    fi

    fn_body=$(extract_function_body_before_wildcard "$SHELL_ERROR_FILE" "$fn_name")
    if [[ -z "$fn_body" ]]; then
        echo "FAIL: could not locate fn $fn_name in $SHELL_ERROR_FILE"
        exit 1
    fi

    n=0
    for v in $variants; do
        n=$((n + 1))
        total=$((total + 1))
        if ! echo "$fn_body" | grep -q "$enum_short::$v\b"; then
            echo "MISSING: $enum_short::$v not handled in $fn_name (before wildcard)"
            missing=$((missing + 1))
        fi
    done

    echo "checked $enum_short ($n variants) -> $fn_name"
done

if [[ $missing -gt 0 ]]; then
    echo ""
    echo "FAIL: $missing inner-enum variant(s) not classified at the pipeline layer"
    echo "Add an explicit arm to the relevant kind_from_* helper in"
    echo "$SHELL_ERROR_FILE BEFORE the helper's wildcard _ => arm."
    exit 1
fi

echo "OK: all $total inner-enum variant(s) classified across 4 kind_from_* helpers"
