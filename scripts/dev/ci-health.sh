#!/usr/bin/env bash
# CI health snapshot.
#
# GitHub already records everything the flake ledger in the maintainer's
# memory tries to track — run_attempt (>1 = somebody pressed rerun), per-job
# conclusions per attempt, and run duration (a 6-hour cancel is the wedge
# class). Nobody aggregated it: a rerun that passes shows as `success`, so
# a 1-in-8 real race (an RTSP-burst case) was indistinguishable from
# weather. This script turns the last N days of runs into one CSV row plus
# per-job rerun counts, and prints ALERT lines for job names rerun >= 2×.
#
# Lives under scripts/dev/ (not scripts/check/): it needs network + a token
# and is a report, not a gate — it always exits 0 on a successful query.
#
# Usage:
#   scripts/dev/ci-health.sh                              # report to stdout
#   scripts/dev/ci-health.sh --append ci-health-week.csv        # the weekly workflow's artifact
#   CI_HEALTH_DAYS=30 scripts/dev/ci-health.sh
#   CI_HEALTH_RUNS_JSON=fixture.json scripts/dev/ci-health.sh   # no API
#
# Env: CI_HEALTH_REPO (aklofas/ts-transformer), CI_HEALTH_DAYS (7),
#      CI_HEALTH_WEDGE_MIN (60), CI_HEALTH_ALERT_RERUNS (2),
#      CI_HEALTH_RUNS_JSON (path to a `{workflow_runs:[…]}` document or a
#      concatenation of them, as `gh api --paginate` emits).
# In Actions: GH_TOKEN=${{ github.token }} with `permissions: actions: read`.
set -euo pipefail

REPO="${CI_HEALTH_REPO:-aklofas/ts-transformer}"
DAYS="${CI_HEALTH_DAYS:-7}"
WEDGE_MIN="${CI_HEALTH_WEDGE_MIN:-60}"
ALERT_RERUNS="${CI_HEALTH_ALERT_RERUNS:-2}"
APPEND=""
while [ $# -gt 0 ]; do
  case "$1" in
    --append) APPEND="$2"; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

command -v jq >/dev/null || { echo "FAIL: jq not installed" >&2; exit 2; }

# `gh api --paginate`, retried: GitHub answers 502 when a page takes too long
# to build, and gh then writes the error body to stdout, so its output goes on
# only after gh exited 0. Waits (seconds) between the three attempts.
api() {
  local path="$1" i=0 out last
  local -a waits
  read -r -a waits <<< "${CI_HEALTH_RETRY_SLEEPS:-2 5}"
  while :; do
    if out="$(gh api --paginate "$path" 2>"$tmp/api.err")"; then
      printf '%s\n' "$out"
      return 0
    fi
    last="$(tail -n 1 "$tmp/api.err")"
    [ -n "$last" ] || last="$(printf '%s\n' "$out" | tail -n 1)"
    i=$((i + 1))
    if [ "$i" -ge 3 ]; then
      echo "ci-health: $path: gh api failed ($last)" >&2
      return 1
    fi
    sleep "${waits[$((i - 1))]:-5}"
  done
}

# Window start (GNU date, BSD fallback).
since="$(date -u -d "-${DAYS} days" +%Y-%m-%d 2>/dev/null || date -u -v-"${DAYS}"d +%Y-%m-%d)"
now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/ci-health.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# ---- 1. completed runs in the window --------------------------------------
if [ -n "${CI_HEALTH_RUNS_JSON:-}" ]; then
  cat "$CI_HEALTH_RUNS_JSON" > "$tmp/runs.raw"
else
  command -v gh >/dev/null || { echo "FAIL: gh not installed" >&2; exit 2; }
  # `created=>=DATE` is GitHub's documented filter; `>=` is sent URL-encoded.
  api "repos/$REPO/actions/runs?created=%3E%3D$since&per_page=100" > "$tmp/runs.raw"
fi
# --paginate concatenates JSON documents; jq -s merges them.
jq -s '[.[] | .workflow_runs[]] | map(select(.status == "completed"))
       | map({id, name, run_attempt, conclusion, event,
              minutes: (((.updated_at | fromdateiso8601) - (.run_started_at | fromdateiso8601)) / 60 | floor)})' \
   "$tmp/runs.raw" > "$tmp/runs.json"

runs="$(jq 'length' "$tmp/runs.json")"
first_try_green="$(jq '[.[] | select(.run_attempt == 1 and .conclusion == "success")] | length' "$tmp/runs.json")"
rerun_runs="$(jq '[.[] | select(.run_attempt > 1)] | length' "$tmp/runs.json")"
wedges="$(jq --argjson m "$WEDGE_MIN" '[.[] | select(.minutes > $m)] | length' "$tmp/runs.json")"
unrecovered="$(jq '[.[] | select(.conclusion == "failure")] | length' "$tmp/runs.json")"

# ---- 2. which JOBS were rerun (attempt-1 failures inside rerun runs) ------
: > "$tmp/rerun-jobs.txt"
: > "$tmp/unread-runs.txt"
if [ -z "${CI_HEALTH_RUNS_JSON:-}" ]; then
  for id in $(jq -r '.[] | select(.run_attempt > 1) | .id' "$tmp/runs.json"); do
    # A listing of a rerun `ci` run holds 90 or more jobs with their steps;
    # a page of 100 takes about ten seconds to build, a page of 30 under two.
    if ! api "repos/$REPO/actions/runs/$id/jobs?filter=all&per_page=30" > "$tmp/jobs.json"; then
      echo "$id" >> "$tmp/unread-runs.txt"
      continue
    fi
    jq -r '.jobs[] | select(.run_attempt == 1 and (.conclusion == "failure" or .conclusion == "cancelled" or .conclusion == "timed_out")) | .name' \
      "$tmp/jobs.json" >> "$tmp/rerun-jobs.txt"
  done
fi
# "name:count;name:count" sorted by count desc — the per-job rerun table.
rerun_jobs="$(sort "$tmp/rerun-jobs.txt" | uniq -c | sort -rn | awk '{c=$1; $1=""; sub(/^ /,""); printf "%s%s:%d", (NR>1?";":""), $0, c}')"
# Human-readable display only: in offline mode (CI_HEALTH_RUNS_JSON) the jobs
# endpoint is never queried, so an empty $rerun_jobs means "not computed", not
# "zero reruns" — say so instead of printing "none".
# The CSV field itself stays the raw (possibly empty) $rerun_jobs — a prose
# string there would break machine parsing of the appended CSV.
if [ -n "${CI_HEALTH_RUNS_JSON:-}" ]; then
  rerun_jobs_display="unavailable (offline mode — no jobs API query)"
else
  rerun_jobs_display="${rerun_jobs:-none}"
fi

# ---- 3. report --------------------------------------------------------------
{
  echo "CI health — $REPO — last $DAYS days (since $since, snapshot $now)"
  echo "  completed runs:        $runs"
  echo "  green on first try:    $first_try_green"
  echo "  rerun runs (attempt>1): $rerun_runs"
  echo "  wedges (> ${WEDGE_MIN} min):  $wedges"
  echo "  unrecovered failures:  $unrecovered"
  echo "  rerun jobs:            $rerun_jobs_display"
  echo
  echo "  per-workflow:"
  # jq's group_by assumes sorted input (it groups by contiguous run, not by
  # equal key across the whole array) — sort_by first, or identically-named
  # workflows split into multiple bogus groups if they weren't adjacent in
  # the API's newest-first ordering.
  jq -r 'sort_by(.name) | group_by(.name) | .[] | "    \(.[0].name): \(length) runs, \([.[] | select(.run_attempt > 1)] | length) reruns, \([.[] | select(.conclusion == "failure")] | length) failures, max \(map(.minutes) | max) min"' "$tmp/runs.json"
} | tee "$tmp/report.txt"

if [ -s "$tmp/unread-runs.txt" ]; then
  echo "Runs whose jobs could not be read (left out of the rerun jobs): $(paste -sd' ' "$tmp/unread-runs.txt")"
fi

sort "$tmp/rerun-jobs.txt" | uniq -c | while read -r count name; do
  if [ "$count" -ge "$ALERT_RERUNS" ]; then
    echo "ALERT: job '$name' was rerun $count times in $DAYS days — a rerun class, not weather (harvest before the next rerun)"
  fi
done

csv_row="$now,$DAYS,$runs,$first_try_green,$rerun_runs,$wedges,$unrecovered,\"$rerun_jobs\""
header="snapshot_utc,window_days,runs,first_try_green,rerun_runs,wedges_over_${WEDGE_MIN}m,unrecovered_failures,rerun_jobs"
echo "csv: $csv_row"

if [ -n "$APPEND" ]; then
  [ -s "$APPEND" ] || echo "$header" > "$APPEND"
  echo "$csv_row" >> "$APPEND"
  echo "appended to $APPEND"
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "## CI health — last $DAYS days"
    echo
    echo '```'
    cat "$tmp/report.txt"
    echo '```'
    echo
    echo "\`$header\`"
    echo
    echo "\`$csv_row\`"
    if [ -s "$tmp/unread-runs.txt" ]; then
      echo
      echo "### Runs whose jobs could not be read"
      echo
      sed 's/^/- run /' "$tmp/unread-runs.txt"
    fi
  } >> "$GITHUB_STEP_SUMMARY"
fi
