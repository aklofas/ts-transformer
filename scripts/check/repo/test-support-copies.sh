#!/usr/bin/env bash
# Files that are published inside more than one package are kept as byte
# copies, because a package cannot reach a file outside itself. FAIL when a
# copy differs from its original:
#   - tests/support/round_trip.rs in tst-udp, tst-tcp and tst-rist, against the
#     tst-pipeline original;
#   - build/host_lib.rs of rist-sys, against the srt-sys one.
# TSC_ROOT overrides the tree to check (default: the repo root).
set -euo pipefail
root="${TSC_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$root"
fail=0
check() { # <original> <copy>
  if [ ! -f "$1" ]; then echo "FAIL: original $1 is missing"; fail=1; return; fi
  if [ ! -f "$2" ]; then echo "FAIL: $2: No such file"; fail=1; return; fi
  if ! cmp -s "$1" "$2"; then echo "FAIL: $2 differs from $1"; fail=1; fi
}
orig=crates/tst-pipeline/tests/support/round_trip.rs
for c in tst-udp tst-tcp tst-rist; do
  check "$orig" "crates/$c/tests/support/round_trip.rs"
done
check crates/srt-sys/build/host_lib.rs crates/rist-sys/build/host_lib.rs
[ "$fail" -eq 0 ] || exit 1
echo "OK: test support copies are identical to their originals"
