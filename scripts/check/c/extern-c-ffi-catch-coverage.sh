#!/usr/bin/env bash
# Verify that every `pub unsafe extern "C" fn` in bindings/c/core/src/
# isolates panics across the C boundary. Three acceptable forms:
#
#   1. The body wraps work in `crate::panic::ffi_catch(...)` (the
#      open-path + builder-setter pattern).
#   2. The body uses `Handle::with_inner_mut(...)` or
#      `Handle::with_inner_ref(...)` — both already wrap their closure
#      in `catch_unwind` internally (see bindings/c/core/src/handle.rs).
#   3. The function name is in the trivially-infallible allowlist
#      below (constant returns with no internal locking, panics, or
#      allocation).
#
# The existing
# `scripts/check/c/lifecycle-ffi-catch-coverage.sh` only covers `_close` /
# `_cancel` entries — `tst_demux_config_free` slipped past that
# ratchet by being a `_free`. This ratchet enumerates *every*
# `pub unsafe extern "C" fn` in tst-c, so future entry points cannot
# accidentally bypass the panic-isolation policy.
#
# Cross-language unwinding past a C frame is undefined behavior under
# `panic="unwind"` and an abort with no last-error visibility under
# `panic="abort"`. See bindings/c/core/src/panic.rs for the contract.

set -euo pipefail

SRC_DIR="bindings/c/core/src"

# Allowlist of function names that are trivially infallible: no
# pointer dereferences, no allocations, no internal locks, no calls
# into tst-core. They return a process-lifetime constant (or a packed
# integer). Keep this list small and intentional.
ALLOWLIST=(
    "tst_get_abi_version_major"
    "tst_get_abi_version_minor"
    "tst_get_version_major"
    "tst_get_version_minor"
    "tst_get_version_patch"
    "tst_get_version_packed"
    "tst_get_version_string"
)

is_allowlisted() {
    local name="$1"
    for entry in "${ALLOWLIST[@]}"; do
        if [[ "$entry" == "$name" ]]; then
            return 0
        fi
    done
    return 1
}

# Step 1: enumerate every signature line. Bash 3.2-portable
# read-into-array pattern (no `mapfile`/`readarray`, no `declare -A`).
ENTRIES=()
while IFS= read -r entry; do
    ENTRIES+=("$entry")
done < <(
    grep -rEn '^pub unsafe extern "C" fn tst_[a-zA-Z0-9_]+' \
        "$SRC_DIR" \
        --include='*.rs' \
    | sort
)

if [[ ${#ENTRIES[@]} -eq 0 ]]; then
    echo "FAIL: found 0 extern \"C\" entry points — grep pattern may have drifted from source layout"
    exit 1
fi

missing=0
checked=0
allowlisted=0

for entry in "${ENTRIES[@]}"; do
    file="${entry%%:*}"
    rest="${entry#*:}"
    lineno="${rest%%:*}"

    # Extract function name. Pattern: `pub unsafe extern "C" fn NAME(` or
    # `pub unsafe extern "C" fn NAME<` or `pub unsafe extern "C" fn NAME `.
    sig_line=$(sed -n "${lineno}p" "$file")
    fn_name=$(echo "$sig_line" \
        | sed -E 's/^pub unsafe extern "C" fn ([a-zA-Z0-9_]+).*/\1/')

    if [[ -z "$fn_name" ]]; then
        echo "FAIL: could not parse function name at ${file}:${lineno}"
        echo "      line: ${sig_line}"
        exit 1
    fi

    # Allowlist short-circuit.
    if is_allowlisted "$fn_name"; then
        allowlisted=$((allowlisted + 1))
        continue
    fi

    # Step 2: extract the function body. Rustfmt formats every public
    # function with the signature `pub unsafe extern "C" fn ...` at
    # column 0 and the closing brace `}` also at column 0. Read from
    # the signature line through the next `^}` line (inclusive).
    body=$(awk -v start="$lineno" '
        NR >= start {
            print
            if (NR > start && $0 == "}") {
                exit
            }
        }
    ' "$file")

    # Step 3: scan body for one of the panic-isolation patterns. The
    # `with_mux_publisher(` HLS helper (bindings/c/core/src/hls/mux_publisher.rs)
    # wraps its closure in crate::panic::ffi_catch internally — same contract
    # as Handle::with_inner_mut — so callers delegating through it are isolated.
    # `crate::transport_impls::` calls (WP18 genericization) forward directly
    # to generic bodies in transport_impls.rs, which route through with_inner_mut
    # / with_inner_ref internally; the isolation guarantee is preserved
    # one hop away but is still unconditional.
    # Here-string, NOT `echo "$body" | grep -q`: `grep -q` closes the pipe on
    # first match, `echo` then dies with SIGPIPE, and under `set -o pipefail`
    # that turns a MATCH into a pipeline failure → a bogus MISSING. The race is
    # timing-dependent (flaky; surfaced on the macOS runner). A here-string has
    # no pipe, so there is no SIGPIPE to mask the match.
    if grep -qE 'crate::panic::ffi_catch\(|^\s*ffi_catch\(|\.with_inner_(mut|ref)\(|with_mux_publisher\(|crate::transport_impls::' <<<"$body"; then
        checked=$((checked + 1))
        continue
    fi

    echo "MISSING: ${fn_name} at ${file}:${lineno} has no ffi_catch / with_inner_mut / with_inner_ref / allowlist entry"
    missing=$((missing + 1))
done

if [[ $missing -gt 0 ]]; then
    echo
    echo "FAIL: $missing of ${#ENTRIES[@]} extern \"C\" entry points bypass panic isolation"
    echo
    echo "Fix options:"
    echo "  1. Wrap the body in crate::panic::ffi_catch(default, || { ... })"
    echo "  2. Route mutation through Handle::with_inner_mut(|inner| { ... })"
    echo "  3. If the function is provably infallible (constant return,"
    echo "     no allocation, no locking), add it to the ALLOWLIST in"
    echo "     scripts/check/c/extern-c-ffi-catch-coverage.sh"
    exit 1
fi

echo "OK: ${#ENTRIES[@]} extern \"C\" entry points wrap panic isolation"
echo "    ${checked} via ffi_catch / with_inner_{mut,ref}"
echo "    ${allowlisted} via allowlist (trivially infallible)"

# Companion assertion for the `crate::transport_impls::` pattern above: the
# pattern trusts the WHOLE module, so enforce the module's isolation contract
# here — every pub(crate) fn in transport_impls.rs must itself route through
# with_inner_mut / with_inner_ref (or ffi_catch). Without this loop, a future
# fn added to the module without isolation would be silently accepted.
TI_FILE="bindings/c/core/src/transport_impls.rs"
if [[ -f "$TI_FILE" ]]; then
    ti_missing=0
    ti_total=0
    while IFS= read -r sig_entry; do
        ti_lineno="${sig_entry%%:*}"
        ti_total=$((ti_total + 1))
        # Body = from the signature line to the next top-level closing brace.
        ti_body=$(awk -v start="$ti_lineno" \
            'NR>=start{print; if(NR>start && $0=="}"){exit}}' "$TI_FILE")
        # Here-string, not a pipe (same SIGPIPE rationale as the main loop).
        if ! grep -qE 'with_inner_(mut|ref)\(|ffi_catch\(' <<<"$ti_body"; then
            echo "MISSING isolation in transport_impls.rs fn at line $ti_lineno"
            ti_missing=$((ti_missing + 1))
        fi
    done < <(grep -nE '^pub\(crate\) (unsafe )?fn ' "$TI_FILE")
    if [[ $ti_missing -gt 0 ]]; then
        echo "FAIL: $ti_missing of $ti_total transport_impls fns lack internal isolation"
        exit 1
    fi
    echo "    ${ti_total} transport_impls generic bodies verified isolated"
fi
