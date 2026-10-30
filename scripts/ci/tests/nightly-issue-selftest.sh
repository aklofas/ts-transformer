#!/usr/bin/env bash
# Self-test of scripts/ci/nightly-issue.sh against a stub `gh` that logs its
# arguments and answers `issue list` from $TMP/open.json. Asserts the issue
# list asks for open ci-nightly issues, and the issue is created when none is
# open and commented on (with the run URL) when one is.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/nightly-issue.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir "$TMP/bin"
cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
# Matches on the subcommand pair only, never the full argument string.
sub="$1 $2"
printf '%s' "$sub" >> "$TMP/gh.log"
shift 2 || true
for a in "$@"; do printf ' [%s]' "$a" >> "$TMP/gh.log"; done
printf '\n' >> "$TMP/gh.log"
case "$sub" in
  "issue list")
    # The script passes --jq '.[0].number // empty'; emulate it on open.json.
    n="$(sed -n 's/.*"number":\([0-9]*\).*/\1/p' "$TMP/open.json")"
    printf '%s\n' "$n" | sed '/^$/d' ;;
  "issue create") echo "https://example.invalid/issues/99" ;;
  "issue comment") : ;;
  *) echo "stub gh: unexpected call" >&2; exit 2 ;;
esac
STUB
chmod +x "$TMP/bin/gh"
export TMP PATH="$TMP/bin:$PATH"

run_case() {
  : > "$TMP/gh.log"
  bash "$SCRIPT" abc1234def5678 https://example.invalid/runs/1 "build,test-main"
}

# Case 1: no open issue -> create
echo '[]' > "$TMP/open.json"
run_case
[ "$(grep -c '^issue create' "$TMP/gh.log")" = 1 ] || { echo "FAIL: expected one issue create"; cat "$TMP/gh.log"; exit 1; }
grep -q '^issue create.*\[--label\] \[ci-nightly\]' "$TMP/gh.log" || { echo "FAIL: create lacks --label ci-nightly"; cat "$TMP/gh.log"; exit 1; }
grep -q '\[--title\] \[nightly ci red on abc1234\]' "$TMP/gh.log" || { echo "FAIL: create lacks the expected title"; cat "$TMP/gh.log"; exit 1; }
grep -q 'build,test-main' "$TMP/gh.log" || { echo "FAIL: create body lacks the failed jobs"; exit 1; }
grep -q '^issue list.*\[--label\] \[ci-nightly\]' "$TMP/gh.log" || { echo "FAIL: issue list lacks --label ci-nightly"; cat "$TMP/gh.log"; exit 1; }
grep -q '^issue list.*\[--state\] \[open\]' "$TMP/gh.log" || { echo "FAIL: issue list lacks --state open"; cat "$TMP/gh.log"; exit 1; }
! grep -q '^issue comment' "$TMP/gh.log" || { echo "FAIL: unexpected issue comment"; exit 1; }

# Case 2: open issue 41 -> comment on it, no create
echo '[{"number":41}]' > "$TMP/open.json"
run_case
[ "$(grep -c '^issue comment \[41\]' "$TMP/gh.log")" = 1 ] || { echo "FAIL: expected one issue comment on 41"; cat "$TMP/gh.log"; exit 1; }
grep -q '^issue comment \[41\].*https://example.invalid/runs/1' "$TMP/gh.log" || { echo "FAIL: comment body lacks the run URL"; cat "$TMP/gh.log"; exit 1; }
! grep -q '^issue create' "$TMP/gh.log" || { echo "FAIL: unexpected issue create"; exit 1; }

echo "nightly-issue self-test: OK"
