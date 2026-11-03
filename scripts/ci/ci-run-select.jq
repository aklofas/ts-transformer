# The ci run a qualification reads: input is one page of the workflow-runs API
# (`.workflow_runs`), `--arg allow_pr true|false` admits pull_request runs.
# Only a full run counts: `push` and `workflow_dispatch` (a pull_request run skips
# the test-main-extra shards); a rehearsal on a PR head also admits its own
# pull_request run. The newest eligible run wins, ties by run id, whatever its
# status, so a run still in progress is waited for rather than skipped.
# No eligible run reads as conclusion "missing". Callers: the ci-lookup job of
# release-qualify.yml and the release-preflight script.
[.workflow_runs[]
 | select(.event == "push" or .event == "workflow_dispatch"
          or ($allow_pr == "true" and .event == "pull_request"))]
| sort_by(.created_at, .id) | last // null
| if . == null
  then {status: "completed", conclusion: "missing", run_url: "", event: null,
        run_id: null, run_attempt: null, head_sha: null}
  else {status, conclusion: (.conclusion // "missing"), run_url: .html_url, event,
        run_id: .id, run_attempt, head_sha}
  end
