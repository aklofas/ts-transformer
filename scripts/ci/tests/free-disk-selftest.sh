#!/usr/bin/env bash
# Self-test of scripts/ci/free-disk.sh against stub `df`, `sudo` and `timeout`
# on PATH. The stub df answers `df -B1 --output=avail /` from $TMP/avail; the
# stub sudo logs its arguments and runs them; the stub timeout logs its
# arguments, drops the bound and runs the rest (exit 124 for a path named in
# $TMP/stuck, without running it). The toolset paths are directories under
# $TMP.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/free-disk.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir "$TMP/bin"
cat > "$TMP/bin/df" <<'STUB'
#!/usr/bin/env bash
case "$*" in
  "-h /") printf 'Filesystem Size Used Avail Use%% Mounted on\nstub 145G 1G 1G 1%% /\n' ;;
  "-B1 --output=avail /") printf '    Avail\n %s\n' "$(cat "$TMP/avail")" ;;
  *) echo "stub df: unexpected call: $*" >&2; exit 2 ;;
esac
STUB
cat > "$TMP/bin/sudo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/sudo.log"
exec "$@"
STUB
cat > "$TMP/bin/timeout" <<'STUB'
#!/usr/bin/env bash
printf 'timeout %s\n' "$*" >> "$TMP/timeout.log"
shift
if [ -f "$TMP/stuck" ] && [ "${*: -1}" = "$(cat "$TMP/stuck")" ]; then exit 124; fi
exec "$@"
STUB
chmod +x "$TMP/bin/df" "$TMP/bin/sudo" "$TMP/bin/timeout"
export TMP PATH="$TMP/bin:$PATH"
export FREE_DISK_PATHS="$TMP/tools/dotnet $TMP/tools/missing $TMP/tools/android"
export GITHUB_OUTPUT="$TMP/output" GITHUB_STEP_SUMMARY="$TMP/summary"

fail() { echo "FAIL: $*"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; echo "--- sudo.log"; cat "$TMP/sudo.log"; exit 1; }

# setup <avail bytes>: fresh logs and toolsets, the space df reports.
setup() {
  rm -rf "$TMP/tools" "$TMP/stuck"
  mkdir -p "$TMP/tools/dotnet/sdk" "$TMP/tools/android/ndk"
  : > "$TMP/sudo.log"; : > "$TMP/timeout.log"; : > "$TMP/out"; : > "$TMP/output"; : > "$TMP/summary"
  printf '%s\n' "$1" > "$TMP/avail"
}
# run <mode> [bytes]: appends the output to $TMP/out, sets RC.
run() { set +e; bash "$SCRIPT" "$@" >> "$TMP/out" 2>&1; RC=$?; set -e; }
summary_lines() { wc -l < "$TMP/summary" | tr -d ' '; }

# 1: 88 GiB free: the removal is skipped, nothing is touched
setup 94489280512
run before
[ "$RC" = 0 ] || fail "case 1 before rc=$RC"
[ "$(cat "$TMP/output")" = "avail_before=94489280512" ] || fail "case 1 output: $(cat "$TMP/output")"
grep -q '^Filesystem' "$TMP/out" || fail "case 1 before did not print df -h /"
run clean 94489280512
[ "$RC" = 0 ] || fail "case 1 clean rc=$RC"
grep -qx 'free-disk: 88.0 GiB free, removal skipped (runs below 30 GiB)' "$TMP/out" || fail "case 1 skip line"
! [ -s "$TMP/sudo.log" ] || fail "case 1 removed something"
[ -d "$TMP/tools/dotnet" ] || fail "case 1 dotnet is gone"
[ -d "$TMP/tools/android" ] || fail "case 1 android is gone"
: > "$TMP/out"
run after 94489280512
[ "$RC" = 0 ] || fail "case 1 after rc=$RC"
grep -q '^Filesystem' "$TMP/out" || fail "case 1 after did not print df -h /"
[ "$(summary_lines)" = 1 ] || fail "case 1 summary has $(summary_lines) lines"
[ "$(cat "$TMP/summary")" = 'free-disk: 88.0 GiB free, removal skipped (runs below 30 GiB)' ] || fail "case 1 summary: $(cat "$TMP/summary")"

# 2: 5 GiB free: one bounded removal per existing path, then the after line
setup 5368709120
run clean 5368709120
[ "$RC" = 0 ] || fail "case 2 clean rc=$RC"
[ "$(cat "$TMP/sudo.log")" = "timeout 300 rm -rf $TMP/tools/dotnet
timeout 300 rm -rf $TMP/tools/android" ] || fail "case 2 removals"
! [ -e "$TMP/tools/dotnet" ] || fail "case 2 dotnet is left"
! [ -e "$TMP/tools/android" ] || fail "case 2 android is left"
grep -qE "^free-disk: $TMP/tools/dotnet removed in [0-9]+ s$" "$TMP/out" || fail "case 2 dotnet line"
grep -qE "^free-disk: $TMP/tools/android removed in [0-9]+ s$" "$TMP/out" || fail "case 2 android line"
! grep -q 'missing' "$TMP/out" || fail "case 2 a missing path was named"
! grep -q 'skipped' "$TMP/out" || fail "case 2 printed the skip line"
printf '16106127360\n' > "$TMP/avail"
run after 5368709120
[ "$RC" = 0 ] || fail "case 2 after rc=$RC"
[ "$(summary_lines)" = 1 ] || fail "case 2 summary has $(summary_lines) lines"
[ "$(cat "$TMP/summary")" = 'free-disk: 5.0 GiB → 15.0 GiB (+10.0 GiB)' ] || fail "case 2 summary: $(cat "$TMP/summary")"
grep -qx 'free-disk: 5.0 GiB → 15.0 GiB (+10.0 GiB)' "$TMP/out" || fail "case 2 after line"

# 3: no usable reading (empty or not a number): the removal runs
for v in '' abc; do
  setup 16106127360
  run clean "$v"
  [ "$RC" = 0 ] || fail "case 3 clean rc=$RC for '$v'"
  [ "$(grep -c 'rm -rf' "$TMP/sudo.log")" = 2 ] || fail "case 3 removals for '$v'"
  run after "$v"
  [ "$RC" = 0 ] || fail "case 3 after rc=$RC for '$v'"
  [ "$(summary_lines)" = 1 ] || fail "case 3 summary has $(summary_lines) lines for '$v'"
  grep -q '^free-disk: 0.0 GiB → 15.0 GiB' "$TMP/summary" || fail "case 3 summary for '$v': $(cat "$TMP/summary")"
done

# 4: the threshold is 30 GiB: exactly 30 skips, one byte less removes
setup 32212254720
run clean 32212254720
! [ -s "$TMP/sudo.log" ] || fail "case 4 removed at 30 GiB"
setup 32212254719
run clean 32212254719
[ "$(grep -c 'rm -rf' "$TMP/sudo.log")" = 2 ] || fail "case 4 kept the toolsets one byte under 30 GiB"

# 5: a removal cut by its bound is named and the next one still runs
setup 5368709120
printf '%s' "$TMP/tools/dotnet" > "$TMP/stuck"
run clean 5368709120
[ "$RC" = 0 ] || fail "case 5 rc=$RC"
grep -qx "free-disk: rm $TMP/tools/dotnet exited 124 (bounded at 300 s)" "$TMP/out" || fail "case 5 the cut removal is not named"
! [ -e "$TMP/tools/android" ] || fail "case 5 the next removal did not run"

# 6: an unknown mode is refused
setup 5368709120
run sideways
[ "$RC" = 2 ] || fail "case 6 rc=$RC"

echo "PASS: 6 cases"
