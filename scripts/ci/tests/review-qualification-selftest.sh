#!/usr/bin/env bash
# Self-test of scripts/review-qualification.sh: `--list` against the committed
# listing, then the part runner against stub tools and a stub tree. Prints one
# PASS line and exits non-zero on the first failed case.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/review-qualification.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cases=0
fail() { echo "FAIL: $1"; [ ! -f "$TMP/out" ] || sed 's/^/    /' "$TMP/out"; exit 1; }
pass() { cases=$((cases + 1)); }

# --list prints the committed listing and runs nothing.
bash "$SCRIPT" --list > "$TMP/list" 2>&1 || fail "--list failed"
diff -u "$ROOT/scripts/ci/tests/review-qualification.expected" "$TMP/list" || fail "--list differs from the committed listing"
pass

# A stub tree: every script the parts call logs itself and exits per STUB_FAIL.
mkdir -p "$TMP/tree/scripts/lib" "$TMP/tree/scripts/ci" "$TMP/tree/scripts/dev" "$TMP/tree/scripts/check/c" \
         "$TMP/tree/scripts/check/rust" "$TMP/tree/scripts/check/repo" "$TMP/tree/scripts/interop" "$TMP/bin"
echo nightly-2000-01-01 > "$TMP/tree/scripts/lib/public-api-toolchain"
for s in ci/fuzz-shard dev/public-api-diff dev/bench-ir dev/prev-tag check/c/abi-symbols check/rust/api-stability-coverage \
         check/repo/deprecations interop/run-matrix; do
  printf '#!/usr/bin/env bash\necho "%s $*" >> "$TMP/calls.log"\ncase " $STUB_FAIL " in *" %s "*) exit 3 ;; esac\necho v0.0.0\n' "$s" "$(basename "$s")" > "$TMP/tree/scripts/$s.sh"
done
# Stub tools. `cargo +<tc> --version` succeeds; every other cargo call logs itself.
printf '#!/usr/bin/env bash\necho "cargo $*" >> "$TMP/calls.log"\ncase " $STUB_FAIL " in *" cargo "*) exit 4 ;; esac\n' > "$TMP/bin/cargo"
for t in cargo-nextest cargo-fuzz cargo-public-api rg nm valgrind git jq python3 tsp ffprobe; do
  printf '#!/bin/sh\nexit 0\n' > "$TMP/bin/$t"
done
chmod +x "$TMP/bin/"*
export TMP

run() { # <STUB_FAIL> <PATH dirs> [args]: sets rc and $TMP/out
  local sf="$1" path="$2"; shift 2
  : > "$TMP/calls.log"; rm -rf "$TMP/o"
  rc=0
  env STUB_FAIL="$sf" RQ_ROOT="$TMP/tree" PATH="$path" bash "$SCRIPT" --outdir "$TMP/o" "$@" > "$TMP/out" 2>&1 || rc=$?
}
# A PATH of basic utilities only, so a tool the host happens to have (valgrind)
# does not count as installed.
mkdir "$TMP/sys"
for t in bash env head mktemp mkdir awk grep tr cat dirname; do ln -s "$(command -v "$t")" "$TMP/sys/$t"; done
SYS="$TMP/sys"
has() { grep -qF -- "$1" "$TMP/out" || fail "output lacks: $1"; pass; }

run "" "$TMP/bin:$SYS"
[ "$rc" = 0 ] || fail "all parts pass: rc $rc"; pass
for p in unit interop fuzz oracles abi api bench-ir; do has "PART $p: rc=0"; done
grep -q 'public-api-diff nightly-2000-01-01' "$TMP/calls.log" || fail "the pin is not passed to public-api-diff"; pass
grep -q 'fuzz-shard 1 1 10 ' "$TMP/calls.log" || fail "fuzz seconds default is not 10"; pass

run "abi-symbols" "$TMP/bin:$SYS" --only abi,api
[ "$rc" = 1 ] || fail "a failing part: rc $rc, want 1"; pass
has "PART abi: rc=3"; has "PART api: rc=0"
grep -q 'PART unit' "$TMP/out" && fail "--only ran another part"; pass

run "deprecations" "$TMP/bin:$SYS" --only api
[ "$rc" = 1 ] || fail "a later command failing fails the part: rc $rc"; pass
has "PART api: rc=3"

# A missing tool is NOT RUN, and the other parts still run and decide the status.
rm "$TMP/bin/valgrind"
run "" "$TMP/bin:$SYS"
[ "$rc" = 0 ] || fail "one part not run, the rest pass: rc $rc"; pass
has "PART bench-ir: NOT RUN (missing valgrind)"

# Nothing ran: every tool missing.
run "" "$SYS" --only bench-ir
[ "$rc" = 1 ] || fail "nothing ran: rc $rc, want 1"; pass
has "PART bench-ir: NOT RUN (missing valgrind)"; has "no part ran"

run "" "$TMP/bin:$SYS" --only nope
[ "$rc" = 2 ] || fail "an unknown part: rc $rc, want 2"; pass

echo "PASS: $cases cases"
