#!/usr/bin/env bash
# Plan #96 Wave D ratchet: keep ABI-version docs and ST 1910 citations
# from regressing.
#
# Four rules:
#
#   1. README.md, docs/, and crate-level rustdoc must not mention a stale
#      ABI minor: any `ABI version 0.N` (bold or plain) with N != the
#      TST_ABI_VERSION_MINOR in bindings/c/core/src/lib.rs.
#
#   2. Bare `ST 1910` (i.e. NOT followed by `.1`) must not appear in
#      crates/ or README.md. The 2026-05-24 audit found 6 sites
#      mis-citing MPEG-TS sync-metadata-AU-cell carriage as "ST 1910";
#      the correct cite is H.222.0 §2.12.4.2 (with ST 1402 as the
#      MISB-side mapping spec). ST 1910.1 itself is a real standard
#      about KLV-in-CMAF-emsg delivery; references with the `.1`
#      version suffix are legitimate (CMAF/HLS deferred-feature
#      context).
#
#   3. tst-c crate-level docs must not say receiver / demux surfaces
#      are "pending" — those surfaces shipped in plan #62 + validate-1.
#
#   4. Pinned current-version doc sites must match the source constants
#      (TST_ABI_VERSION_MINOR + the workspace Cargo.toml version) — the
#      ABI-minor staleness class recurred at 0.17 → 0.18 → 0.19.
#
# Bash 3.2-portable: no `mapfile`, no `declare -A`, no `readarray`
# (see feedback_bash_ratchets_macos_portability.md). Uses
# `while IFS= read -r x; do arr+=("$x"); done < <(...)` pattern.

# --self-test: build a fixture tree with a planted stale minor, expect FAIL;
# correct it, expect OK. Runs the real script recursively via DOC_ABI_ROOT.
if [ "${1:-}" = "--self-test" ]; then
    set -euo pipefail
    SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/docabi-selftest.XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/bindings/c/core/src" "$tmp/docs/languages" "$tmp/docs/reference" "$tmp/crates"
    printf 'pub const TST_ABI_VERSION_MINOR: c_int = 21;\n' > "$tmp/bindings/c/core/src/lib.rs"
    printf '[workspace.package]\nversion = "0.6.0"\n' > "$tmp/Cargo.toml"
    printf '# x\n\nC ABI **0.21** today.\n' > "$tmp/README.md"
    printf '#define TST_ABI_VERSION_MINOR 21\n#define TST_VERSION_MAJOR 0\n#define TST_VERSION_MINOR 6\n#define TST_VERSION_PATCH 0\n' > "$tmp/docs/languages/c.md"
    printf 'ABI minor is **21** today.\n' > "$tmp/docs/reference/binding-authors.md"
    printf '| `tst-c` | ABI version **0.20** (additive). |\n' > "$tmp/docs/reference/compatibility.md"
    if DOC_ABI_ROOT="$tmp" bash "$SELF" >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: planted 'ABI version **0.20**' on a 0.21 tree passed" >&2; exit 1
    fi
    echo "  ok: planted stale bold minor fails"
    printf '| `tst-c` | ABI version 0.20 (additive). |\n' > "$tmp/docs/reference/compatibility.md"
    if DOC_ABI_ROOT="$tmp" bash "$SELF" >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: planted 'ABI version 0.20' on a 0.21 tree passed" >&2; exit 1
    fi
    echo "  ok: planted stale plain minor fails"
    printf '| `tst-c` | ABI version **0.21** (additive). |\n' > "$tmp/docs/reference/compatibility.md"
    if ! DOC_ABI_ROOT="$tmp" bash "$SELF" >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: current minor 0.21 rejected" >&2; exit 1
    fi
    echo "  ok: current minor passes"
    echo "doc-abi-and-st1910-currency self-test: OK"
    exit 0
fi

set -euo pipefail

# DOC_ABI_ROOT lets --self-test point the rail at a fixture tree.
ROOT="${DOC_ABI_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$ROOT"

FAILED=0

# Current ABI minor is derived ONCE, up front — every rule below compares
# against it. (Before 2026-09-14 rule 1 hard-coded `0\.[0-4]` and went blind
# the day the minor reached 0.5; a stale "ABI version 0.20" then sat on a
# 0.21 tree unnoticed — deep review #4, META-07.)
CURRENT_MINOR=$(grep -E '^pub const TST_ABI_VERSION_MINOR' bindings/c/core/src/lib.rs | grep -oE '[0-9]+' | tail -1)
if [ -z "$CURRENT_MINOR" ]; then
    echo "FAIL: cannot read TST_ABI_VERSION_MINOR from bindings/c/core/src/lib.rs"
    exit 1
fi

# -----------------------------------------------------------------------
# Rule 1 — Stale ABI minor wording
# -----------------------------------------------------------------------
#
# Any `ABI version 0.N` (plain or **bold**) in README, docs/, or crate
# rustdoc whose N is not the current minor is stale. Historical wording
# ("added in ABI 0.13") does not use the "ABI version" phrase and is not
# matched.
ABI_HITS=()
while IFS= read -r line; do
    ABI_HITS+=("$line")
done < <(
    grep -rnE 'ABI version \*{0,2}0\.[0-9]+' \
        README.md docs/ crates/ 2>/dev/null \
        | { grep -v '^[^:]*\.lock:' || true; } \
        | { grep -vE "ABI version \*{0,2}0\.${CURRENT_MINOR}([^0-9]|\$)" || true; }
)

if [ ${#ABI_HITS[@]} -gt 0 ]; then
    echo "FAIL: stale 'ABI version' references found (current: 0.${CURRENT_MINOR}):"
    for h in "${ABI_HITS[@]}"; do echo "  $h"; done
    echo "Update each hit to the current value."
    FAILED=1
fi

# -----------------------------------------------------------------------
# Rule 2 — Bare 'ST 1910' (not followed by '.')
# -----------------------------------------------------------------------
#
# Forbid `ST 1910` or `ST1910` NOT followed by a `.` (which would
# indicate a versioned cite like `ST 1910.1`). Hits anywhere in
# crates/ or README.md are presumed mis-cites for MPEG-TS AU cells.
#
# Allowlist: docs/compatibility.md (CMAF section) + docs/deferred-features.md
# (CMAF entry) are scoped out by NOT searching docs/.
ST1910_HITS=()
while IFS= read -r line; do
    # Skip if it's actually ST 1910.<digit>
    if echo "$line" | grep -qE 'ST ?1910\.[0-9]'; then
        continue
    fi
    ST1910_HITS+=("$line")
done < <(
    grep -rnE 'ST ?1910' README.md crates/ 2>/dev/null \
        | { grep -v 'check-doc-abi-and-st1910-currency\.sh:' || true; }
)

if [ ${#ST1910_HITS[@]} -gt 0 ]; then
    echo "FAIL: bare 'ST 1910' (mis-cite for MPEG-TS sync metadata AU cells) found:"
    for h in "${ST1910_HITS[@]}"; do echo "  $h"; done
    echo
    echo "Correct cite for the 5-byte Metadata_AU_cell header is"
    echo "ITU-T H.222.0 §2.12.4.2 (with MISB ST 1402 as the MISB-side"
    echo "mapping spec). ST 1910.1 is a distinct CMAF/HLS standard; use"
    echo "it with the .1 suffix when actually referring to that work."
    FAILED=1
fi

# -----------------------------------------------------------------------
# Rule 3 — tst-c crate docs claiming receiver/demux pending
# -----------------------------------------------------------------------
TSTC_LIB="bindings/c/core/src/lib.rs"
if [ -f "$TSTC_LIB" ]; then
    PENDING_HITS=()
    while IFS= read -r line; do
        PENDING_HITS+=("$line")
    done < <(
        # Crate-level docs are //! lines; only check the first ~30 lines.
        sed -n '1,40p' "$TSTC_LIB" \
            | grep -nE '(receiver[- ]surface|demux event surface).*pending|pending.*(receiver[- ]surface|demux event surface)' \
            || true
    )
    if [ ${#PENDING_HITS[@]} -gt 0 ]; then
        echo "FAIL: $TSTC_LIB crate-level docs still mark receiver/demux surfaces as pending:"
        for h in "${PENDING_HITS[@]}"; do echo "  $TSTC_LIB:$h"; done
        echo
        echo "Receiver-side surfaces (raw, TS-aligned, typed demux event)"
        echo "all shipped — update the crate-level //! docs to current state."
        FAILED=1
    fi
fi

# -----------------------------------------------------------------------
# Rule 4 — pinned current-version doc sites (docs deep edit, 2026-07-12)
# -----------------------------------------------------------------------
#
# The ABI-minor staleness class recurred at 0.17 → 0.18 → 0.19: each time,
# the three sites below quoted a superseded minor. Pin them to
# bindings/c/core/src/lib.rs + the workspace Cargo.toml so they cannot
# drift silently again. Historical mentions ("added in ABI 0.1", the
# version-history list in binding-authors.md) are deliberately NOT
# checked — only these current-value assertion sites are.
PKG_VERSION=$(grep -E '^version' Cargo.toml | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
PKG_MAJOR=$(echo "$PKG_VERSION" | cut -d. -f1)
PKG_MINOR=$(echo "$PKG_VERSION" | cut -d. -f2)
PKG_PATCH=$(echo "$PKG_VERSION" | cut -d. -f3)

# \*{0,2} admits the bolded form (`C ABI **0.19**`) alongside plain `ABI 0.19`.
BAD_README=$(grep -nE 'ABI \*{0,2}0\.[0-9]+' README.md | { grep -vE "ABI \*{0,2}0\.${CURRENT_MINOR}([^0-9]|\$)" || true; })
if [ -n "$BAD_README" ]; then
    echo "FAIL: README.md quotes a non-current ABI minor (current: 0.${CURRENT_MINOR}):"
    echo "$BAD_README" | sed 's/^/  README.md:/'
    FAILED=1
fi

if ! grep -qE "#define TST_ABI_VERSION_MINOR +${CURRENT_MINOR}( |\$)" docs/languages/c.md; then
    echo "FAIL: docs/languages/c.md '#define TST_ABI_VERSION_MINOR' is not ${CURRENT_MINOR}"
    FAILED=1
fi
if ! grep -qE "#define TST_VERSION_MAJOR +${PKG_MAJOR}( |\$)" docs/languages/c.md \
|| ! grep -qE "#define TST_VERSION_MINOR +${PKG_MINOR}( |\$)" docs/languages/c.md \
|| ! grep -qE "#define TST_VERSION_PATCH +${PKG_PATCH}( |\$)" docs/languages/c.md; then
    echo "FAIL: docs/languages/c.md '#define TST_VERSION_*' does not match workspace version ${PKG_VERSION}"
    FAILED=1
fi

if ! grep -q "\*\*${CURRENT_MINOR}\*\* today" docs/reference/binding-authors.md; then
    echo "FAIL: docs/reference/binding-authors.md ABI-minor 'today' line is not ${CURRENT_MINOR}"
    FAILED=1
fi

if [ "$FAILED" -ne 0 ]; then
    exit 1
fi

echo "OK: ABI-version docs are current, ST 1910 mis-cites scrubbed, tst-c crate docs reflect shipped state, pinned version sites match"
