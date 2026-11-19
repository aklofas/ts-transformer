#!/usr/bin/env bash
# Self-test of scripts/ci/nightly-dedup.sh against a stub `gh` that answers the
# runs listing with $TMP/runs.json. Only a workflow_dispatch run of ci.yml by
# the bot on the given sha counts; a pending run is waited on, a completed one
# skips the night.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/nightly-dedup.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*"; exit 1; }

mkdir "$TMP/bin"
cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
[ "$1" = api ] || { echo "stub gh: unexpected call" >&2; exit 2; }
printf '%s\n' "$2" >> "$TMP/gh.log"
cat "$TMP/runs.json"
STUB
chmod +x "$TMP/bin/gh"
export TMP PATH="$TMP/bin:$PATH" GITHUB_REPOSITORY=example/repo

TIP=aaaa111111111111111111111111111111111111
OTHER=bbbb222222222222222222222222222222222222
# run <id> <status> <sha> <actor> <created>
run() {
  printf '{"id":%s,"status":"%s","conclusion":%s,"head_sha":"%s","event":"workflow_dispatch","created_at":"%s","actor":{"login":"%s"}}' \
    "$1" "$2" "$([ "$2" = completed ] && echo '"success"' || echo null)" "$3" "$5" "$4"
}
BOT='github-actions[bot]'
expect() {
  local name="$1" want="$2" got
  got="$(bash "$SCRIPT" "$TIP")" || fail "$name: the script failed"
  [ "$got" = "$want" ] || fail "$name: wanted '$want', got '$got'"
}

echo '{"workflow_runs":[]}' > "$TMP/runs.json"
expect none dispatch
echo "{\"workflow_runs\":[$(run 11 completed $TIP "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect completed "skip 11"
echo "{\"workflow_runs\":[$(run 12 in_progress $TIP "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect in_progress "wait 12"
echo "{\"workflow_runs\":[$(run 13 queued $TIP "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect queued "wait 13"
echo "{\"workflow_runs\":[$(run 14 completed $OTHER "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect "completed on another sha" dispatch
echo "{\"workflow_runs\":[$(run 15 completed $TIP octocat 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect "a dispatch by a person" dispatch
echo "{\"workflow_runs\":[$(run 16 completed $TIP "$BOT" 2026-01-01T02:00:00Z),$(run 17 queued $TIP "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect "two runs, one pending" "wait 17"
echo "{\"workflow_runs\":[$(run 18 queued $TIP "$BOT" 2026-01-01T02:00:00Z),$(run 19 completed $TIP "$BOT" 2026-01-02T02:00:00Z)]}" > "$TMP/runs.json"
expect "an older run still pending" "wait 18"

# The listing asks for the sha it was given.
grep -q "head_sha=$TIP" "$TMP/gh.log" || fail "the request does not carry the sha"

echo "PASS: 8 cases"
