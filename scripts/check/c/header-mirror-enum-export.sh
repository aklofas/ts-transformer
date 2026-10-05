#!/usr/bin/env bash
# Verify every `pub enum Tst<Name>` declared in bindings/c/core/src/ is
# listed in bindings/c/cbindgen.toml's `[export] include = [...]`
# allowlist.
#
# Background: cbindgen uses an explicit allowlist (`[export] include`).
# Enums (and their constants) that are not in the allowlist are silently
# dropped from the generated header, even if they are referenced from
# Rust-side doc comments that survive into the header. The au-cell CFI
# tolerance feature shipped with `TstCellFragmentIndication` defined in
# event.rs and quoted in tst-c rustdoc / NonConformantIssue docs, but the
# symbol was never added to the include list — so C callers got raw
# cc_expected/cc_observed bytes with no way to compare against constants.
# This ratchet prevents the next mirror enum from regressing the same way.
#
# Scope note: we deliberately do NOT verify the rendered header — that
# would require parsing cbindgen's rename rules (`tst_e`, `tst_nonconformant_code`,
# `tst_stream_kind`, ...) which use ad-hoc snake_case shortenings that
# don't follow a simple capitalization rule. Membership in the include
# allowlist is the actual root cause of the CFI bug we're guarding
# against — if it's listed, cbindgen emits it.

set -euo pipefail

cd "$(dirname "$0")/../../.."

CBINDGEN="bindings/c/cbindgen.toml"

[ -f "$CBINDGEN" ] || { echo "FAIL: $CBINDGEN not found"; exit 1; }

ENUMS=$(grep -rhE '^pub enum Tst[A-Z][A-Za-z0-9]*' bindings/c/core/src/ \
            | sed -E 's/^pub enum (Tst[A-Za-z0-9]+).*/\1/' \
            | sort -u)

[ -n "$ENUMS" ] || { echo "FAIL: no Tst* enums found under bindings/c/core/src/"; exit 1; }

# Extract only the `[export] include = [...]` block — not [export.rename]
# (which would create a hole where a rename rule exists but the include
# entry is missing, silently dropping the type from the header).
INCLUDE_BLOCK=$(awk '
    /^\[/                        { in_block = 0 }
    /^\[export\]/                { next }
    /^include[[:space:]]*=[[:space:]]*\[/ { in_block = 1; next }
    in_block && /^\]/            { in_block = 0; next }
    in_block                     { print }
' "$CBINDGEN")

[ -n "$INCLUDE_BLOCK" ] || { echo "FAIL: could not extract [export] include block from $CBINDGEN"; exit 1; }

MISSING=()
while IFS= read -r enum; do
    # herestring, not printf|grep -q: grep -q exits on first match and can
    # SIGPIPE the printf, which pipefail turns into a false "missing" verdict
    if ! grep -qE "\"${enum}\"" <<< "$INCLUDE_BLOCK"; then
        MISSING+=("$enum")
    fi
done <<< "$ENUMS"

if [ ${#MISSING[@]} -gt 0 ]; then
    echo "FAIL: Tst* enums declared in bindings/c/core/src/ but missing from"
    echo "      cbindgen.toml [export] include allowlist:"
    for e in "${MISSING[@]}"; do echo "  - $e"; done
    echo
    echo "Fix: add the enum name to the include list in $CBINDGEN, and"
    echo "     add a matching entry to [export.rename] if a custom"
    echo "     snake_case typedef name is wanted. Then rebuild tst-c and"
    echo "     copy target/<profile>/include/tstrans.h into the"
    echo "     checked-in bindings/c/include/tstrans.h."
    exit 1
fi

count=$(echo "$ENUMS" | grep -c .)
echo "OK: $count Tst* enums in tst-c-core/src/ are all in $CBINDGEN export allowlist"
