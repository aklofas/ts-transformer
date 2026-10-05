#!/usr/bin/env bash
# Verify that every C ABI lifecycle entry (_close + _cancel) in
# bindings/c/core/src/{sender,receiver}/ wraps its body in
# `crate::panic::ffi_catch(...)`.
#
# Every lifecycle entry (24 today: 12 _close + 12 _cancel) wraps its body
# in ffi_catch so a panic never unwinds across the C frame (UB). Two unit
# tests in panic.rs pin that ffi_catch *itself* works, but they do NOT
# pin the contract that each wrapper actually uses it — a future commit
# accidentally dropping the wrap from one entry would slip through. This
# ratchet closes that gap.
#
# Mechanic: enumerate every `pub (unsafe )?extern "C" fn
# tst_<name>_(close|cancel)\b` signature, then assert that the
# 5-line window following the signature contains
# `crate::panic::ffi_catch(`.

set -euo pipefail

# Paths relative to ts-transformer/ workspace root.
SRC_DIRS=(
    "bindings/c/core/src/sender"
    "bindings/c/core/src/receiver"
)

# Window size (in lines) after the signature line where the
# `crate::panic::ffi_catch(` call must appear. The convention places it
# on the line immediately after `{`, so 5 is comfortably above the
# floor while still tight enough to catch accidental unwrapping.
WINDOW=5

# Step 1: enumerate lifecycle entries (file:lineno pairs).
#
# Portable read-into-array pattern (bash 3.2+, including macOS default
# bash 3.2.57). `mapfile`/`readarray` are bash 4.0+ only and silently
# fail with "command not found" on macOS.
ENTRIES=()
while IFS= read -r entry; do
    ENTRIES+=("$entry")
done < <(
    grep -rEn 'pub (unsafe )?extern "C" fn tst_\w+_(close|cancel)\b' \
        "${SRC_DIRS[@]}" \
        --include='*.rs' \
    | sort
)

if [[ ${#ENTRIES[@]} -eq 0 ]]; then
    echo "FAIL: found 0 lifecycle entries — grep pattern may have drifted from source layout"
    exit 1
fi

missing=0
for entry in "${ENTRIES[@]}"; do
    file="${entry%%:*}"
    rest="${entry#*:}"
    lineno="${rest%%:*}"

    # Extract the function name from the signature for a clear error msg.
    fn_name=$(sed -n "${lineno}p" "$file" \
        | grep -oE 'tst_\w+_(close|cancel)\b' \
        | head -1)

    start=$((lineno + 1))
    end=$((lineno + WINDOW))
    window=$(sed -n "${start},${end}p" "$file")

    if ! echo "$window" | grep -q 'crate::panic::ffi_catch('; then
        echo "MISSING: ${fn_name} at ${file}:${lineno} does not call crate::panic::ffi_catch( within ${WINDOW} lines"
        missing=$((missing + 1))
    fi
done

if [[ $missing -gt 0 ]]; then
    echo "FAIL: $missing lifecycle entries not wrapped in ffi_catch"
    exit 1
fi

echo "OK: ${#ENTRIES[@]} lifecycle entries wrapped in ffi_catch"
