#!/usr/bin/env bash
# kind-table-coverage: every variant of the tst-core error enums that
# `tst_pipeline::binding::kind` classifies must be named explicitly in its
# classifier BEFORE the `_ =>` wildcard arm.
#
# Those enums are #[non_exhaustive] in tst-core, so a match on them from
# tst-pipeline MUST carry a wildcard (E0004 otherwise) and rustc cannot tell
# us when a new variant silently lands on it as INTERNAL. This ratchet can.
#
# Classifier names: the five enums whose wildcard decides "unmapped" have
# their arms in the private `map_<domain>` helpers (which return
# Option<BindingErrorKind>, the None arm being the wildcard); the public
# `kind_of_<domain>` wrappers just default None to INTERNAL and hold no arms.
# TransportError's arms are still inline in `kind_of_transport`.
#
# MuxError is deliberately not a row: kind_of_mux keeps four overrides and
# delegates everything else to MuxError::kind(), whose own per-variant
# coverage is scripts/check/rust/mux-error-kind-coverage.sh.
#
# Same extractor as scripts/check/c/raw-mapper-coverage.sh (deleted in WP-B1).
# GNU grep -P → Linux only; CI runs it on the linux-x86_64 leg.

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "kind-table-coverage: SKIP (linux-only: needs GNU grep -P)"
    exit 0
fi

cd "$(dirname "$0")/../../.."

KIND_RS="crates/tst-pipeline/src/binding/kind.rs"

# Each row: enum-file | enum-short-name | classifier-fn-name
ENUMS=(
    "crates/tst-core/src/transport.rs|TransportError|kind_of_transport"
    "crates/tst-core/src/error.rs|DemuxError|map_demux"
    "crates/tst-core/src/error.rs|KlvDecodeError|map_klv_decode"
    "crates/tst-core/src/error.rs|KlvFieldError|map_klv_field"
    "crates/tst-core/src/error.rs|KlvEncodeError|map_klv_encode"
    "crates/tst-core/src/codec/mod.rs|CodecParseError|map_codec"
)

# Prints the classifier's body up to (not including) its `_ =>` wildcard arm.
#
# Comments are stripped before anything is printed or counted: the copied
# extractor greps the body text, so without this a COMMENTED-OUT arm would
# still satisfy the coverage check (verified — the plan's own mutation did not
# bite until this was added). Stripping also keeps the brace-depth count honest
# when a comment contains a brace.
extract_function_body_before_wildcard() {
    local file="$1"
    local fn_name="$2"
    awk -v fn="fn $fn_name(" '
        BEGIN { inside = 0; depth = 0; started = 0 }
        index($0, fn) { inside = 1 }
        inside {
            line = $0
            sub(/\/\/.*$/, "", line)
        }
        inside && started == 0 {
            for (i = 1; i <= length(line); i++) {
                c = substr(line, i, 1)
                if (c == "{") { depth++; started = 1 }
            }
            print line
            next
        }
        inside && started {
            trimmed = line
            sub(/^[ \t]+/, "", trimmed)
            if (trimmed ~ /^_[ \t]*=>/) { exit }
            print line
            for (i = 1; i <= length(line); i++) {
                c = substr(line, i, 1)
                if (c == "{") depth++
                else if (c == "}") { depth--; if (depth == 0) exit }
            }
        }
    ' "$file"
}

extract_enum_variants() {
    local file="$1"
    local enum_short="$2"
    awk -v name="pub enum $enum_short {" '
        BEGIN { inside = 0; depth = 0 }
        index($0, name) { inside = 1 }
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
    fn_name="${rest#*|}"

    variants=$(extract_enum_variants "$enum_file" "$enum_short")
    if [[ -z "$variants" ]]; then
        echo "FAIL: zero variants extracted for $enum_short in $enum_file"
        exit 1
    fi

    fn_body=$(extract_function_body_before_wildcard "$KIND_RS" "$fn_name")
    if [[ -z "$fn_body" ]]; then
        echo "FAIL: could not locate fn $fn_name in $KIND_RS"
        exit 1
    fi

    n=0
    for v in $variants; do
        n=$((n + 1))
        total=$((total + 1))
        if ! printf '%s\n' "$fn_body" | grep -q "$enum_short::$v\b"; then
            echo "MISSING: $enum_short::$v is not named in $fn_name before its wildcard"
            missing=$((missing + 1))
        fi
    done

    echo "checked $enum_short ($n variants) -> $fn_name"
done

if [[ $missing -gt 0 ]]; then
    echo ""
    echo "FAIL: $missing variant(s) fall through to INTERNAL in $KIND_RS"
    echo "Add an explicit arm before the wildcard (and a BindingErrorKind row"
    echo "if the variant deserves a kind of its own)."
    exit 1
fi

echo "OK: all $total tst-core variant(s) classified explicitly in $KIND_RS"
