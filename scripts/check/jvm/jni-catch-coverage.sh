#!/usr/bin/env bash
# Verify every `pub extern "system" fn Java_org_tstrans_*` in bindings/jvm/src/
# isolates panics across the JNI boundary by wrapping its body in
# `crate::panic::jni_catch(...)` (or bare `jni_catch(...)`). A panic unwinding
# out of `extern "system"` aborts the JVM; jni_catch converts it to a thrown
# RuntimeException. See bindings/jvm/src/panic.rs for the contract.
#
# Two acceptable forms:
#   1. The body calls `jni_catch(...)` (the standard wrapper).
#   2. The function name is in the trivially-infallible allowlist below.
#
# Bash 3.2-portable (no mapfile/readarray/declare -A; here-strings not
# pipes).

set -euo pipefail

SRC_DIR="bindings/jvm/src"

# Trivially-infallible natives: no allocation, no locking, no tst-core call,
# no panic site. Keep small + intentional. (Empty today — the bootstrap
# Version.versionString allocates a Java string via .expect(), so it is NOT
# infallible and must be wrapped.)
ALLOWLIST=(
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

ENTRIES=()
while IFS= read -r entry; do
    ENTRIES+=("$entry")
done < <(
    grep -rEn '^pub extern "system" fn Java_org_tstrans_[a-zA-Z0-9_]+' \
        "$SRC_DIR" --include='*.rs' | sort
)

if [[ ${#ENTRIES[@]} -eq 0 ]]; then
    echo "FAIL: found 0 JNI entry points — grep pattern may have drifted from source layout"
    exit 1
fi

missing=0
checked=0
allowlisted=0

for entry in "${ENTRIES[@]}"; do
    file="${entry%%:*}"
    rest="${entry#*:}"
    lineno="${rest%%:*}"

    sig_line=$(sed -n "${lineno}p" "$file")
    fn_name=$(echo "$sig_line" \
        | sed -E 's/^pub extern "system" fn ([a-zA-Z0-9_]+).*/\1/')

    if [[ -z "$fn_name" ]]; then
        echo "FAIL: could not parse function name at ${file}:${lineno}"
        echo "      line: ${sig_line}"
        exit 1
    fi

    if is_allowlisted "$fn_name"; then
        allowlisted=$((allowlisted + 1))
        continue
    fi

    # rustfmt puts the signature `pub extern "system" fn ...` at column 0 and
    # the closing `}` at column 0. Read from the signature through the next `^}`.
    body=$(awk -v start="$lineno" '
        NR >= start {
            print
            if (NR > start && $0 == "}") { exit }
        }
    ' "$file")

    # here-string (NOT echo|grep -q) to avoid SIGPIPE-under-pipefail flakes.
    if grep -qE 'jni_catch\(' <<<"$body"; then
        checked=$((checked + 1))
        continue
    fi

    echo "MISSING: ${fn_name} at ${file}:${lineno} has no jni_catch / allowlist entry"
    missing=$((missing + 1))
done

if [[ $missing -gt 0 ]]; then
    echo
    echo "FAIL: $missing of ${#ENTRIES[@]} JNI entry points bypass panic isolation"
    echo
    echo "Fix: wrap the body in crate::panic::jni_catch(&mut env, <default>, |env| { ... })"
    echo "     (see the default-value table in bindings/jvm/src/panic.rs),"
    echo "     or, if provably infallible, add the fn name to the ALLOWLIST above."
    exit 1
fi

echo "OK: ${#ENTRIES[@]} JNI entry points wrap panic isolation"
echo "    ${checked} via jni_catch"
echo "    ${allowlisted} via allowlist (trivially infallible)"
