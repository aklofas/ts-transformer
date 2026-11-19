#!/usr/bin/env bash
# Usage: nightly-dedup.sh <tip-sha>
# Prints one line: `dispatch` (no nightly run of ci.yml on the sha), `wait <id>`
# (one is queued or in progress) or `skip <id>` (one completed). A nightly run
# is a workflow_dispatch run of ci.yml by github-actions[bot]; a pending run
# wins over a completed one. GH_TOKEN and GITHUB_REPOSITORY come from the
# environment.
set -euo pipefail

tip="$1"

runs="$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs?head_sha=$tip&event=workflow_dispatch&actor=github-actions%5Bbot%5D&per_page=100")"

jq -r --arg tip "$tip" '
  [.workflow_runs[]
   | select(.head_sha == $tip and .event == "workflow_dispatch" and .actor.login == "github-actions[bot]")]
  | sort_by(.created_at) as $all
  | ([$all[] | select(.status != "completed")] | last) as $pending
  | ([$all[] | select(.status == "completed")] | last) as $done
  | if $pending != null then "wait \($pending.id)"
    elif $done != null then "skip \($done.id)"
    else "dispatch" end' <<<"$runs"
