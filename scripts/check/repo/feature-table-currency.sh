#!/usr/bin/env bash
# Every `| crate | feature |` row
# in docs/languages/rust.md must name a feature that exists in that crate's
# Cargo.toml [features] table. rust.md documented a default-on tst-srt `log`
# feature that never existed; nothing could see it.
#
# Row shape (first two cells): `| `tst-srt` | `mbedtls` | on | …`. The first
# cell may carry trailing prose (`srt-sys` (published as `tstrans-srt-sys`));
# the crate is the first backticked token, the feature the second cell's
# backticked token. Crate dir = crates/<name> (DIRECTORY names, not package
# names — `srt-sys` lives at crates/srt-sys although it publishes as
# tstrans-srt-sys).
#
# Run:       bash scripts/check/repo/feature-table-currency.sh
# Overridable (scripts/ratchets/tests/self_test.sh points these at fixtures):
#   FTC_RUST_MD     the markdown file to scan
#   FTC_CRATES_DIR  the directory holding <crate>/Cargo.toml
#
# Bash 3.2-portable (macOS is a gating platform): no mapfile/readarray,
# no declare -A, no grep -P; mktemp always gets a template. Does not read
# stdin.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FTC_RUST_MD="${FTC_RUST_MD:-$ROOT/docs/languages/rust.md}"
FTC_CRATES_DIR="${FTC_CRATES_DIR:-$ROOT/crates}"

ROW_RE='^\| *`[^`]+`[^|]*\| *`[^`]+` *\|'

check() {
    local failed=0 checked=0 line crate feature toml
    while IFS= read -r line; do
        crate=$(printf '%s\n' "$line" | sed -E 's/^\| *`([^`]+)`.*/\1/')
        feature=$(printf '%s\n' "$line" | sed -E 's/^\|[^|]*\| *`([^`]+)`.*/\1/')
        toml="$FTC_CRATES_DIR/$crate/Cargo.toml"
        checked=$((checked + 1))
        if [ ! -f "$toml" ]; then
            echo "FAIL: $FTC_RUST_MD: row names crate \`$crate\` but $toml does not exist"
            failed=1
            continue
        fi
        # The [features] table: from its header up to the next [section].
        if ! sed -n '/^\[features\]/,/^\[/p' "$toml" | grep -qE "^${feature} *="; then
            echo "FAIL: $FTC_RUST_MD: feature \`$feature\` is not in $toml [features]"
            failed=1
        fi
    done < <(grep -E "$ROW_RE" "$FTC_RUST_MD" || true)
    if [ "$checked" -eq 0 ]; then
        echo "FAIL: no feature-table rows found in $FTC_RUST_MD (table renamed or reshaped?)"
        return 1
    fi
    if [ "$failed" -ne 0 ]; then
        return 1
    fi
    echo "OK: $checked feature-table rows in $(basename "$FTC_RUST_MD") exist in their crate's [features]"
}

check
