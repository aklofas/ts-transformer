#!/usr/bin/env bash
# Self-test for release-vm-soak.sh's ci-green and launch handshake: a fake `gh`
# answers from fixture run pages and a stub soak.sh stands in for the real one.
# The fixture checkout is a swept tree (workspace 0.2.0 above its tag v0.1.0)
# until the last cases, which launch on an unswept one.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../release-vm-soak.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0
unset TSTRANS_NOT_A_RELEASE

mkdir -p "$tmp/bin" "$tmp/fix" "$tmp/repo/scripts/interop"
cat > "$tmp/bin/gh" <<'GH'
#!/usr/bin/env bash
case "$1 $2" in
  "repo view") echo o/r ;;
  "api repos/o/r/actions/workflows/"*)
    wf=${2#repos/o/r/actions/workflows/}; wf=${wf%%/*}
    cat "$FIX/$wf.json" ;;
  *) echo "fake gh: unexpected: $*" >&2; exit 1 ;;
esac
GH
chmod +x "$tmp/bin/gh"

printf '#!/usr/bin/env bash\necho stub\nexit 7\n' > "$tmp/repo/scripts/interop/soak.sh"
git -C "$tmp/repo" init -q
commit_version() {
  printf '[workspace.package]\nversion = "%s"\n' "$1" > "$tmp/repo/Cargo.toml"
  git -C "$tmp/repo" add -A
  git -C "$tmp/repo" -c user.name=t -c user.email=t@example.com -c commit.gpgsign=false commit -q -m "$1"
}
commit_version 0.1.0
git -C "$tmp/repo" tag v0.1.0
commit_version 0.2.0
S=$(git -C "$tmp/repo" rev-parse HEAD)
OTHER=$(printf '%040d' 1)

# run <name>: one fixture run object.
run() { # id event status conclusion created_at
  printf '{"id":%s,"event":"%s","status":"%s","conclusion":%s,"created_at":"%s","html_url":"u","run_attempt":1,"head_sha":"%s"}' \
    "$1" "$2" "$3" "$4" "$5" "$S"
}
green() { printf '{"workflow_runs":[%s]}' "$(run 1 push completed '"success"' 2026-01-01T00:00:00Z)" > "$FIX/$1.json"; }

export FIX=$tmp/fix
export PATH=$tmp/bin:$PATH
export TSTRANS_REPO_DIR=$tmp/repo

check() { # name rc want_rc output-pattern
  local name=$1 rc=$2 want=$3 pat=${4:-}
  if [[ "$rc" -ne "$want" ]]; then
    echo "FAIL: $name: exit $rc, wanted $want"; sed 's/^/    /' "$tmp/out"; fail=1; return
  fi
  if [[ -n "$pat" ]] && ! grep -q -- "$pat" "$tmp/out"; then
    echo "FAIL: $name: output lacks '$pat'"; sed 's/^/    /' "$tmp/out"; fail=1; return
  fi
  echo "ok: $name"
}

green jvm-jar.yml; green python-wheels.yml

# 1: an older push success does not cover a newer failed push run
printf '{"workflow_runs":[%s,%s]}' \
  "$(run 10 push completed '"success"' 2026-01-01T00:00:00Z)" \
  "$(run 11 push completed '"failure"' 2026-01-02T00:00:00Z)" > "$FIX/ci.yml.json"
rm -f "$tmp/m"
timeout 30 bash "$SCRIPT" ci-green "$S" local --marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "ci-green: an older push success is not enough while the newest push run failed" "$rc" 2 'run 11'
[[ ! -e "$tmp/m" ]] || { echo "FAIL: ci-green wrote a marker for a red run"; fail=1; }

# 2: a newer pull_request run is ignored; the marker carries the sha
printf '{"workflow_runs":[%s,%s]}' \
  "$(run 10 push completed '"success"' 2026-01-01T00:00:00Z)" \
  "$(run 12 pull_request completed '"failure"' 2026-01-02T00:00:00Z)" > "$FIX/ci.yml.json"
rm -f "$tmp/m"
timeout 30 bash "$SCRIPT" ci-green "$S" local --marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "ci-green: a newer pull_request run is ignored" "$rc" 0
[[ "$(cat "$tmp/m" 2>/dev/null)" == "$S" ]] || { echo "FAIL: the marker does not hold exactly the sha"; fail=1; }

# 3-5: launch reads the marker
echo "$OTHER" > "$tmp/m"
timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out1" --wait-marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "launch: a marker written for another sha is refused" "$rc" 2 'written for'

: > "$tmp/m"
timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out2" --wait-marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "launch: an empty marker is refused" "$rc" 2 'carries no sha'

printf '%s\n' "$S" > "$tmp/m"
timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out3" --wait-marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "launch: a marker for this sha reaches the pre-flight" "$rc" 7 'stub'

# 6-8: launch refuses a tree whose version sweep has not happened, before it
# waits for the marker (a missing marker would otherwise hold it for the
# whole timeout) and before the pre-flight.
git -C "$tmp/repo" tag v0.2.0 "$S~1"
rm -f "$tmp/m"
timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out4" --wait-marker "$tmp/m" > "$tmp/out" 2>&1; rc=$?
check "launch: a tree at the newest release tag's version is refused" "$rc" 2 'newest release tag v0.2.0'
grep -q stub "$tmp/out" && { echo "FAIL: the unswept launch reached the pre-flight"; fail=1; }

TSTRANS_NOT_A_RELEASE=1 timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out5" > "$tmp/out" 2>&1; rc=$?
check "launch: TSTRANS_NOT_A_RELEASE=1 lets a run that is not release evidence start" "$rc" 7 'skipped (TSTRANS_NOT_A_RELEASE=1)'

git -C "$tmp/repo" tag -d v0.2.0 > /dev/null; git -C "$tmp/repo" tag -d v0.1.0 > /dev/null
timeout 30 bash "$SCRIPT" launch "$S" --outdir "$tmp/out6" > "$tmp/out" 2>&1; rc=$?
check "launch: a checkout with no release tag is refused" "$rc" 2 'git fetch --tags'

exit "$fail"
