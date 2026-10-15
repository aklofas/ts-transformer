#!/usr/bin/env bash
# dependency-lattice: for every (package, flags) row of
# scripts/ratchets/dependency-lattice.tsv, the normal dependency tree
# (`cargo tree -e normal`, no dev-dependencies) must contain every crate in
# the third column and none in the fourth. A feature that stops forwarding
# (a C `tls` build without rustls) or a heavy dependency leaking into a
# plane that promised to be free of it (tokio in the client-only RTSP plane)
# fails here; `--all-features` builds hide both by unifying features.
#
# Overridable: LATTICE_TSV (the table; the ratchet self-test plants one).
# Bash 3.2-portable. Does not read stdin.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
TSV="${LATTICE_TSV:-scripts/ratchets/dependency-lattice.tsv}"
fail=0
rows=0
while IFS="$(printf '\t')" read -r pkg flags want deny; do
    case "$pkg" in ''|\#*) continue ;; esac
    rows=$((rows + 1))
    [ "$flags" = "(default)" ] && flags=""
    # shellcheck disable=SC2086 # flags is a word list on purpose
    if ! tree="$(cargo tree -p "$pkg" $flags -e normal --prefix none -f '{p}' 2>/dev/null | awk '{print $1}' | sort -u)"; then
        echo "FAIL: cargo tree -p $pkg $flags did not resolve"
        fail=1
        continue
    fi
    for c in $(printf '%s' "$want" | tr ',' ' '); do
        [ "$c" = "-" ] && continue
        printf '%s\n' "$tree" | grep -qx "$c" || { echo "FAIL: $pkg [$flags] lacks $c"; fail=1; }
    done
    for c in $(printf '%s' "$deny" | tr ',' ' '); do
        [ "$c" = "-" ] && continue
        if printf '%s\n' "$tree" | grep -qx "$c"; then
            echo "FAIL: $pkg [$flags] contains $c"
            fail=1
        fi
    done
done < "$TSV"
[ "$rows" -gt 0 ] || { echo "FAIL: $TSV has no rows"; exit 1; }
[ "$fail" -eq 0 ] && echo "dependency-lattice: OK ($rows planes)"
exit "$fail"
