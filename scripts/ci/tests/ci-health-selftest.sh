#!/usr/bin/env bash
# Self-test of scripts/dev/ci-health.sh against a stub `gh` that serves the
# fixtures in ci-health-fixtures/ by request path. Like the real client on a
# 5xx, the stub writes the error body to STDOUT and exits 1; $TMP/fail-<key>
# holds how many calls to that key still fail (with $TMP/partial present they
# first print a page, so a half answer must never reach jq). Cases: all calls succeed; a failing
# first call and a failing later page are retried; a run whose jobs never
# come is listed and the rest aggregated; a runs listing that never comes
# fails the script; no jobs request asks for 100 to a page.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/dev/ci-health.sh"
FX="$ROOT/scripts/ci/tests/ci-health-fixtures"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*"; exit 1; }

mkdir "$TMP/bin"
cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
[ "$1" = api ] || { echo "stub gh: unexpected call" >&2; exit 2; }
path="${@: -1}"
printf '%s\n' "$path" >> "$TMP/gh.log"
case "$path" in
  repos/*/actions/runs\?*) key=runs; file="$FX/runs.json" ;;
  repos/*/actions/runs/*/jobs\?*)
    id="${path#*/actions/runs/}"; id="${id%%/*}"
    key="jobs-$id"; file="$FX/jobs-$id.json" ;;
  *) echo "stub gh: unexpected path $path" >&2; exit 2 ;;
esac
n="$(cat "$TMP/fail-$key" 2>/dev/null || echo 0)"
if [ "$n" -gt 0 ]; then
  echo $((n - 1)) > "$TMP/fail-$key"
  if [ -e "$TMP/partial" ]; then head -n 3 "$file"; fi
  echo "gh: Server Error (HTTP 502)"
  exit 1
fi
cat "$file"
STUB
chmod +x "$TMP/bin/gh"
export TMP FX PATH="$TMP/bin:$PATH" CI_HEALTH_RETRY_SLEEPS="0 0" CI_HEALTH_DAYS=7

# run_case <name>: runs the script, leaves $TMP/<name>.{out,err,sum,rc}
run_case() {
  : > "$TMP/gh.log"
  : > "$TMP/$1.sum"
  set +e
  GITHUB_STEP_SUMMARY="$TMP/$1.sum" bash "$SCRIPT" > "$TMP/$1.out" 2> "$TMP/$1.err"
  echo $? > "$TMP/$1.rc"
  set -e
}
# The snapshot time and the window start differ from run to run.
norm() { sed -E 's/snapshot [0-9T:Z-]+/snapshot NOW/; s/since [0-9-]+/since DAY/; s/^csv: [0-9T:Z-]+/csv: NOW/' "$1"; }
rc_is() { [ "$(cat "$TMP/$1.rc")" = "$2" ] || { echo "rc=$(cat "$TMP/$1.rc") ($1)"; cat "$TMP/$1.err"; fail "$1: expected exit $2"; }; }

# 1: everything answers; output equals the committed expected file.
run_case c1
rc_is c1 0
norm "$TMP/c1.out" > "$TMP/c1.norm"
diff -u "$FX/expected.txt" "$TMP/c1.norm" || fail "case 1: output differs from expected.txt"

# 2: the first jobs call of run 101 fails once with a 502 on stdout.
echo 1 > "$TMP/fail-jobs-101"
run_case c2
rc_is c2 0
norm "$TMP/c2.out" | diff -u "$FX/expected.txt" - || fail "case 2: output differs after one failed call"
[ "$(grep -c 'runs/101/jobs' "$TMP/gh.log")" = 2 ] || fail "case 2: expected two jobs calls for run 101"
rm "$TMP/fail-jobs-101"

# 2b: two failures, then the third attempt answers: three calls in all.
echo 2 > "$TMP/fail-jobs-101"
run_case c2b
rc_is c2b 0
norm "$TMP/c2b.out" | diff -u "$FX/expected.txt" - || fail "case 2b: output differs after two failed calls"
[ "$(grep -c 'runs/101/jobs' "$TMP/gh.log")" = 3 ] || fail "case 2b: expected three jobs calls for run 101"
rm "$TMP/fail-jobs-101"

# 3: run 102's jobs never come: left out and listed, the rest aggregated.
echo 3 > "$TMP/fail-jobs-102"
run_case c3
rc_is c3 0
grep -q 'Runs whose jobs could not be read' "$TMP/c3.sum" || fail "case 3: summary lacks the heading"
[ "$(grep -c 'runs/102/jobs' "$TMP/gh.log")" = 3 ] || fail "case 3: expected three jobs calls for run 102"
grep -q '102' "$TMP/c3.sum" || fail "case 3: summary does not name run 102"
grep -q 'repos/.*/runs/102/jobs.*gh api failed (gh: Server Error (HTTP 502))' "$TMP/c3.err" || { cat "$TMP/c3.err"; fail "case 3: stderr lacks the failure line"; }
grep -q '^csv: .*"test (windows):1;build (linux):1"' "$TMP/c3.out" || { grep '^csv' "$TMP/c3.out"; fail "case 3: the other runs were not aggregated"; }
rm "$TMP/fail-jobs-102"

# 4: the same, with the failure on a later page of the same call.
echo 1 > "$TMP/fail-jobs-101"
touch "$TMP/partial"
run_case c4
rc_is c4 0
norm "$TMP/c4.out" | diff -u "$FX/expected.txt" - || fail "case 4: output differs after a failed later page"
rm "$TMP/fail-jobs-101" "$TMP/partial"

# 5: the runs listing never comes.
echo 3 > "$TMP/fail-runs"
run_case c5
rc_is c5 1
grep -q 'ci-health: repos/.*/actions/runs?.*: gh api failed' "$TMP/c5.err" || { cat "$TMP/c5.err"; fail "case 5: stderr lacks the path"; }
rm "$TMP/fail-runs"

# 6: no jobs request asks for 100 to a page.
run_case c6
! grep 'runs/.*/jobs' "$TMP/gh.log" | grep -q 'per_page=100' || fail "case 6: a jobs request carries per_page=100"
grep -q 'runs/.*/jobs.*per_page=30' "$TMP/gh.log" || fail "case 6: no jobs request carries per_page=30"

echo "PASS: 7 cases"
