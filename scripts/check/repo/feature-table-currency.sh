#!/usr/bin/env bash
# Deep-review-#4 rail (META-07, 2026-09-14): every `| crate | feature |` row
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
# Self-test: bash scripts/check/repo/feature-table-currency.sh --self-test
# Overridable (the self-test points these at fixtures):
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

self_test() {
    local tmp; tmp="$(mktemp -d "${TMPDIR:-/tmp}/ftc-selftest.XXXXXX")"
    trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/crates/foo"
    printf '[package]\nname = "foo"\n\n[features]\ndefault = ["alpha"]\nalpha = []\nbeta = ["dep:x"]\n\n[dependencies]\nx = "1"\n' > "$tmp/crates/foo/Cargo.toml"
    run() { FTC_RUST_MD="$tmp/rust.md" FTC_CRATES_DIR="$tmp/crates" bash "$0"; }
    expect() { # <pass|fail> <label>
        local want="$1" label="$2" rc
        if run >/dev/null 2>&1; then rc=pass; else rc=fail; fi
        if [ "$rc" = "$want" ]; then echo "  ok: $label (expected $want)"; else
            echo "  SELF-TEST FAIL: $label expected $want got $rc" >&2; return 1; fi
    }
    printf '| Crate | Feature | Default | Effect |\n| --- | --- | --- | --- |\n| `foo` | `alpha` | on | a |\n| `foo` (published as `bar-foo`) | `beta` | off | b |\n' > "$tmp/rust.md"
    expect pass "every row exists" || return 1
    printf '| `foo` | `alpha` | on | a |\n| `foo` | `zzz` | on | planted |\n' > "$tmp/rust.md"
    expect fail "planted missing feature" || return 1
    printf '| `nope` | `alpha` | on | a |\n' > "$tmp/rust.md"
    expect fail "planted missing crate" || return 1
    printf '| Crate | Feature |\n| --- | --- |\n' > "$tmp/rust.md"
    expect fail "table with no rows" || return 1
    echo "feature-table-currency self-test: OK"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
else
    check
fi
