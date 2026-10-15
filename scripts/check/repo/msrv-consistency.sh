#!/usr/bin/env bash
# rust-toolchain.toml's channel is the one Rust toolchain the workspace builds
# and tests with.
#   (a) the sites that STATE it (the workspace rust-version and the
#       user-facing doc claims) equal the channel and still exist;
#   (b) nothing else writes it down: workflows read it through
#       .github/actions/rust-setup, the embedded gates read rust-toolchain.toml,
#       member crates inherit rust-version.workspace.
# Negative cases: scripts/ratchets/tests/self_test.sh.
# Overridable: MSRV_ROOT — the tree root every path resolves against.
# Bash 3.2-portable: no mapfile/declare -A, no sed -i. Does not read stdin.
set -euo pipefail
ROOT="${MSRV_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"

stated() { # <path> TAB <ERE with @V@ for the version> TAB <min occurrences>
  printf '%s\t%s\t%s\n' \
    'Cargo.toml'                      '^rust-version[[:space:]]*=[[:space:]]*"@V@"' 1 \
    'README.md'                       'MSRV(: |-)@V@'                               2 \
    'docs/languages/rust.md'          '(Rust \*\*@V@\*\*|auto-uses @V@ via rustup)'   2 \
    'docs/reference/compatibility.md' 'MSRV \*\*@V@\*\*'                            1 \
    'docs/start/quickstart.md'        '(Rust @V@\+|pins to @V@ )'                   2
}
# A toolchain version written where the channel must be read instead.
FORBIDDEN='(RUSTUP_TOOLCHAIN|toolchain|rust-version):[[:space:]]*"?[0-9]+\.[0-9]+|--toolchain [0-9]+\.[0-9]+|toolchain install [0-9]+\.[0-9]+|cargo \+[0-9]+\.[0-9]+|Install Rust [0-9]+\.[0-9]+'

pinned="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+(\.[0-9]+)?)".*/\1/p' "$ROOT/rust-toolchain.toml" | head -1)"
[ -n "$pinned" ] || { echo "FAIL: no numeric channel in rust-toolchain.toml (a 'stable'/'nightly' channel is not an MSRV pin)"; exit 1; }
pin_re="$(printf '%s' "$pinned" | sed 's/\./\\./g')"; gen_re='[0-9]+\.[0-9]+(\.[0-9]+)?'
rc=0; n=0
while IFS="$(printf '\t')" read -r path re min; do
  f="$ROOT/$path"
  [ -f "$f" ] || { echo "FAIL: stated site missing: $path"; rc=1; continue; }
  got="$( (grep -oE "${re//@V@/$pin_re}" "$f" || true) | wc -l | tr -d ' ')"
  all="$( (grep -oE "${re//@V@/$gen_re}" "$f" || true) | wc -l | tr -d ' ')"
  [ "$all" -eq "$got" ] || { echo "FAIL: $path states a version other than $pinned"; rc=1; }
  [ "$got" -ge "$min" ] || { echo "FAIL: $path: expected >= $min statement(s) of $pinned, found $got"; rc=1; }
  n=$((n + got))
done < <(stated)
for d in .github scripts embedded/scripts embedded/freertos-srt/substrate; do
  [ -d "$ROOT/$d" ] || continue
  hits="$(grep -rnE "$FORBIDDEN" "$ROOT/$d" || true)"
  [ -z "$hits" ] || { echo "FAIL: toolchain literal(s) under $d (read rust-toolchain.toml instead):"; printf '%s\n' "$hits" | sed 's/^/  /'; rc=1; }
done
for d in crates bindings examples; do
  [ -d "$ROOT/$d" ] || continue
  hits="$(find "$ROOT/$d" -name Cargo.toml -not -path '*/vendor/*' -not -path '*/target/*' -not -path '*/fuzz/*' -exec grep -lE '^rust-version[[:space:]]*=[[:space:]]*"' {} + || true)"
  [ -z "$hits" ] || { echo "FAIL: member manifest(s) with their own rust-version (use rust-version.workspace = true): $hits"; rc=1; }
done
[ "$rc" -eq 0 ] && echo "msrv-consistency: OK — $n stated sites equal rust-toolchain.toml channel $pinned; no other toolchain literal"
exit "$rc"
