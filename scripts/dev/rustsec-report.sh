#!/usr/bin/env bash
# Usage: rustsec-report.sh [--dry-run]
# The weekly RustSec report: `cargo deny check advisories` against the LIVE
# advisory-db on every workspace of scripts/dev/cargo-deny-all.sh. During a
# release freeze ci checks the pinned commit (tests/coverage/advisory-db-pin.toml),
# so this is where an advisory published since then shows. When cargo-deny
# reports any advisory, the open issue labelled dependency-advisory with the
# title below gets the list as a comment, or is opened with it; --dry-run only
# prints the list. An advisory never fails the script; a workspace cargo-deny
# could not check does. GH_REPO and GH_TOKEN come from the environment.
# It also warns when the advisory-db pin is still set although the tag of the
# workspace version exists (the release it was set for is out).
# RUSTSEC_PIN_FILE and RUSTSEC_TAGS (the tag list) stand in for the self-test.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LABEL=dependency-advisory
TITLE="RustSec advisories in the committed lockfiles"
dry=0
case "${1:-}" in
  --dry-run) dry=1 ;;
  "") ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

# The pin is cleared by the first pull request after the tag; a pin still set
# then would keep ci on a database that no longer moves.
pin_file="${RUSTSEC_PIN_FILE:-$ROOT/tests/coverage/advisory-db-pin.toml}"
pin="$(sed -nE 's/^rev[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$pin_file" | head -1)"
if [ -n "$pin" ]; then
  ver="$(sed -nE '/^\[workspace\.package\]/,/^\[/ s/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$ROOT/Cargo.toml" | head -1)"
  tags="${RUSTSEC_TAGS-$(git -C "$ROOT" ls-remote --tags --refs origin "refs/tags/v$ver" 2>/dev/null | sed 's|.*refs/tags/||' || true)}"
  if [ -n "$ver" ] && printf '%s\n' "$tags" | grep -qx "v$ver"; then
    echo "::warning::the advisory-db pin in tests/coverage/advisory-db-pin.toml is still set although v$ver is tagged: clear it (docs/project/releasing.md, release step 8 \"Clear the advisory-db pin\")"
  fi
fi

report=""
broken=()
while IFS= read -r m; do
  rc=0
  out="$(cargo deny --format json --color never --manifest-path "$ROOT/$m" --config "$ROOT/deny.toml" --locked \
    check advisories 2>&1)" || rc=$?
  rows="$(printf '%s\n' "$out" | { grep '^{' || true; } | jq -r 'select(.type == "diagnostic" and .fields.advisory != null)
    | "- `\(.fields.advisory.id)` \(.fields.graphs[0].Krate.name // .fields.advisory.package) \(.fields.graphs[0].Krate.version // ""): \(.fields.advisory.title)"' | sort -u)"
  if [ -n "$rows" ]; then
    report+="### \`$m\`"$'\n\n'"$rows"$'\n\n'
  elif [ "$rc" -ne 0 ]; then
    printf '%s\n' "$out" | tail -20
    broken+=("$m")
  fi
done < <(bash "$ROOT/scripts/dev/cargo-deny-all.sh" --list)

for m in ${broken[@]+"${broken[@]}"}; do echo "rustsec-report: cargo-deny could not check $m"; done

if [ -z "$report" ]; then
  if [ ${#broken[@]} -eq 0 ]; then
    echo "rustsec-report: no advisories in any workspace"
    exit 0
  fi
  echo "rustsec-report: no advisory found, but ${#broken[@]} workspace(s) could not be checked"
  exit 1
fi

run_url=""
[ -n "${GITHUB_RUN_ID:-}" ] && run_url=" ($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID)"
body="cargo-deny reports these RustSec advisories against the live advisory-db$run_url:

$report
Each needs a fix (a dependency bump) or an \`ignore\` entry with its reason in deny.toml. An advisory that fails the check turns \`ci\` red too, except while a release freeze pins the advisory-db (tests/coverage/advisory-db-pin.toml)."

printf '%s\n' "$body"
[ -z "${GITHUB_STEP_SUMMARY:-}" ] || printf '%s\n' "$body" >> "$GITHUB_STEP_SUMMARY"

if [ "$dry" -eq 0 ]; then
  open="$(gh issue list --label "$LABEL" --state open --limit 200 --json number,title \
    --jq "map(select(.title == \"$TITLE\")) | .[0].number // empty")"
  if [ -n "$open" ]; then
    gh issue comment "$open" --body "$body"
  else
    gh issue create --label "$LABEL" --title "$TITLE" --body "$body"
  fi
fi
[ ${#broken[@]} -eq 0 ]
