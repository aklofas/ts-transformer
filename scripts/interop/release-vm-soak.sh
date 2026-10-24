#!/usr/bin/env bash
# The release VM's soak, as the operations the release checklist performs
# on it: a pre-flight that fails before anything is generated, a clean
# stop of a running soak that keeps its evidence and frees its media, a
# detached launch that only reports success once the roles are up, a
# read-only status for the 24 h / 48 h checkpoints, and the CI-green
# handshake the launch waits on. Each one existed as a hand-typed ssh
# command line before this file; every mistake they encode was made once.
#
# Usage (on the VM unless said otherwise):
#   bash release-vm-soak.sh preflight --outdir DIR [--hours 72] [--seed 1] [-- SOAK_ARGS...]
#   bash release-vm-soak.sh stop --outdir DIR [--keep-as DIR2]
#   bash release-vm-soak.sh launch <full sha> --outdir DIR [--hours 72] [--seed 1]
#       [--wait-marker FILE] [--confirm-timeout-s 3600] [-- SOAK_ARGS...]
#   bash release-vm-soak.sh status --outdir DIR
#   bash release-vm-soak.sh ci-green <full sha> <ssh target|local>
#       [--marker PATH] [--identity FILE] [--known-hosts FILE]
#       [--dispatch-wheels REF] [--repo OWNER/NAME]        (operator side)
#
# preflight  Runs soak.sh --preflight-only into a scratch outdir on DIR's
#            filesystem: the build, the declaration's validation, the
#            memory gate and the rtsp-publish disk rule (1.2 x the
#            pre-generated stream's peak must be free). On a refusal it
#            lists the largest directories beside DIR, which is where a
#            previous run's outdir hides. Exit: soak.sh's (0 ok, 2 refused).
# stop       Ends the soak running in DIR: its supervisor by exact command
#            line, then every pid it recorded (TERM, then KILL), verifies
#            none survives, moves DIR to DIR2 (default DIR-aborted-<UTC>)
#            with DIR.launch.log inside, and deletes the rtsp-publish
#            leg's regenerable media (source + segments) from the kept
#            copy — logs, samples, reports and every other leg stay. Exit 0
#            when nothing is left running.
# launch     Requires the checkout at the sha and clean, DIR absent, and
#            (with --wait-marker) the marker file present before anything
#            starts; runs preflight; launches soak.sh detached (nohup +
#            setsid, log DIR.launch.log); then waits until the run prints
#            its `running until` line and every recorded role is alive,
#            printing `SOAK-UP roles=N running_until=T`. A supervisor that
#            exits first, or a role that dies, or the timeout, exits 1 with
#            the launch log's last lines — a launch is not a success until
#            the roles are.
# status     Roles alive / dead, the fail-fast marker, the SRT receiver's
#            reconnect count, the kernel's OOM-kill count, disk and load.
#            Exit 1 when a role is dead or the marker exists.
# ci-green   On a machine with an authenticated `gh`: waits until the ci,
#            jvm-jar and python-wheels workflows each have a successful run
#            on the sha, then creates the marker on the VM over ssh (or
#            locally with target `local`). --dispatch-wheels REF starts the
#            wheels dry run once, after checking REF's head is the sha.
#
# Environment: SRT_RECONNECT_MODE (default background) and every soak.sh
# knob pass through to preflight and launch. TSTRANS_REPO_DIR is the
# checkout (default: this script's repository).
#
# Validated on Ubuntu 24.04 x86_64 (the release VM); see lib.sh for this
# directory's portability stance.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=${TSTRANS_REPO_DIR:-$(cd "$HERE/../.." && pwd)}
SOAK_SH=$REPO_DIR/scripts/interop/soak.sh
export PATH=$HOME/.cargo/bin:/snap/bin:$PATH
export SRT_RECONNECT_MODE=${SRT_RECONNECT_MODE:-background}

usage() { awk 'NR >= 2 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'; }
die() { echo "release-vm-soak.sh: $*" >&2; exit 2; }
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# alive <pid> — true while the process exists and is not a zombie (a
# killed child whose parent has not reaped it still answers `kill -0`).
alive() {
  local st
  st=$(ps -o stat= -p "$1" 2>/dev/null) || return 1
  [[ -n "${st// /}" && "${st// /}" != Z* ]]
}

# soak_supervisor_pids <outdir> — the soak.sh processes driving OUTDIR,
# matched on the exact `soak.sh --outdir OUTDIR` text (never on a bare
# word: `pkill -f tst-interop` once killed the operator's own ssh session,
# whose command line mentioned it). soak.sh runs some legs in subshells
# that carry the same command line; all of them are the supervisor.
soak_supervisor_pids() {
  pgrep -f "scripts/interop/soak\.sh .*--outdir ${1}( |\$)" || true
}

# role_pids <outdir> — "<role> <pid>" per recorded role.
role_pids() {
  local p
  for p in "$1"/pids/*.pid; do
    [[ -f "$p" ]] || continue
    echo "$(basename "$p" .pid) $(cat "$p")"
  done
}

# --- preflight -----------------------------------------------------------

cmd_preflight() {
  local outdir="" hours=72 seed=1 extra=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --outdir) outdir=$2; shift 2 ;;
      --hours) hours=$2; shift 2 ;;
      --seed) seed=$2; shift 2 ;;
      --) shift; extra=("$@"); break ;;
      *) die "preflight: unknown argument: $1" ;;
    esac
  done
  [[ -n "$outdir" ]] || die "preflight: --outdir is required"
  local parent scratch rc=0
  parent=$(dirname "$outdir")
  [[ -d "$parent" ]] || die "preflight: $parent does not exist"
  # Same filesystem as the real outdir, so the disk rule measures the
  # right volume; removed afterwards whatever happened.
  scratch=$(mktemp -d "$parent/.soak-preflight-XXXXXX")
  local log="$scratch.log"
  bash "$SOAK_SH" --preflight-only --outdir "$scratch" --hours "$hours" --seed "$seed" "${extra[@]}" >"$log" 2>&1 || rc=$?
  cat "$log" >&2
  rm -rf "$scratch"
  if [[ $rc -ne 0 ]]; then
    echo "release-vm-soak.sh: preflight refused (rc=$rc)" >&2
    # Only the disk rule is about the neighbours; a memory-gate or
    # declaration refusal names its own remedy.
    if grep -q '< 1.2 x peak' "$log"; then
      echo "release-vm-soak.sh: the largest directories beside $outdir:" >&2
      du -sh "$parent"/*/ "$parent"/*.tar.gz 2>/dev/null | sort -rh | head -8 >&2
      echo "release-vm-soak.sh: a previous run's outdir is the usual offender — \`stop\` keeps its logs and frees its media" >&2
    fi
  fi
  rm -f "$log"
  echo "PREFLIGHT rc=$rc"
  return "$rc"
}

# --- stop ----------------------------------------------------------------

cmd_stop() {
  local outdir="" keep_as=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --outdir) outdir=$2; shift 2 ;;
      --keep-as) keep_as=$2; shift 2 ;;
      *) die "stop: unknown argument: $1" ;;
    esac
  done
  [[ -n "$outdir" ]] || die "stop: --outdir is required"
  [[ -d "$outdir" ]] || die "stop: $outdir is not a directory"
  outdir=${outdir%/}
  [[ -n "$keep_as" ]] || keep_as="$outdir-aborted-$(date -u +%Y%m%dT%H%M%SZ)"
  [[ ! -e "$keep_as" ]] || die "stop: $keep_as already exists"

  local pids role pid p
  # The supervisor first, so it cannot read a role's death as a failure
  # and write a fail-fast marker into the evidence we are keeping.
  pids=$(soak_supervisor_pids "$outdir")
  for pid in $pids; do kill -TERM "$pid" 2>/dev/null || true; done
  sleep 2
  while read -r role pid; do
    [[ -n "${pid:-}" ]] || continue
    kill -TERM "$pid" 2>/dev/null || true
  done < <(role_pids "$outdir")
  sleep 5
  for pid in $pids; do alive "$pid" && kill -KILL "$pid" 2>/dev/null; done
  while read -r role pid; do
    [[ -n "${pid:-}" ]] || continue
    alive "$pid" && kill -KILL "$pid" 2>/dev/null
  done < <(role_pids "$outdir")
  sleep 1

  local left=0
  for pid in $pids; do alive "$pid" && { echo "stop: supervisor $pid still alive" >&2; left=$((left + 1)); }; done
  while read -r role pid; do
    [[ -n "${pid:-}" ]] || continue
    alive "$pid" && { echo "stop: role $role ($pid) still alive" >&2; left=$((left + 1)); }
  done < <(role_pids "$outdir")
  [[ $left -eq 0 ]] || { echo "SOAK-STOPPED at $(now) FAILED: $left process(es) survived" >&2; return 1; }

  mv "$outdir" "$keep_as" || return 1
  [[ ! -f "$outdir.launch.log" ]] || mv "$outdir.launch.log" "$keep_as/launch.log"
  # The regenerable media only: the pre-generated source and its cut
  # segments are the leg's input, reproducible from the seed, and the
  # bulk of a stopped run (28 GiB of a 29 GiB outdir at 36 h). Everything
  # the run measured stays.
  local n=0 bytes=0 f
  for f in "$keep_as"/rtsp-publish/source.ts "$keep_as"/rtsp-publish/segments/*.ts; do
    [[ -f "$f" ]] || continue
    bytes=$((bytes + $(stat -c %s "$f")))
    rm -f "$f"
    n=$((n + 1))
  done
  echo "SOAK-STOPPED at $(now) kept=$keep_as pruned=$n media file(s), $((bytes / 1073741824)) GiB"
}

# --- launch --------------------------------------------------------------

cmd_launch() {
  local sha="" outdir="" hours=72 seed=1 marker="" confirm_timeout=3600 extra=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --outdir) outdir=$2; shift 2 ;;
      --hours) hours=$2; shift 2 ;;
      --seed) seed=$2; shift 2 ;;
      --wait-marker) marker=$2; shift 2 ;;
      --confirm-timeout-s) confirm_timeout=$2; shift 2 ;;
      --) shift; extra=("$@"); break ;;
      -*) die "launch: unknown argument: $1" ;;
      *) [[ -z "$sha" ]] || die "launch: one sha only"; sha=$1; shift ;;
    esac
  done
  [[ ${#sha} -eq 40 ]] || die "launch: need the full 40-hex sha (got '$sha')"
  [[ -n "$outdir" ]] || die "launch: --outdir is required"
  outdir=${outdir%/}
  [[ ! -e "$outdir" ]] || die "launch: $outdir exists — \`stop\` a running soak or choose another --outdir"
  local head dirty
  head=$(cd "$REPO_DIR" && git rev-parse HEAD) || die "launch: $REPO_DIR is not a checkout"
  [[ "$head" == "$sha" ]] || die "launch: $REPO_DIR is at $head, not $sha"
  dirty=$(cd "$REPO_DIR" && git status --short)
  [[ -z "$dirty" ]] || { echo "$dirty" >&2; die "launch: the checkout is not clean"; }

  if [[ -n "$marker" ]]; then
    echo "launch: waiting for $marker (CI green on $sha)..."
    until [[ -f "$marker" ]]; do sleep 60; done
    echo "CI-GREEN seen at $(now)"
  fi

  cmd_preflight --outdir "$outdir" --hours "$hours" --seed "$seed" -- "${extra[@]}" || return $?

  local log="$outdir.launch.log"
  (cd "$REPO_DIR" &&
    nohup setsid bash scripts/interop/soak.sh --outdir "$outdir" --hours "$hours" --seed "$seed" "${extra[@]}" \
      >"$log" 2>&1 </dev/null &
  )
  sleep 2
  local sup
  sup=$(soak_supervisor_pids "$outdir" | head -1)
  [[ -n "$sup" ]] || { echo "launch: no soak.sh process after 2 s; log:" >&2; tail -20 "$log" >&2; return 1; }
  echo "SOAK-LAUNCHED pid=$sup at $(now) log=$log"

  # Not up until the run says so: the `running until` line comes after
  # the build, the pre-generation (tens of minutes for 72 h) and every
  # role's launch. A supervisor that exits before it is a refusal.
  local deadline=$(( $(date +%s) + confirm_timeout )) until_line=""
  while :; do
    until_line=$(grep -m1 -o 'running until [^ ]*' "$log" 2>/dev/null || true)
    [[ -z "$until_line" ]] || break
    if ! alive "$sup"; then
      echo "launch: the supervisor exited before the run started; log:" >&2
      tail -15 "$log" >&2
      echo "SOAK-UP FAILED"
      return 1
    fi
    [[ $(date +%s) -lt $deadline ]] || { echo "launch: no 'running until' line within ${confirm_timeout}s" >&2; tail -15 "$log" >&2; echo "SOAK-UP FAILED"; return 1; }
    sleep 30
  done
  sleep 10
  local roles=0 dead=0 role pid
  while read -r role pid; do
    [[ -n "${pid:-}" ]] || continue
    roles=$((roles + 1))
    alive "$pid" || { dead=$((dead + 1)); echo "launch: role $role ($pid) is dead" >&2; }
  done < <(role_pids "$outdir")
  if [[ $roles -eq 0 || $dead -gt 0 || -e "$outdir/soak-FAILED" ]]; then
    tail -15 "$log" >&2
    echo "SOAK-UP FAILED roles=$roles dead=$dead"
    return 1
  fi
  echo "SOAK-UP roles=$roles ${until_line} at $(now)"
}

# --- status --------------------------------------------------------------

cmd_status() {
  local outdir=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --outdir) outdir=$2; shift 2 ;;
      *) die "status: unknown argument: $1" ;;
    esac
  done
  [[ -n "$outdir" ]] || die "status: --outdir is required"
  [[ -d "$outdir" ]] || die "status: $outdir is not a directory"
  outdir=${outdir%/}
  local bad=0 roles=0 role pid
  echo "status at $(now) for $outdir"
  while read -r role pid; do
    [[ -n "${pid:-}" ]] || continue
    roles=$((roles + 1))
    if alive "$pid"; then
      echo "  $role $pid $(ps -o stat= -p "$pid" 2>/dev/null)"
    else
      echo "  $role $pid DEAD"; bad=$((bad + 1))
    fi
  done < <(role_pids "$outdir")
  echo "roles: $roles alive: $((roles - bad))"
  if [[ -e "$outdir/soak-FAILED" ]]; then echo "fail-fast marker: PRESENT"; cat "$outdir/soak-FAILED"; bad=$((bad + 1)); else echo "fail-fast marker: none"; fi
  [[ ! -f "$outdir.launch.log" ]] || grep -o 'running until [^ ]*' "$outdir.launch.log" | tail -1
  [[ ! -f "$outdir/logs/srt-recv.log" ]] || echo "srt-recv: $(grep -o 'reconnects=[0-9]*' "$outdir/logs/srt-recv.log" | tail -1)"
  echo "oom kills: $(journalctl -k --no-pager 2>/dev/null | grep -ci 'out of memory' || echo 0)"
  echo "samples: $(($(wc -l <"$outdir/rss.csv" 2>/dev/null || echo 1) - 1)) rss rows"
  df -h "$outdir" | tail -1
  uptime
  [[ $bad -eq 0 ]]
}

# --- ci-green (operator side) -------------------------------------------

cmd_ci_green() {
  # The marker's default is expanded by the VM's shell (`touch ~/ci-green`
  # over ssh), or by the tilde substitution below for target `local`.
  # shellcheck disable=SC2088
  local sha="" target="" marker='~/ci-green' identity="" known_hosts="" dispatch_ref="" repo=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --marker) marker=$2; shift 2 ;;
      --identity) identity=$2; shift 2 ;;
      --known-hosts) known_hosts=$2; shift 2 ;;
      --dispatch-wheels) dispatch_ref=$2; shift 2 ;;
      --repo) repo=$2; shift 2 ;;
      -*) die "ci-green: unknown argument: $1" ;;
      *)
        if [[ -z "$sha" ]]; then sha=$1; elif [[ -z "$target" ]]; then target=$1; else die "ci-green: too many arguments"; fi
        shift
        ;;
    esac
  done
  [[ ${#sha} -eq 40 ]] || die "ci-green: need the full 40-hex sha (got '$sha')"
  [[ -n "$target" ]] || die "ci-green: need the ssh target (or 'local')"
  command -v gh >/dev/null 2>&1 || die "ci-green: gh is not installed here"
  [[ -n "$repo" ]] || repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null) || die "ci-green: pass --repo OWNER/NAME"

  local ssh_opts=()
  [[ -z "$identity" ]] || ssh_opts+=(-i "$identity")
  [[ -z "$known_hosts" ]] || ssh_opts+=(-o "UserKnownHostsFile=$known_hosts" -o StrictHostKeyChecking=yes)

  if [[ -n "$dispatch_ref" ]]; then
    local ref_sha
    ref_sha=$(gh api "repos/$repo/commits/$dispatch_ref" --jq .sha) || die "ci-green: cannot resolve $dispatch_ref"
    [[ "$ref_sha" == "$sha" ]] || die "ci-green: $dispatch_ref is at $ref_sha, not $sha — a dispatched run would test the wrong tree"
    gh workflow run python-wheels.yml -R "$repo" --ref "$dispatch_ref" && echo "ci-green: wheels dry run dispatched on $dispatch_ref"
  fi

  # successes <workflow file> — successful runs of it on the sha.
  successes() {
    gh api "repos/$repo/actions/workflows/$1/runs?head_sha=$sha&per_page=30" \
      --jq '[.workflow_runs[] | select(.conclusion=="success")] | length'
  }
  unfinished() {
    gh api "repos/$repo/actions/workflows/$1/runs?head_sha=$sha&per_page=30" \
      --jq '[.workflow_runs[] | select(.status!="completed")] | length'
  }
  local c j w
  while :; do
    c=$(successes ci.yml); j=$(successes jvm-jar.yml); w=$(successes python-wheels.yml)
    echo "$(now) ci=$c jvm-jar=$j wheels=$w (unfinished: ci=$(unfinished ci.yml) jvm-jar=$(unfinished jvm-jar.yml) wheels=$(unfinished python-wheels.yml))"
    if [[ "$c" -ge 1 && "$j" -ge 1 && "$w" -ge 1 ]]; then break; fi
    sleep 120
  done
  if [[ "$target" == local ]]; then
    touch "${marker/#\~/$HOME}"
  else
    ssh "${ssh_opts[@]}" "$target" "touch $marker" || die "ci-green: could not create $marker on $target"
  fi
  echo "CI-GREEN marked for $sha on $target ($marker) at $(now)"
}

# --- dispatch ------------------------------------------------------------

[[ $# -ge 1 ]] || { usage; exit 2; }
cmd=$1; shift
case "$cmd" in
  preflight) cmd_preflight "$@" ;;
  stop) cmd_stop "$@" ;;
  launch) cmd_launch "$@" ;;
  status) cmd_status "$@" ;;
  ci-green) cmd_ci_green "$@" ;;
  -h | --help) usage ;;
  *) die "unknown command: $cmd (preflight | stop | launch | status | ci-green)" ;;
esac
