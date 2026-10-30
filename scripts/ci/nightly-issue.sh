#!/usr/bin/env bash
# Usage: nightly-issue.sh <sha> <run_url> <failed_jobs_csv>
# Opens an issue for a red nightly ci run, or comments on the open issue that
# already carries the ci-nightly label. GH_REPO and GH_TOKEN come from the
# environment.
set -euo pipefail

sha="$1"
run_url="$2"
failed="$3"
short="${sha:0:7}"

body="The nightly \`ci\` run on \`$sha\` failed: $run_url

Failed jobs: $failed

Rerun the run if every failed job matches a row in docs/project/ci-flakes.md; otherwise this is a finding."

open="$(gh issue list --label ci-nightly --state open --json number --jq '.[0].number // empty')"

if [ -n "$open" ]; then
  gh issue comment "$open" --body "$body"
else
  gh issue create --label ci-nightly --title "nightly ci red on $short" --body "$body"
fi
