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
  printf '#!/usr/bin/env bash\necho "%s $*" >> "$TMP/calls.log"\ncase " $STUB_FAIL " in *" %s "*) exit 3 ;; esac\n[ -z "${STUB_SKIP:-}" ] || case " $STUB_SKIP " in *" %s "*) echo "SKIP: %s skipped on purpose"; exit 0 ;; esac\n[ -z "${STUB_SKIPNAMED:-}" ] || case " $STUB_SKIPNAMED " in *" %s "*) echo "%s: SKIP (linux-only)"; exit 0 ;; esac\n[ -z "${STUB_NOISE:-}" ] || echo "cells: 3 SKIPPED_TOOL_MISSING; skip this line"\necho v0.0.0\n' "$s" "$(basename "$s")" "$(basename "$s")" "$(basename "$s")" "$(basename "$s")" "$(basename "$s")" > "$TMP/tree/scripts/$s.sh"
done
# run-matrix also writes the inventory its caller reads: STUB_CELLS cells.
cat >> "$TMP/tree/scripts/interop/run-matrix.sh" <<'RM'
out=""; while [ $# -gt 0 ]; do [ "$1" = --outdir ] && out="$2"; shift; done
mkdir -p "$out"; n="${STUB_CELLS:-3}"
jq -n --argjson n "$n" '{cells: [range($n) | {id: "c\(.)"}]}' > "$out/inventory.json"
RM
# Stub tools. `cargo +<tc> --version` succeeds; every other cargo call logs itself.
printf '#!/usr/bin/env bash\necho "cargo $*" >> "$TMP/calls.log"\ncase "$1" in +*) [ "$1" != "+${STUB_NO_TC:-none}" ]; exit ;; esac\ncase " $STUB_FAIL " in *" cargo "*) exit 4 ;; esac\n' > "$TMP/bin/cargo"
ln -s "$(command -v jq)" "$TMP/bin/jq"
for t in cargo-nextest cargo-fuzz cargo-public-api rg nm valgrind git python3 tsp tsanalyze ffprobe; do
  printf '#!/bin/sh\nexit 0\n' > "$TMP/bin/$t"
done
chmod +x "$TMP/bin/"* 2>/dev/null || true
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
for t in bash env head mktemp mkdir awk grep tr cat dirname uname rm; do ln -s "$(command -v "$t")" "$TMP/sys/$t"; done
SYS="$TMP/sys"
last_is() { [ "$(tail -n1 "$TMP/out")" = "$1" ] || fail "the last line is not: $1"; pass; }
has() { grep -qF -- "$1" "$TMP/out" || fail "output lacks: $1"; pass; }

run "" "$TMP/bin:$SYS"
[ "$rc" = 0 ] || fail "all parts pass: rc $rc"; pass
for p in unit interop fuzz oracles abi api bench-ir; do has "PART $p: rc=0"; done
hasnt() { ! grep -qF -- "$1" "$TMP/out" || fail "output has: $1"; pass; }
hasnt "skipped checks"
last_is "REVIEW QUALIFICATION: 7 passed, 0 partly run, 0 not run, 0 failed"
[ ! -e "$TMP/o/.cmd.out" ] || fail "a scratch file was left in the outdir"; pass
grep -q 'public-api-diff nightly-2000-01-01' "$TMP/calls.log" || fail "the pin is not passed to public-api-diff"; pass
grep -q 'fuzz-shard 1 1 10 ' "$TMP/calls.log" || fail "fuzz seconds default is not 10"; pass

run "abi-symbols" "$TMP/bin:$SYS" --only abi,api
last_is "REVIEW QUALIFICATION: 1 passed, 0 partly run, 0 not run, 1 failed"
run "abi-symbols" "$TMP/bin:$SYS" --only abi,api
[ "$rc" = 1 ] || fail "a failing part: rc $rc, want 1"; pass
has "PART abi: rc=3"; has "PART api: rc=0"
grep -q 'PART unit' "$TMP/out" && fail "--only ran another part"; pass

run "deprecations" "$TMP/bin:$SYS" --only api
[ "$rc" = 1 ] || fail "a later command failing fails the part: rc $rc"; pass
has "PART api: rc=3"

# An earlier command failing stops the part: a later one that would pass is not run.
run "public-api-diff" "$TMP/bin:$SYS" --only api
[ "$rc" = 1 ] || fail "an earlier command failing fails the part: rc $rc"; pass
has "PART api: rc=3"
! grep -q 'deprecations' "$TMP/calls.log" || fail "a command ran after the part failed"; pass

# The toolchain requirement is checked, with the pin substituted in.
STUB_NO_TC=nightly-2000-01-01 run "" "$TMP/bin:$SYS" --only api
has "PART api: NOT RUN (missing toolchain:nightly-2000-01-01)"
[ "$rc" = 1 ] || fail "only a missing toolchain: nothing ran, rc $rc"; pass

# run-matrix that selected no cell fails the parts that read its inventory.
STUB_CELLS=0 run "" "$TMP/bin:$SYS" --only interop,oracles
[ "$rc" = 1 ] || fail "no cell selected: rc $rc"; pass
has "PART interop: rc=1"; has "PART oracles: rc=1"
grep -q "selected no cell" "$TMP/o/interop.log" || fail "the part log does not say no cell was selected"; pass

# A command that printed SKIP is never a plain pass.
STUB_SKIP="abi-symbols" run "" "$TMP/bin:$SYS" --only abi
[ "$rc" = 0 ] || fail "a partly run part is not a failure: rc $rc"; pass
has "PART abi: rc=0, partly run (abi-symbols skipped on purpose)"; last_is "REVIEW QUALIFICATION: 0 passed, 1 partly run, 0 not run, 0 failed; partly run: abi (abi-symbols skipped on purpose)"
STUB_SKIP="public-api-diff api-stability-coverage deprecations" run "" "$TMP/bin:$SYS" --only api
has "PART api: NOT RUN (deprecations skipped on purpose)"
[ "$rc" = 1 ] || fail "an all-skipped part is not a part that ran: rc $rc"; pass
last_is "REVIEW QUALIFICATION: 0 passed, 0 partly run, 1 not run, 0 failed; not run: api (deprecations skipped on purpose)"
STUB_SKIPNAMED="api-stability-coverage" run "" "$TMP/bin:$SYS" --only api
has "PART api: rc=0, partly run (api-stability-coverage skipped (linux-only))"
last_is "REVIEW QUALIFICATION: 0 passed, 1 partly run, 0 not run, 0 failed; partly run: api (api-stability-coverage skipped (linux-only))"
STUB_SKIPNAMED="public-api-diff api-stability-coverage deprecations" run "" "$TMP/bin:$SYS" --only api
has "PART api: NOT RUN (deprecations skipped (linux-only))"
[ "$rc" = 1 ] || fail "named skips of every command: nothing ran, rc $rc"; pass
STUB_SKIPNAMED="abi-symbols" run "" "$TMP/bin:$SYS" --only abi
has "PART abi: rc=0, partly run (abi-symbols skipped (linux-only))"
STUB_NOISE=1 run "" "$TMP/bin:$SYS" --only api
has "PART api: rc=0"; hasnt "partly run ("
STUB_SKIP="deprecations" run "" "$TMP/bin:$SYS" --only api
has "PART api: rc=0, partly run (deprecations skipped on purpose)"

# A missing previous tag and a foreign platform are NOT RUN.
printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/tree/scripts/dev/prev-tag.sh"
run "" "$TMP/bin:$SYS" --only bench-ir
has "PART bench-ir: NOT RUN (no previous release tag; git fetch --tags)"
mkdir "$TMP/bin2"; printf '#!/bin/sh\necho Plan9\n' > "$TMP/bin2/uname"; chmod +x "$TMP/bin2/uname"
run "" "$TMP/bin2:$TMP/bin:$SYS" --only abi
has "PART abi: NOT RUN (needs Linux)"

# An outdir with a space reaches the commands whole.
run "" "$TMP/bin:$SYS" --only fuzz --outdir "$TMP/a b"
grep -qF "$TMP/a b/fuzz" "$TMP/calls.log" || fail "an outdir with a space was split"; pass

# A missing tool is NOT RUN, and the other parts still run and decide the status.
rm "$TMP/bin/valgrind"
run "" "$TMP/bin:$SYS"
[ "$rc" = 0 ] || fail "one part not run, the rest pass: rc $rc"; pass
has "PART bench-ir: NOT RUN (missing valgrind)"
last_is "REVIEW QUALIFICATION: 6 passed, 0 partly run, 1 not run, 0 failed; not run: bench-ir (missing valgrind)"

# Nothing ran: every tool missing.
run "" "$SYS" --only bench-ir
[ "$rc" = 1 ] || fail "nothing ran: rc $rc, want 1"; pass
has "PART bench-ir: NOT RUN (missing cargo)"; has "no part ran"
last_is "REVIEW QUALIFICATION: 0 passed, 0 partly run, 1 not run, 0 failed; not run: bench-ir (missing cargo)"

run "" "$TMP/bin:$SYS" --only nope
[ "$rc" = 2 ] || fail "an unknown part: rc $rc, want 2"; pass

echo "PASS: $cases cases"
