#!/usr/bin/env bash
# Driver: run the `py` and `pyarm` rows of error-mapping.tsv through
# lib/coverage.sh. Each `py` row asserts every `class <Enum>ErrorKind` member
# in tstrans.exceptions has a make_<proto>_error call site under
# bindings/python/src/. Each `pyarm` row asserts every tst-core Rust enum
# variant has an explicit arm in a named Python-binding .rs source file.
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"          # scripts/ratchets -> scripts -> workspace root
# shellcheck source=lib/coverage.sh
. "$DIR/lib/coverage.sh"

TSV="$DIR/error-mapping.tsv"
EXC_FILE="$ROOT/bindings/python/python/tstrans/exceptions.py"
SRC_DIR="$ROOT/bindings/python/src"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --tsv) TSV="$2"; shift 2 ;;
        --exc-file) EXC_FILE="$2"; shift 2 ;;
        --src-dir) SRC_DIR="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done
if [[ ! -f "$TSV" ]]; then echo "FAIL: TSV $TSV not found" >&2; exit 1; fi

rc=0
rows=0
while IFS=$'\t' read -r lang name col3 col4 col5 col6 col7; do
    [[ "$lang" == \#* || -z "${lang:-}" ]] && continue
    case "$lang" in
        py)
            rows=$((rows + 1))
            if ! assert_py_row "$col3" "$col4" "$EXC_FILE" "$SRC_DIR"; then
                rc=1
            fi
            ;;
        pyarm)
            rows=$((rows + 1))
            col7="${col7%$'\r'}"  # tolerate a CRLF checkout (match_names is the last field)
            # variant_source/py_file are used as-is (workspace-relative, like
            # the retired `rust` driver) — resolves correctly
            # because every caller (CI steps, the pre-push rail sweep, the
            # hermetic self-test) runs from the workspace root.
            if ! assert_rust_arm_row "$col3" "$col4" "$col5" "$col6" "$col7"; then
                rc=1
            fi
            ;;
        *) continue ;;
    esac
done < "$TSV"

if [[ "$rows" -eq 0 ]]; then
    echo "FAIL: no py/pyarm rows found in $TSV (malformed table?)" >&2
    exit 1
fi
exit "$rc"
