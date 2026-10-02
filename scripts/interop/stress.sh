#!/usr/bin/env bash
# Stress harness: find where each transport stops coping, then (Task 12)
# hold a fraction of that load for a day. Where soak.sh asks "does a
# fixed, realistic load survive 72 hours of impairment?", this script asks
# "how much load does one box carry before a verdict fails?" — per
# transport, on two axes, one step at a time — and hands every step to
# `tst-interop report step` and the whole run to `tst-interop report
# stress` for the verdicts. Bash launches, samples and reaps; every
# judgement is Rust.
#
# NEVER RUN THIS ON THE RC SOAK VM. The sweep deliberately drives the box
# to its limits; on a host that is also running a soak it would corrupt
# that soak's resource evidence and this run's ceilings at the same time.
# It runs on its own VM.
#
# # Topology
#
# One STREAM is one process triple, all `tst-interop` talking to itself:
#
#   send -> proxy (UDP relay) -> recv          srt, rist, udp
#   send ----------------------> recv          tcp (no proxy: it relays UDP)
#
#   - srt:  recv `srt://:PORT?mode=listener&latency=1200 --managed`,
#           send `srt://PROXY?latency=1200 --managed --reconnect-mode
#           $SRT_RECONNECT_MODE` (both ends managed, as in soak.sh).
#   - rist: recv `rist://@0.0.0.0:PORT?buffer=1200`, send
#           `rist://PROXY?buffer=1200` (unmanaged, as in soak.sh).
#   - udp:  recv `udp://127.0.0.1:PORT`, send `udp://PROXY`.
#   - tcp:  recv `tcp://127.0.0.1:PORT?listen=1`, send `tcp://127.0.0.1:PORT`.
#
# Every sender runs `--profile baseline --au-sizes realistic --au-scale F
# --klv-set rich` with the corruption tap on (`rate=5,min_gap=1000`), and
# every receiver judges its stream against the sender's corruption log,
# so the correctness verdicts keep their meaning under load. Stream `i`
# (zero-based) uses KLV seed `SEED + i` and corruption seed `SEED + 1 + i`.
# The sweep's proxies run a CLEAN link (`--loss 0 --jitter 0 --delay 0
# --reorder 0,0`): a ceiling is measured without impairment, and the proxy
# stays in the path so the sweep and the hold share one topology.
#
# # The sweep
#
# For each transport in `--transports` order: the STREAMS axis (N
# streams at `--au-scale 1`, N over `--stream-ladder`), then the BITRATE
# axis (one stream at `--au-scale F`, F over `--scale-ladder`). Each step
# launches its streams, runs `--step-warmup-s` + `--step-hold-s` seconds,
# reaps every process, and runs `report step` on its directory. An axis
# stops at its first failing step; the ceiling is the last passing load.
# A step's nominal bitrate is 1.7 Mb/s x F per stream; a step whose
# aggregate exceeds STRESS_MAX_AGG_MBPS is refused before anything
# launches. Each step has a hard wall-clock limit of its run time + 180s:
# on expiry every process of the step is killed and the step FAILs with
# `step_timeout`. Steps are strictly sequential. A step whose `report
# step` cannot judge it (exit 2) while a worker exited nonzero or the
# step timed out is an OVERLOADED step, not a harness error: it FAILs
# with `step_unjudgeable` and stops the axis.
#
# # The hold
#
# After the sweep (unless --skip-hold), every transport runs together
# for --hold-hours at a fraction of its streams ceiling C_t (the highest
# streams load whose step-results.json passed): n_t = max(1,
# floor(0.7 x C_t)). If the sweep's per-stream CPU cost predicts more
# than 70% of the box for the sum, every n_t is scaled down by
# 0.70/predicted (cpu_scale_factor). A transport with no passing streams
# step refuses the hold (exit 1). Stream i (global, 0-based over every
# transport) has leg `<t>-<k>` (k per transport), KLV seed SEED + i,
# corruption seed SEED + 1 + i, and a proxy running the seeded schedule
# `seed=SEED+i,phases=HOLD_SCHEDULE_PHASES,phase_s=hold_s/phases`. SRT
# proxies also cut the link for HOLD_OUTAGE_DUR_S every
# HOLD_OUTAGE_PERIOD_S, and are launched SRT_PROXY_WARMUP_S =
# HOLD_OUTAGE_DUR_S + 30 s before the hold clock starts (soak.sh's
# window-0 reasoning), so in hold time outage n >= 1 starts at
# n x PERIOD - SRT_PROXY_WARMUP_S. Every HOLD_RESTART_PERIOD_S (first at
# HOLD_RESTART_OFFSET_S) the supervisor kills srt-0's receiver and
# relaunches it on the same port; `report hold` then requires srt-0's
# sender to show a fresh reconnect within 120 s. A restart later than
# hold_s - 120 s could not be judged, so none is scheduled there, and a
# restart whose (t - DUR - 60, t + 120] guard band meets an outage
# window is a usage error (exit 2), refused before anything launches:
# its recovery would be indistinguishable from the outage's.
#
# Accepted race: an SRT stream's receiver port is probed free when its
# proxy launches, but the receiver binds it only SRT_PROXY_WARMUP_S
# later, so another process can take it in between (about 0.1% of runs
# at 128 SRT streams). The receiver then fails to bind, the supervisor
# sees it dead at its first poll (minute 1) and the run aborts with
# stress-FAILED. Rerun it.
#
# # Usage
#
#   stress.sh --outdir DIR --seed N [--transports srt,rist,udp,tcp]
#             [--stream-ladder 1,2,4,8,16,32,64,128] [--scale-ladder 1,2,4,8,16,32,64]
#             [--step-warmup-s 60] [--step-hold-s 600] [--hold-hours 24]
#             [--skip-hold] [--smoke] [--dry-run]
#             [--dry-run-ceilings srt=C,...] [--dry-run-cpu srt=F,...]
#
#   --smoke    the end-to-end smoke shape: ladders 1,2 / 1,2, warm-up 15s,
#              step hold 60s, sampler cadence 5s, hold 0.1h, and the hold
#              knobs below shortened (HOLD_OUTAGE_PERIOD_S=345
#              HOLD_OUTAGE_DUR_S=10 HOLD_RESTART_PERIOD_S=120
#              HOLD_RESTART_OFFSET_S=50). Explicit flags and env knobs win.
#              With STRESS_SMOKE_FORCE_FAIL=1 the LAST bitrate step of the
#              LAST transport is judged with `--cpu-headroom-max 0.000001`,
#              so the FAIL path and the ceiling rule run end to end
#              (declared as `smoke_forced_fail_step`). The env var is
#              refused without --smoke.
#   --dry-run  every validation, then one line per step
#              `transport axis load nominal_mbps` (nominal_mbps = the
#              step's AGGREGATE nominal bitrate). Builds nothing, writes
#              nothing, launches nothing. Without --skip-hold it also
#              prints the hold's schedule (restart instants, outage
#              windows, the collision verdict). Exit codes as a real
#              run's pre-flight: 2 bad argument/env/schedule, 1 a
#              refused step.
#   --dry-run-ceilings, --dry-run-cpu  TEST-ONLY, --dry-run only: stand-in
#              streams ceilings and per-stream CPU fractions, one per
#              transport, so --dry-run also prints the hold sizing (a
#              ceiling of 0 = no passing step -> exit 1, as a real run).
#
# # Knobs (environment; all declared in stress-config.json)
#
#   SRT_RECONNECT_MODE              REQUIRED, blocking|background (exit 2 if unset) —
#                                   same rule and reasoning as soak.sh
#   RSS_SLOPE_THRESHOLD_KB_PER_HOUR REQUIRED, > 0 (exit 2 if unset) — `report step`
#                                   has no default for it on purpose
#   STRESS_MAX_AGG_MBPS=2000        refuse a step above this aggregate nominal bitrate
#   CPU_HEADROOM_MAX=0.80           report step thresholds (see `report step`)
#   FD_DELTA_MAX=2
#   THREAD_DELTA_MAX=1
#   DELIVERY_SLACK=0.7
#   QUEUE_DEPTH_FRACTION=0.9
#   HOLD_OUTAGE_PERIOD_S=900        hold only: SRT outage every 15 min,
#   HOLD_OUTAGE_DUR_S=30              30s long,
#   HOLD_RESTART_PERIOD_S=7200        one SRT receiver restarted every 2h,
#   HOLD_RESTART_OFFSET_S=450         first restart this far into the hold,
#   HOLD_SCHEDULE_PHASES=96           seeded impairment schedule phases.
#
# # Outputs under --outdir
#
#   provenance.json        as soak.sh (source SHA, submodules, toolchain, host,
#                          argv, env knobs), written before the build
#   stress-config.json     the run's declared parameters, written before any launch
#   stress-events.log      timestamped lifecycle events for every step
#   pids/stress.pid        this script's own pid (kill it to abort the run; the
#                          INT/TERM trap kills the current step's processes)
#   sweep/<transport>/<streams|bitrate>/<load>/
#     config.json          the step declaration (`report step`'s StepDeclaration)
#     rss.csv proc.csv host.csv   sampler series, same columns as soak.sh's
#     exits.json           every worker's exit status (+ step_timeout: 124 on timeout)
#     streams/<i>/{leg.txt,send-report.json,recv-report.json,proxy-stats.json,corruption.jsonl}
#     logs/<leg>-<proxy|recv|send>.log, logs/<leg>-proxy.stdout
#     pids/<leg>-<proxy|recv|send>.pid, pids/sampler.pid
#     step-results.json    `report step`'s verdict document
#   stress-FAILED          only on a harness error (names the step and reason)
#   hold/                  the hold, laid out as one step (config.json with
#                          transport "all" / axis "hold", CSVs, exits.json,
#                          streams/<i>/, logs/, pids/) plus:
#     hold-config.json     n_hold, ceilings_declared, cpu_scale_factor (before launch)
#     hold-schedule.json   restart instants and outage-window starts, hold time
#     restart-events.log   `<elapsed_s> RESTART role=<leg>-recv old_pid=N new_pid=N`
#     restart-exits.json   the killed receivers' exit statuses, keyed by elapsed_s
#     hold-results.json    `report hold`'s verdict document
#   stress-results.json    `report stress`: per-axis steps, ceilings, first
#                          failures, the hold
#   summary.txt            the ceilings table and the hold lines
#
# Exit status: 0 = `report stress` overall pass; 1 = a verdict failed, a
# step was refused, or the hold could not be sized; 2 = bad argument,
# bad env knob, colliding hold schedule or harness error.
#
# # Launching
#
# Prerequisites are soak.sh's (see its header): a Rust toolchain, jq,
# python3, build-essential, cmake, meson, ninja-build, clang, libclang-dev.
#
# **Launch this genuinely detached — `nohup ... &` — and NEVER through a
# supervising tool/session mechanism that can enforce its own lifetime
# cap** (soak.sh's header has the incident: a session tool's background
# wrapper killed a run at ~60 min). A full sweep takes hours and the hold
# a day more. Poll for
# `summary.txt` / `stress-results.json`, or `pids/stress.pid` + `ps`:
#
#   SRT_RECONNECT_MODE=background RSS_SLOPE_THRESHOLD_KB_PER_HOUR=1024 \
#     nohup bash scripts/interop/stress.sh --outdir ~/stress-$(date +%F) --seed 1 &
#
# Validated on linux-x86_64 only; see lib.sh's header for this
# directory's shell-portability stance (not macOS-portable).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

# Backtraces in every worker log, managed-reconnect logs visible — same
# reasoning as soak.sh.
export RUST_BACKTRACE=1
export RUST_LOG="${RUST_LOG:-info}"

# Kept verbatim for provenance.json (the parse loop below consumes "$@").
SCRIPT_ARGV=("$@")

die() {
  echo "stress.sh: $*" >&2
  exit 2
}

# ---------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------

OUTDIR=""
SEED=""
TRANSPORTS_RAW="srt,rist,udp,tcp"
# Empty = take the production default, or the --smoke default under
# --smoke (resolved after parsing, so flag order does not matter and an
# explicit flag always wins over --smoke).
STREAM_LADDER_RAW=""
SCALE_LADDER_RAW=""
STEP_WARMUP_S=""
STEP_HOLD_S=""
HOLD_HOURS=""
SKIP_HOLD=0
SMOKE=0
DRY_RUN=0
DRY_RUN_CEILINGS_RAW=""
DRY_RUN_CPU_RAW=""

need_value() { [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die "$1 requires a value"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --outdir) need_value "$@"; OUTDIR=$2; shift 2 ;;
    --seed) need_value "$@"; SEED=$2; shift 2 ;;
    --transports) need_value "$@"; TRANSPORTS_RAW=$2; shift 2 ;;
    --stream-ladder) need_value "$@"; STREAM_LADDER_RAW=$2; shift 2 ;;
    --scale-ladder) need_value "$@"; SCALE_LADDER_RAW=$2; shift 2 ;;
    --step-warmup-s) need_value "$@"; STEP_WARMUP_S=$2; shift 2 ;;
    --step-hold-s) need_value "$@"; STEP_HOLD_S=$2; shift 2 ;;
    --hold-hours) need_value "$@"; HOLD_HOURS=$2; shift 2 ;;
    --skip-hold) SKIP_HOLD=1; shift ;;
    --smoke) SMOKE=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --dry-run-ceilings) need_value "$@"; DRY_RUN_CEILINGS_RAW=$2; shift 2 ;;
    --dry-run-cpu) need_value "$@"; DRY_RUN_CPU_RAW=$2; shift 2 ;;
    -h | --help)
      awk 'NR >= 2 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$OUTDIR" ]] || die "--outdir is required"
[[ -n "$SEED" ]] || die "--seed is required"

if [[ "$SMOKE" -eq 1 ]]; then
  : "${STREAM_LADDER_RAW:=1,2}" "${SCALE_LADDER_RAW:=1,2}"
  : "${STEP_WARMUP_S:=15}" "${STEP_HOLD_S:=60}" "${HOLD_HOURS:=0.1}"
  SAMPLE_CADENCE_S=5
  : "${HOLD_OUTAGE_PERIOD_S:=345}" "${HOLD_OUTAGE_DUR_S:=10}"
  : "${HOLD_RESTART_PERIOD_S:=120}" "${HOLD_RESTART_OFFSET_S:=50}"
else
  : "${STREAM_LADDER_RAW:=1,2,4,8,16,32,64,128}" "${SCALE_LADDER_RAW:=1,2,4,8,16,32,64}"
  : "${STEP_WARMUP_S:=60}" "${STEP_HOLD_S:=600}" "${HOLD_HOURS:=24}"
  SAMPLE_CADENCE_S=30
fi
: "${HOLD_OUTAGE_PERIOD_S:=900}" "${HOLD_OUTAGE_DUR_S:=30}"
: "${HOLD_RESTART_PERIOD_S:=7200}" "${HOLD_RESTART_OFFSET_S:=450}"
: "${HOLD_SCHEDULE_PHASES:=96}"
: "${STRESS_MAX_AGG_MBPS:=2000}" "${CPU_HEADROOM_MAX:=0.80}"
: "${FD_DELTA_MAX:=2}" "${THREAD_DELTA_MAX:=1}"
: "${DELIVERY_SLACK:=0.7}" "${QUEUE_DEPTH_FRACTION:=0.9}"
: "${STRESS_SMOKE_FORCE_FAIL:=0}"

# ---------------------------------------------------------------------
# Validation (identical in --dry-run and a real run)
# ---------------------------------------------------------------------
#
# Integers go through lib.sh's canon_int (no bash arithmetic before the
# bound check — see soak.sh for the octal and 64-bit-wrap hazards).
# Decimals are refused with a leading zero ("00.8") so every declared
# value is also a valid JSON number for `jq --argjson`.

# pos_int <value> <max> <name> -> canonical value, or exit 2.
pos_int() {
  local v
  [[ "$1" =~ ^[0-9]+$ ]] || die "$3 must be a positive integer, got: $1"
  v=$(canon_int "$1" "$2" "$3") || exit 2
  [[ "$v" -ge 1 ]] || die "$3 must be a positive integer, got: $1"
  printf '%s' "$v"
}

# nonneg_int <value> <max> <name> -> canonical value, or exit 2.
nonneg_int() {
  [[ "$1" =~ ^[0-9]+$ ]] || die "$3 must be a non-negative integer, got: $1"
  canon_int "$1" "$2" "$3" || exit 2
}

# pos_decimal <value> <name> [<max>] — exit 2 unless 0 < value (<= max).
pos_decimal() {
  [[ "$1" =~ ^(0|[1-9][0-9]*)(\.[0-9]+)?$ ]] || die "$2 must be a positive number, got: $1"
  awk -v v="$1" -v m="${3:-}" 'BEGIN{exit !(v > 0 && (m == "" || v <= m))}' ||
    die "$2 must be a positive number${3:+ <= $3}, got: $1"
}

# parse_ladder <raw> <max> <flag> <out-array-name> — comma-separated,
# positive, strictly increasing, each <= max.
parse_ladder() {
  local raw=$1 max=$2 flag=$3 v prev=0
  local -n out=$4
  local -a parts
  [[ "$raw" =~ ^[0-9]+(,[0-9]+)*$ ]] || die "$flag must be comma-separated positive integers, got: $raw"
  IFS=',' read -r -a parts <<<"$raw"
  out=()
  for v in "${parts[@]}"; do
    v=$(pos_int "$v" "$max" "$flag") || exit 2
    [[ "$v" -gt "$prev" ]] || die "$flag must be strictly increasing, got: $raw"
    out+=("$v")
    prev=$v
  done
}

# u64 on the Rust side, minus headroom for `SEED + 1 + i` over every stream.
SEED=$(nonneg_int "$SEED" 9223372036854675807 --seed)

TRANSPORTS=()
[[ "$TRANSPORTS_RAW" =~ ^[a-z]+(,[a-z]+)*$ ]] || die "--transports must be a comma-separated list, got: $TRANSPORTS_RAW"
IFS=',' read -r -a _t_parts <<<"$TRANSPORTS_RAW"
for t in "${_t_parts[@]}"; do
  case "$t" in
    srt | rist | udp | tcp) ;;
    *) die "--transports: unknown transport '$t' (known: srt,rist,udp,tcp)" ;;
  esac
  for seen in "${TRANSPORTS[@]}"; do [[ "$seen" != "$t" ]] || die "--transports lists '$t' twice"; done
  TRANSPORTS+=("$t")
done

# Streams: generous cap (each stream is three processes). Scale: the
# receiver's 4 MiB per-PID PES cap divided by the largest realistic
# keyframe (53 248 B) = 78, `fixtures::MAX_AU_SCALE` — `send` refuses
# more, so refuse it here before anything launches.
MAX_AU_SCALE=78
STREAM_LADDER=()
SCALE_LADDER=()
parse_ladder "$STREAM_LADDER_RAW" 4096 --stream-ladder STREAM_LADDER
parse_ladder "$SCALE_LADDER_RAW" "$MAX_AU_SCALE" --scale-ladder SCALE_LADDER

STEP_WARMUP_S=$(pos_int "$STEP_WARMUP_S" 86400 --step-warmup-s)
STEP_HOLD_S=$(pos_int "$STEP_HOLD_S" 86400 --step-hold-s)
# `report step` judges RSS slope only over a span of >= 2 sampler
# cadences inside the hold window and FAILS a step it cannot judge, so a
# shorter hold fails every step by construction. 4 cadences (not 3)
# leaves room for the sampler's own drift.
((STEP_HOLD_S >= 4 * SAMPLE_CADENCE_S)) || die "--step-hold-s $STEP_HOLD_S is shorter than 4 x the \
${SAMPLE_CADENCE_S}s sampler cadence: no step could be judged (RSS slope needs a >= 2-cadence span)"
pos_decimal "$HOLD_HOURS" --hold-hours 1000

# The two required knobs, in this order (the dry-run checks rely on it).
[[ -n "${SRT_RECONNECT_MODE:-}" ]] || die "SRT_RECONNECT_MODE is required (no default) — set it to 'blocking' or \
'background'; it picks how every SRT stream's managed sender spends an outage (see soak.sh's header)"
case "$SRT_RECONNECT_MODE" in
  blocking | background) ;;
  *) die "SRT_RECONNECT_MODE must be blocking|background, got: $SRT_RECONNECT_MODE" ;;
esac
[[ -n "${RSS_SLOPE_THRESHOLD_KB_PER_HOUR:-}" ]] || die "RSS_SLOPE_THRESHOLD_KB_PER_HOUR is required (no default) — \
\`report step\` judges every process's post-warm-up RSS slope against it"
pos_decimal "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" RSS_SLOPE_THRESHOLD_KB_PER_HOUR

pos_decimal "$STRESS_MAX_AGG_MBPS" STRESS_MAX_AGG_MBPS
pos_decimal "$CPU_HEADROOM_MAX" CPU_HEADROOM_MAX 1
pos_decimal "$DELIVERY_SLACK" DELIVERY_SLACK 1
pos_decimal "$QUEUE_DEPTH_FRACTION" QUEUE_DEPTH_FRACTION 1
FD_DELTA_MAX=$(nonneg_int "$FD_DELTA_MAX" 1000000 FD_DELTA_MAX)
THREAD_DELTA_MAX=$(nonneg_int "$THREAD_DELTA_MAX" 1000000 THREAD_DELTA_MAX)
HOLD_OUTAGE_PERIOD_S=$(pos_int "$HOLD_OUTAGE_PERIOD_S" 31536000 HOLD_OUTAGE_PERIOD_S)
HOLD_OUTAGE_DUR_S=$(pos_int "$HOLD_OUTAGE_DUR_S" 31536000 HOLD_OUTAGE_DUR_S)
[[ "$HOLD_OUTAGE_DUR_S" -lt "$HOLD_OUTAGE_PERIOD_S" ]] ||
  die "HOLD_OUTAGE_DUR_S ($HOLD_OUTAGE_DUR_S) must be shorter than HOLD_OUTAGE_PERIOD_S ($HOLD_OUTAGE_PERIOD_S)"
HOLD_RESTART_PERIOD_S=$(pos_int "$HOLD_RESTART_PERIOD_S" 31536000 HOLD_RESTART_PERIOD_S)
HOLD_RESTART_OFFSET_S=$(nonneg_int "$HOLD_RESTART_OFFSET_S" 31536000 HOLD_RESTART_OFFSET_S)
[[ "$HOLD_RESTART_OFFSET_S" -lt "$HOLD_RESTART_PERIOD_S" ]] ||
  die "HOLD_RESTART_OFFSET_S ($HOLD_RESTART_OFFSET_S) must be shorter than HOLD_RESTART_PERIOD_S ($HOLD_RESTART_PERIOD_S)"
HOLD_SCHEDULE_PHASES=$(pos_int "$HOLD_SCHEDULE_PHASES" 4294967295 HOLD_SCHEDULE_PHASES)

case "$STRESS_SMOKE_FORCE_FAIL" in
  0) SMOKE_FORCED_FAIL_STEP="" ;;
  1)
    [[ "$SMOKE" -eq 1 ]] || die "STRESS_SMOKE_FORCE_FAIL=1 is a --smoke knob; refusing it on a real run"
    SMOKE_FORCED_FAIL_STEP="${TRANSPORTS[-1]}/bitrate/${SCALE_LADDER[-1]}"
    ;;
  *) die "STRESS_SMOKE_FORCE_FAIL must be 0 or 1, got: $STRESS_SMOKE_FORCE_FAIL" ;;
esac

for dep in jq python3 awk nproc getconf; do
  have "$dep" || die "required tool '$dep' not found on PATH (see soak.sh's header for the prerequisite list)"
done

# ---------------------------------------------------------------------
# The hold's clock, schedule and collision check — validated here so a
# bad knob is refused before hours of sweep, not after them
# ---------------------------------------------------------------------

# Samples before HOLD_WARMUP_S are start-up noise (`report hold`'s
# warmup_s); the judged window follows it, so every hold process runs
# HOLD_WARMUP_S + HOLD_S seconds (a sweep step's shape).
HOLD_WARMUP_S=60
HOLD_S=$(awk -v h="$HOLD_HOURS" 'BEGIN{printf "%d", h * 3600 + 0.5}')
[[ "$HOLD_S" -ge 1 ]] || die "--hold-hours $HOLD_HOURS rounds to a 0 s hold"
HOLD_RUN_S=$((HOLD_WARMUP_S + HOLD_S))
# The proxy schedule's phase_s is whole seconds and must be >= 1, so
# more phases than hold seconds are clamped (recorded in
# hold-schedule.json and stress-events.log).
HOLD_PHASES=$HOLD_SCHEDULE_PHASES
HOLD_PHASES_CLAMPED=0
if [[ "$HOLD_PHASES" -gt "$HOLD_S" ]]; then
  HOLD_PHASES=$HOLD_S
  HOLD_PHASES_CLAMPED=1
fi
HOLD_PHASE_S=$((HOLD_S / HOLD_PHASES))
# soak.sh's SRT_PROXY_WARMUP_S: an SRT proxy's outage window 0 covers
# its first OUTAGE_DUR_S seconds, so it starts this long before the hold
# clock and window 0 is over before any handshake.
SRT_PROXY_WARMUP_S=$((HOLD_OUTAGE_DUR_S + 30))
# `report hold`'s RESTART_RECOVERY_WINDOW_S: a restart must be followed
# by a fresh sender reconnect within this long, and one later than
# HOLD_S minus it is unjudgeable — so none is scheduled there.
RESTART_RECOVERY_WINDOW_S=120
# Before a restart: one outage's length plus a minute for the sender to
# have come back from it, so a restart never lands on an outage's tail.
RESTART_LEAD_GUARD_S=60

HOLD_HAS_SRT=0
for t in "${TRANSPORTS[@]}"; do [[ "$t" != srt ]] || HOLD_HAS_SRT=1; done

# Outages and restarts exist only with an SRT stream (outages are cut on
# SRT proxies; the restarted receiver is srt-0's). Hold time = seconds
# since the hold clock (START_EPOCH) starts.
RESTART_INSTANTS=()
OUTAGE_STARTS=()
SCHEDULE_ROWS=()
COLLISIONS=()
if [[ "$SKIP_HOLD" -eq 0 && "$HOLD_HAS_SRT" -eq 1 ]]; then
  # Window 0, [-W, -30], ends before START_EPOCH and every handshake: deliberately not checked.
  for ((n = 1; n * HOLD_OUTAGE_PERIOD_S - SRT_PROXY_WARMUP_S < HOLD_RUN_S; n++)); do
    OUTAGE_STARTS+=("$((n * HOLD_OUTAGE_PERIOD_S - SRT_PROXY_WARMUP_S))")
  done
  for ((t = HOLD_RESTART_OFFSET_S; t < HOLD_S && t <= HOLD_S - RESTART_RECOVERY_WINDOW_S; t += HOLD_RESTART_PERIOD_S)); do
    RESTART_INSTANTS+=("$t")
    # Outage n (>= 1) covers [s, s + DUR], s = n x PERIOD - W. It meets
    # the guard band (lo, hi] iff s + DUR > lo and s <= hi: n_lo is the
    # first n past lo, n_hi the last n not past hi.
    lo=$((t - HOLD_OUTAGE_DUR_S - RESTART_LEAD_GUARD_S))
    hi=$((t + RESTART_RECOVERY_WINDOW_S))
    num=$((lo - HOLD_OUTAGE_DUR_S + SRT_PROXY_WARMUP_S))
    if [[ $num -lt 0 ]]; then n_lo=1; else n_lo=$((num / HOLD_OUTAGE_PERIOD_S + 1)); fi
    n_hi=$(((hi + SRT_PROXY_WARMUP_S) / HOLD_OUTAGE_PERIOD_S))
    s=$((n_lo * HOLD_OUTAGE_PERIOD_S - SRT_PROXY_WARMUP_S))
    if [[ $n_lo -le $n_hi ]]; then
      verdict=COLLISION
      COLLISIONS+=("restart at ${t}s (guard band ($lo,$hi]) meets outage window [$s,$((s + HOLD_OUTAGE_DUR_S))]")
    else
      verdict=ok
    fi
    SCHEDULE_ROWS+=("hold restart t=${t}s guard=($lo,$hi] next_outage=[$s,$((s + HOLD_OUTAGE_DUR_S))] $verdict")
  done
fi

# The two test-only --dry-run inputs: `t=V,...`, one per transport.
declare -A DRY_CEIL DRY_CPU
parse_kv() {
  local raw=$1 kind=$2 flag=$3 kv t v
  local -n kv_out=$4
  local -a parts
  [[ "$raw" =~ ^[a-z]+=[0-9.]+(,[a-z]+=[0-9.]+)*$ ]] || die "$flag must be t=V,... (e.g. srt=16,rist=8), got: $raw"
  IFS=',' read -r -a parts <<<"$raw"
  for kv in "${parts[@]}"; do
    t=${kv%%=*}
    v=${kv#*=}
    case "$t" in srt | rist | udp | tcp) ;; *) die "$flag: unknown transport '$t'" ;; esac
    [[ -z "${kv_out[$t]:-}" ]] || die "$flag lists '$t' twice"
    if [[ "$kind" == int ]]; then
      v=$(nonneg_int "$v" 4096 "$flag $t") || exit 2
    else
      [[ "$v" =~ ^(0|[1-9][0-9]*)(\.[0-9]+)?$ ]] || die "$flag $t must be a non-negative number, got: $v"
    fi
    kv_out[$t]=$v
  done
}
if [[ -n "$DRY_RUN_CEILINGS_RAW" || -n "$DRY_RUN_CPU_RAW" ]]; then
  [[ "$DRY_RUN" -eq 1 ]] || die "--dry-run-ceilings/--dry-run-cpu are test-only --dry-run flags; refusing them on a real run"
  [[ "$SKIP_HOLD" -eq 0 ]] || die "--dry-run-ceilings/--dry-run-cpu size the hold; they mean nothing with --skip-hold"
  [[ -n "$DRY_RUN_CEILINGS_RAW" && -n "$DRY_RUN_CPU_RAW" ]] || die "--dry-run-ceilings and --dry-run-cpu go together"
  parse_kv "$DRY_RUN_CEILINGS_RAW" int --dry-run-ceilings DRY_CEIL
  parse_kv "$DRY_RUN_CPU_RAW" dec --dry-run-cpu DRY_CPU
  for t in "${TRANSPORTS[@]}"; do
    [[ -n "${DRY_CPU[$t]:-}" ]] || die "--dry-run-cpu has no value for transport '$t'"
  done
fi

# Hold CPU budget: the sizing keeps the predicted aggregate under this.
HOLD_CPU_BUDGET=0.70

# size_hold <ceilings-assoc> <cpu-fraction-assoc> — the hold sizing rule
# (see the header): sets HOLD_N, HOLD_N_UNSCALED, HOLD_PRED_CPU and
# HOLD_CPU_SCALE_FACTOR. Returns 1, having named it, at the first
# transport whose streams ceiling is unset or 0 (no passing step).
declare -A HOLD_N
HOLD_N_UNSCALED=""
HOLD_PRED_CPU=0
HOLD_CPU_SCALE_FACTOR=1.0
size_hold() {
  local -n sh_ceil=$1 sh_cpu=$2
  local t n terms=""
  HOLD_N=()
  for t in "${TRANSPORTS[@]}"; do
    if [[ -z "${sh_ceil[$t]:-}" || "${sh_ceil[$t]}" -eq 0 ]]; then
      echo "stress: $t has no passing stream step; refusing to size a hold for it" >&2
      return 1
    fi
    n=$((sh_ceil[$t] * 7 / 10))
    [[ $n -ge 1 ]] || n=1
    HOLD_N[$t]=$n
    terms+="$n ${sh_cpu[$t]} "
  done
  HOLD_N_UNSCALED=$(for t in "${TRANSPORTS[@]}"; do printf '%s=%s ' "$t" "${HOLD_N[$t]}"; done)
  HOLD_N_UNSCALED=${HOLD_N_UNSCALED% }
  HOLD_PRED_CPU=$(awk -v terms="$terms" 'BEGIN{k = split(terms, a, " "); s = 0; for (i = 1; i < k; i += 2) s += a[i] * a[i + 1]; printf "%.6g", s}')
  if awk -v p="$HOLD_PRED_CPU" -v b="$HOLD_CPU_BUDGET" 'BEGIN{exit !(p > b)}'; then
    # One rounded factor, both recorded and applied.
    HOLD_CPU_SCALE_FACTOR=$(awk -v p="$HOLD_PRED_CPU" -v b="$HOLD_CPU_BUDGET" 'BEGIN{printf "%.6g", b / p}')
    for t in "${TRANSPORTS[@]}"; do
      HOLD_N[$t]=$(awk -v n="${HOLD_N[$t]}" -v f="$HOLD_CPU_SCALE_FACTOR" 'BEGIN{v = int(n * f); if (v < 1) v = 1; print v}')
    done
  else
    HOLD_CPU_SCALE_FACTOR=1.0
  fi
}

# print_hold_sizing — the sizing, one fact per line (dry-run and summary).
print_hold_sizing() {
  local t line=""
  echo "n_hold $HOLD_N_UNSCALED"
  if [[ "$HOLD_CPU_SCALE_FACTOR" == "1.0" ]]; then
    echo "predicted_cpu $HOLD_PRED_CPU <= $HOLD_CPU_BUDGET -> cpu_scale_factor 1.0"
  else
    echo "predicted_cpu $HOLD_PRED_CPU > $HOLD_CPU_BUDGET -> cpu_scale_factor $(awk -v f="$HOLD_CPU_SCALE_FACTOR" 'BEGIN{printf "%.3f", f}')"
    for t in "${TRANSPORTS[@]}"; do line+="$t=${HOLD_N[$t]} "; done
    echo "n_hold ${line% }"
  fi
}

# print_hold_schedule — the hold's clock and restart table.
print_hold_schedule() {
  echo "hold: hold_s=$HOLD_S warmup_s=$HOLD_WARMUP_S run_s=$HOLD_RUN_S schedule phases=$HOLD_PHASES phase_s=${HOLD_PHASE_S}s$([[ $HOLD_PHASES_CLAMPED -eq 1 ]] && echo " (HOLD_SCHEDULE_PHASES=$HOLD_SCHEDULE_PHASES clamped to hold_s)")"
  if [[ "$HOLD_HAS_SRT" -eq 0 ]]; then
    echo "hold: no srt transport — no outages, no restarts"
    return 0
  fi
  echo "hold: srt outages every ${HOLD_OUTAGE_PERIOD_S}s for ${HOLD_OUTAGE_DUR_S}s, proxies launched ${SRT_PROXY_WARMUP_S}s early; ${#OUTAGE_STARTS[@]} window(s) in the run, first at ${OUTAGE_STARTS[0]:-none}s"
  echo "hold: srt-0 receiver restarts every ${HOLD_RESTART_PERIOD_S}s from ${HOLD_RESTART_OFFSET_S}s; ${#RESTART_INSTANTS[@]} scheduled (none after $((HOLD_S - RESTART_RECOVERY_WINDOW_S))s)"
  [[ ${#SCHEDULE_ROWS[@]} -eq 0 ]] || printf '%s\n' "${SCHEDULE_ROWS[@]}"
  if [[ ${#COLLISIONS[@]} -eq 0 ]]; then echo "hold schedule: no collision"; else echo "hold schedule: COLLISION"; fi
}

# Nominal bitrate of one realistic stream at --au-scale 1.
NOMINAL_MBPS_PER_SCALE=1.7

# step_streams/step_scale <axis> <load>: what one step runs.
step_streams() { if [[ "$1" == "streams" ]]; then echo "$2"; else echo 1; fi; }
step_scale() { if [[ "$1" == "bitrate" ]]; then echo "$2"; else echo 1; fi; }
per_stream_mbps() { awk -v s="$1" -v k="$NOMINAL_MBPS_PER_SCALE" 'BEGIN{print s * k}'; }
agg_mbps() { awk -v n="$1" -v s="$2" -v k="$NOMINAL_MBPS_PER_SCALE" 'BEGIN{print n * s * k}'; }
over_cap() { awk -v a="$1" -v m="$STRESS_MAX_AGG_MBPS" 'BEGIN{exit !(a > m)}'; }

# The step list, and the aggregate-bitrate guard over every step (the top
# rung of each axis is the binding one). A refused step refuses the whole
# run up front rather than hours in.
STEP_LIST=()
REFUSED=()
for t in "${TRANSPORTS[@]}"; do
  for axis in streams bitrate; do
    if [[ "$axis" == "streams" ]]; then ladder=("${STREAM_LADDER[@]}"); else ladder=("${SCALE_LADDER[@]}"); fi
    for load in "${ladder[@]}"; do
      agg=$(agg_mbps "$(step_streams "$axis" "$load")" "$(step_scale "$axis" "$load")")
      STEP_LIST+=("$t $axis $load $agg")
      if over_cap "$agg"; then REFUSED+=("$t/$axis/$load ($agg Mb/s)"); fi
    done
  done
done

if [[ "$DRY_RUN" -eq 1 ]]; then
  printf '%s\n' "${STEP_LIST[@]}"
  [[ "$SKIP_HOLD" -eq 1 ]] || print_hold_schedule
fi
# A colliding hold schedule is a usage error (the knobs are wrong).
if [[ ${#COLLISIONS[@]} -gt 0 ]]; then
  echo "stress.sh: the hold's restart schedule collides with its SRT outage windows (set HOLD_RESTART_OFFSET_S/HOLD_RESTART_PERIOD_S or HOLD_OUTAGE_PERIOD_S/HOLD_OUTAGE_DUR_S apart):" >&2
  printf '  %s\n' "${COLLISIONS[@]}" >&2
  exit 2
fi
if [[ ${#REFUSED[@]} -gt 0 ]]; then
  echo "stress.sh: refusing to run — aggregate nominal bitrate above STRESS_MAX_AGG_MBPS=$STRESS_MAX_AGG_MBPS:" >&2
  printf '  %s\n' "${REFUSED[@]}" >&2
  exit 1
fi
if [[ "$DRY_RUN" -eq 1 ]]; then
  if [[ -n "$DRY_RUN_CEILINGS_RAW" ]]; then
    size_hold DRY_CEIL DRY_CPU || exit 1
    print_hold_sizing
  elif [[ "$SKIP_HOLD" -eq 0 ]]; then
    echo "hold sizing: from the sweep's ceilings (pass --dry-run-ceilings/--dry-run-cpu to preview it)"
  fi
  exit 0
fi

# ---------------------------------------------------------------------
# Run setup: outdir, events, provenance, declaration, build
# ---------------------------------------------------------------------

# A step's CSVs are appended to, so a reused outdir would mix two runs'
# samples in one series. Refuse instead.
[[ ! -e "$OUTDIR/stress-config.json" ]] || die "$OUTDIR already holds a stress run (stress-config.json exists); use a fresh --outdir"
mkdir -p "$OUTDIR/sweep" "$OUTDIR/pids"
OUTDIR="$(cd "$OUTDIR" && pwd)"
EVENTS_LOG="$OUTDIR/stress-events.log"
event() { event_to "$EVENTS_LOG" "$@"; }

# ---------------------------------------------------------------------
# Process bookkeeping and the harness-error path
# ---------------------------------------------------------------------

# The CURRENT step's processes (reset per step), role -> pid.
declare -A PIDS
CURRENT_STEP=""
# Set immediately before every DELIBERATE exit from here on. Any other
# exit — a `set -e` abort on a failed jq/mkdir/free_port, a failed build —
# is a harness error, and the EXIT trap turns it into one.
RUN_DONE=0

# abort_run <reason> — a harness error (not a verdict): kill every process
# of the current step (workers and sampler), record stress-FAILED, exit 2.
# Harness errors never exit 1, which is reserved for a failed verdict or a
# refused step. Reached three ways: called directly, from the INT/TERM trap
# (so killing pids/stress.pid takes the step's workers with it), and from
# the EXIT trap on any unplanned exit.
abort_run() {
  local role last_event
  trap - INT TERM EXIT
  for role in "${!PIDS[@]}"; do kill -9 "${PIDS[$role]}" 2>/dev/null || true; done
  last_event=$(tail -n 1 "$EVENTS_LOG" 2>/dev/null) || last_event=""
  {
    echo "step=${CURRENT_STEP:-none}"
    echo "reason=$1"
    echo "last_event=${last_event:-none}"
    echo "detected_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$OUTDIR/stress-FAILED"
  event "HARNESS-ERROR step=${CURRENT_STEP:-none} $1"
  echo "stress: harness error at step ${CURRENT_STEP:-none}: $1 (see stress-FAILED)" >&2
  exit 2
}
on_exit() {
  local rc=$1
  [[ "$RUN_DONE" -eq 1 ]] || abort_run "unexpected exit rc=$rc"
}
trap 'abort_run "interrupted by signal"' INT TERM
trap 'on_exit $?' EXIT

# Written directly, not through record_pid_to: this pid is not a worker,
# and must never be in PIDS (abort_run kills everything in PIDS).
printf '%s\n' "$$" >"$OUTDIR/pids/.stress.pid.tmp" && mv -f "$OUTDIR/pids/.stress.pid.tmp" "$OUTDIR/pids/stress.pid"
event "START pid=$$ seed=$SEED transports=${TRANSPORTS[*]} smoke=$SMOKE"

VCPUS=$(nproc)
CLK_TCK=$(getconf CLK_TCK)

ARGV_JSON=$(printf '%s\n' "${SCRIPT_ARGV[@]}" | jq -Rn '[inputs | select(length > 0)]')
ENV_JSON=$(jq -n \
  --arg SRT_RECONNECT_MODE "$SRT_RECONNECT_MODE" \
  --arg RSS_SLOPE_THRESHOLD_KB_PER_HOUR "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" \
  --arg STRESS_MAX_AGG_MBPS "$STRESS_MAX_AGG_MBPS" --arg CPU_HEADROOM_MAX "$CPU_HEADROOM_MAX" \
  --arg FD_DELTA_MAX "$FD_DELTA_MAX" --arg THREAD_DELTA_MAX "$THREAD_DELTA_MAX" \
  --arg DELIVERY_SLACK "$DELIVERY_SLACK" --arg QUEUE_DEPTH_FRACTION "$QUEUE_DEPTH_FRACTION" \
  --arg HOLD_OUTAGE_PERIOD_S "$HOLD_OUTAGE_PERIOD_S" --arg HOLD_OUTAGE_DUR_S "$HOLD_OUTAGE_DUR_S" \
  --arg HOLD_RESTART_PERIOD_S "$HOLD_RESTART_PERIOD_S" --arg HOLD_RESTART_OFFSET_S "$HOLD_RESTART_OFFSET_S" \
  --arg HOLD_SCHEDULE_PHASES "$HOLD_SCHEDULE_PHASES" --arg STRESS_SMOKE_FORCE_FAIL "$STRESS_SMOKE_FORCE_FAIL" \
  --arg RUST_LOG "$RUST_LOG" \
  '$ARGS.named')
write_provenance "$REPO_ROOT" "$OUTDIR/provenance.json" "$ARGV_JSON" "$ENV_JSON"

# Fixed per-stream wiring, declared below.
PROFILE=baseline
KLV_SET=rich
CORRUPT_SPEC="rate=5,min_gap=1000"
# Receive-side latency budget for SRT and recovery buffer for RIST, in ms:
# soak.sh's numbers and reasoning (sized for the hold's impairment
# schedule; harmless on the sweep's clean link, and one value for both
# phases keeps the sweep and hold comparable).
SRT_LATENCY_MS=1200
RIST_BUFFER_MS=1200
# soak.sh's SAMPLER_END_SLACK_S: a proxy outlives the sampler's last tick.
PROXY_END_SLACK_S=35
# Hard per-step limit past the step's own run time.
STEP_TIMEOUT_SLACK_S=180
# Settle between the last receiver launch and the first sender launch
# (run-matrix.sh's SETTLE).
SETTLE=2
PROXY_ADDR_POLL_TIMEOUT_S=10
# How often the per-step supervisor checks for its workers to be gone.
SUPERVISE_POLL_S=5

# Declared BEFORE any process launches. `report stress` recomputes the
# ceilings from the step results; this is what the run SAID it would do.
jq -n \
  --argjson seed "$SEED" --argjson smoke "$([[ $SMOKE -eq 1 ]] && echo true || echo false)" \
  --argjson skip_hold "$([[ $SKIP_HOLD -eq 1 ]] && echo true || echo false)" \
  --argjson transports "$(printf '%s\n' "${TRANSPORTS[@]}" | jq -Rn '[inputs]')" \
  --argjson stream_ladder "$(printf '%s\n' "${STREAM_LADDER[@]}" | jq -n '[inputs]')" \
  --argjson scale_ladder "$(printf '%s\n' "${SCALE_LADDER[@]}" | jq -n '[inputs]')" \
  --argjson step_warmup_s "$STEP_WARMUP_S" --argjson step_hold_s "$STEP_HOLD_S" \
  --argjson sample_cadence_s "$SAMPLE_CADENCE_S" --argjson hold_hours "$HOLD_HOURS" \
  --argjson max_agg_mbps "$STRESS_MAX_AGG_MBPS" \
  --argjson nominal_mbps_per_scale "$NOMINAL_MBPS_PER_SCALE" \
  --arg profile "$PROFILE" --arg klv_set "$KLV_SET" --arg corrupt_spec "$CORRUPT_SPEC" \
  --arg srt_reconnect_mode "$SRT_RECONNECT_MODE" \
  --argjson srt_latency_ms "$SRT_LATENCY_MS" --argjson rist_buffer_ms "$RIST_BUFFER_MS" \
  --argjson rss "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" --argjson cpu "$CPU_HEADROOM_MAX" \
  --argjson fd "$FD_DELTA_MAX" --argjson thr "$THREAD_DELTA_MAX" \
  --argjson slack "$DELIVERY_SLACK" --argjson qdf "$QUEUE_DEPTH_FRACTION" \
  --argjson hop "$HOLD_OUTAGE_PERIOD_S" --argjson hod "$HOLD_OUTAGE_DUR_S" \
  --argjson hrp "$HOLD_RESTART_PERIOD_S" --argjson hro "$HOLD_RESTART_OFFSET_S" \
  --argjson hsp "$HOLD_SCHEDULE_PHASES" \
  --arg forced "$SMOKE_FORCED_FAIL_STEP" \
  '{seed: $seed, smoke: $smoke, skip_hold: $skip_hold, transports: $transports,
    stream_ladder: $stream_ladder, scale_ladder: $scale_ladder,
    step_warmup_s: $step_warmup_s, step_hold_s: $step_hold_s,
    sample_cadence_s: $sample_cadence_s, hold_hours: $hold_hours,
    max_agg_mbps: $max_agg_mbps, nominal_mbps_per_scale: $nominal_mbps_per_scale,
    profile: $profile, klv_set: $klv_set, au_sizes: "realistic", corrupt_spec: $corrupt_spec,
    srt_reconnect_mode: $srt_reconnect_mode,
    srt_latency_ms: $srt_latency_ms, rist_buffer_ms: $rist_buffer_ms,
    thresholds: {rss_slope_kb_per_hour: $rss, cpu_headroom_max: $cpu,
                 fd_delta_max: $fd, thread_delta_max: $thr,
                 delivery_slack: $slack, queue_depth_fraction: $qdf},
    hold: {outage_period_s: $hop, outage_dur_s: $hod, restart_period_s: $hrp,
           restart_offset_s: $hro, schedule_phases: $hsp},
    smoke_forced_fail_step: (if $forced == "" then null else $forced end)}' \
  >"$OUTDIR/stress-config.json"

echo "stress: building tst-interop (release)..." >&2
(cd "$REPO_ROOT" && SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop)
BIN="$REPO_ROOT/target/release/tst-interop"


# ---------------------------------------------------------------------
# One stream
# ---------------------------------------------------------------------
#
# launch_stream <phase> <transport> <idx> <leg> <stream_dir> <seconds> <scale> <managed:0|1> <outage_spec|->
#
# One function for the whole per-transport wiring table, called in TWO
# phases over all of a step's streams: `listen` (receiver, then its
# proxy) for every stream, one SETTLE, then `send` for every stream.
# Launching triple by triple with a settle each would stagger a 128-stream
# step by minutes, so the first streams' fixed `--seconds` windows would
# end long before the last ones started. Two phases keep every stream's
# window inside a few seconds of the step's start, and every receiver
# bound before any sender starts.
#
# The step's pids/ and logs/ are the stream dir's grandparent's. The
# proxy's impairment flags come from PROXY_IMPAIR_ARGS (the sweep sets a
# clean link; the hold sets each stream's seeded schedule). <outage_spec>
# is applied to SRT proxies only, and is `-` throughout the sweep.
#
# The hold launches the same pieces in another order (proxy, recv, send
# per stream, SRT proxies earlier still), so `listen` is just `recv` then
# `proxy`, and the stream's port is taken by whichever runs first. Three
# hold-only knobs, all neutral for the sweep: PROXY_SEED (empty = SEED,
# the sweep's one proxy seed), PROXY_EXTRA_S (added to the proxy's
# --run-seconds: an SRT proxy launched early must also end later). The
# receiver log is always opened for append: a step's log is new, and a
# restarted receiver continues its predecessor's.
declare -A STREAM_PORT
declare -A USED_PORTS
PROXY_IMPAIR_ARGS=()
PROXY_SEED=""
PROXY_EXTRA_S=0

# stream_port <transport> <leg> — the leg's port, allocated on first use.
# Re-probes while the candidate is already taken by an earlier stream of
# this step: those receivers may not have bound yet, so the kernel can
# hand the same free port out twice.
stream_port() {
  local transport=$1 leg=$2 port=""
  [[ -z "${STREAM_PORT[$leg]:-}" ]] || return 0
  until [[ -n "$port" && -z "${USED_PORTS[$port]:-}" ]]; do
    if [[ "$transport" == "tcp" ]]; then port=$(free_port tcp); else port=$(free_port udp); fi
  done
  USED_PORTS[$port]=1
  STREAM_PORT[$leg]=$port
}

launch_stream() {
  local phase=$1 transport=$2 idx=$3 leg=$4 stream_dir=$5 seconds=$6 scale=$7 managed=$8 outage=$9
  local step_dir=${stream_dir%/streams/*}
  local logs=$step_dir/logs pids=$step_dir/pids
  local port addr recv_url send_url
  local -a managed_recv=() managed_send=() outage_args=()

  if [[ "$managed" -eq 1 ]]; then
    managed_recv=(--managed)
    managed_send=(--managed --reconnect-mode "$SRT_RECONNECT_MODE")
  fi

  case "$phase" in
    listen)
      launch_stream recv "${@:2}"
      launch_stream proxy "${@:2}"
      ;;
    recv)
      stream_port "$transport" "$leg"
      port=${STREAM_PORT[$leg]}
      case "$transport" in
        srt) recv_url="srt://:$port?mode=listener&latency=$SRT_LATENCY_MS" ;;
        rist) recv_url="rist://@0.0.0.0:$port?buffer=$RIST_BUFFER_MS" ;;
        udp) recv_url="udp://127.0.0.1:$port" ;;
        tcp) recv_url="tcp://127.0.0.1:$port?listen=1" ;;
      esac
      "$BIN" recv --url "$recv_url" --expect "$PROFILE" --seconds "$seconds" \
        --json "$stream_dir/recv-report.json" --no-klv-digest \
        --klv-set "$KLV_SET" --klv-seed "$((SEED + idx))" \
        --corruption-log "$stream_dir/corruption.jsonl" "${managed_recv[@]}" \
        >>"$logs/$leg-recv.log" 2>&1 &
      record_pid_to "$pids" "$EVENTS_LOG" "$leg-recv" "$!"
      ;;
    proxy)
      [[ "$transport" != "tcp" ]] || return 0
      stream_port "$transport" "$leg"
      port=${STREAM_PORT[$leg]}
      if [[ "$outage" != "-" && "$transport" == "srt" ]]; then
        outage_args=(--outage "$outage")
      fi
      "$BIN" proxy --listen 127.0.0.1:0 --forward "127.0.0.1:$port" \
        "${PROXY_IMPAIR_ARGS[@]}" --seed "${PROXY_SEED:-$SEED}" "${outage_args[@]}" \
        --stats-json "$stream_dir/proxy-stats.json" \
        --run-seconds "$((seconds + PROXY_EXTRA_S + PROXY_END_SLACK_S))" \
        >"$logs/$leg-proxy.stdout" 2>"$logs/$leg-proxy.log" &
      record_pid_to "$pids" "$EVENTS_LOG" "$leg-proxy" "$!"
      ;;
    send)
      port=${STREAM_PORT[$leg]}
      if [[ "$transport" == "tcp" ]]; then
        addr="127.0.0.1:$port"
      else
        addr=$(wait_for_bound_addr "$logs/$leg-proxy.stdout" "$PROXY_ADDR_POLL_TIMEOUT_S" stress) ||
          abort_run "$leg proxy never reported its bound address (see $logs/$leg-proxy.stdout)"
      fi
      case "$transport" in
        srt) send_url="srt://$addr?latency=$SRT_LATENCY_MS" ;;
        rist) send_url="rist://$addr?buffer=$RIST_BUFFER_MS" ;;
        udp) send_url="udp://$addr" ;;
        tcp) send_url="tcp://$addr" ;;
      esac
      "$BIN" send --profile "$PROFILE" --url "$send_url" --seconds "$seconds" \
        --json "$stream_dir/send-report.json" --no-klv-digest \
        --au-sizes realistic --au-scale "$scale" \
        --klv-set "$KLV_SET" --klv-seed "$((SEED + idx))" \
        --corrupt "$CORRUPT_SPEC" --corruption-log "$stream_dir/corruption.jsonl" \
        --seed "$((SEED + 1 + idx))" "${managed_send[@]}" \
        >"$logs/$leg-send.log" 2>&1 &
      record_pid_to "$pids" "$EVENTS_LOG" "$leg-send" "$!"
      ;;
    *) abort_run "launch_stream: unknown phase '$phase'" ;;
  esac
}

# ---------------------------------------------------------------------
# One step
# ---------------------------------------------------------------------

# workers_alive <role>... — true while any named role's pid still runs.
workers_alive() {
  local role
  for role in "$@"; do
    if kill -0 "${PIDS[$role]}" 2>/dev/null; then return 0; fi
  done
  return 1
}

# thresholds_json <cpu_headroom_max> — `report step`'s StepThresholds as
# this run passes them.
thresholds_json() {
  jq -n --argjson rss "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" --argjson fd "$FD_DELTA_MAX" \
    --argjson thr "$THREAD_DELTA_MAX" --argjson cpu "$1" \
    --argjson slack "$DELIVERY_SLACK" --argjson qdf "$QUEUE_DEPTH_FRACTION" \
    '{rss_slope_kb_per_hour: $rss, fd_delta_max: $fd, thread_delta_max: $thr,
      cpu_headroom_max: $cpu, delivery_slack: $slack, queue_depth_fraction: $qdf}'
}

# fallback_step_results <dir> <cpu_headroom_max> <failing>... — print a
# FAIL verdict document for a step (or the hold) `report step` could not
# judge. It has the full StepResults shape, empty where nothing was
# judged, because `report stress` deserializes every step-results.json
# (and the hold's `step`) strictly.
fallback_step_results() {
  local dir=$1 cpu_max=$2
  shift 2
  jq -n --slurpfile decl "$dir/config.json" \
    --argjson thresholds "$(thresholds_json "$cpu_max")" \
    --argjson failing "$(printf '%s\n' "$@" | jq -Rn '[inputs]')" \
    '{decl: $decl[0], thresholds: $thresholds, pass: false, verdicts: [],
      failing: $failing, per_stream: [], aggregate_cpu_fraction: 0,
      cpu_fraction_per_stream: 0, aggregate_wire_mbps: 0, samples_used: 0}'
}

# any_nonzero_exit <exits.json> — true when a worker exited nonzero (or
# the file records a timeout): the box was overloaded, so a `report`
# that cannot judge the evidence is a FAIL, not a harness error.
any_nonzero_exit() { jq -e 'any(.[]; . != 0)' "$1" >/dev/null 2>&1; }

# run_step <transport> <axis> <load> — sets STEP_RC (0 pass / 1 fail);
# a harness error never returns (abort_run). Called plainly, not as
# `run_step || rc=$?`, so `set -e` stays in force inside it.
STEP_RC=0
run_step() {
  local transport=$1 axis=$2 load=$3
  local step_dir="$OUTDIR/sweep/$transport/$axis/$load"
  local streams scale nominal agg managed seconds start i leg role rc timed_out
  local cpu_max launch_end
  local -a roles=()
  CURRENT_STEP="$transport/$axis/$load"
  STEP_RC=1

  streams=$(step_streams "$axis" "$load")
  scale=$(step_scale "$axis" "$load")
  nominal=$(per_stream_mbps "$scale")
  agg=$(agg_mbps "$streams" "$scale")
  # Belt and braces: the pre-flight already refused any such step.
  if over_cap "$agg"; then
    event "REFUSED step=$CURRENT_STEP agg_mbps=$agg max=$STRESS_MAX_AGG_MBPS"
    echo "stress: refusing $CURRENT_STEP: $agg Mb/s > STRESS_MAX_AGG_MBPS=$STRESS_MAX_AGG_MBPS" >&2
    RUN_DONE=1
    exit 1
  fi
  managed=0
  [[ "$transport" != "srt" ]] || managed=1

  mkdir -p "$step_dir/streams" "$step_dir/logs" "$step_dir/pids"
  jq -n --arg transport "$transport" --arg axis "$axis" \
    --argjson streams "$streams" --argjson au_scale "$scale" \
    --argjson warmup_s "$STEP_WARMUP_S" --argjson hold_s "$STEP_HOLD_S" \
    --argjson vcpus "$VCPUS" --argjson clk_tck "$CLK_TCK" \
    --argjson sample_cadence_s "$SAMPLE_CADENCE_S" --argjson nominal "$nominal" \
    --argjson managed "$([[ $managed -eq 1 ]] && echo true || echo false)" \
    '{transport: $transport, axis: $axis, streams: $streams, au_scale: $au_scale,
      warmup_s: $warmup_s, hold_s: $hold_s, vcpus: $vcpus, clk_tck: $clk_tck,
      sample_cadence_s: $sample_cadence_s, nominal_mbps_per_stream: $nominal,
      managed: $managed, outage_period_s: null, outage_dur_s: null,
      restart_period_s: null}' >"$step_dir/config.json"
  printf 'elapsed_s,leg,process,pid,rss_kb\n' >"$step_dir/rss.csv"
  printf 'elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n' >"$step_dir/proc.csv"
  printf 'elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n' >"$step_dir/host.csv"

  PIDS=()
  STREAM_PORT=()
  USED_PORTS=()
  PROXY_IMPAIR_ARGS=(--loss 0 --jitter 0 --delay 0 --reorder 0,0)
  seconds=$((STEP_WARMUP_S + STEP_HOLD_S))
  event "STEP-START step=$CURRENT_STEP streams=$streams au_scale=$scale agg_mbps=$agg seconds=$seconds"
  echo "stress: step $CURRENT_STEP — $streams stream(s) at --au-scale $scale (~$agg Mb/s), ${seconds}s..." >&2

  # The step clock starts before the first receiver launches. A receiver's
  # window starts at its first event and a sender's at its own launch, both
  # after this instant, so every worker is still alive at start + seconds
  # and the sampler's last tick never lands on a process that ended on
  # schedule (soak.sh needed a 35s slack for the opposite ordering).
  start=$(date +%s)
  for ((i = 0; i < streams; i++)); do
    leg="$transport-$i"
    mkdir -p "$step_dir/streams/$i"
    printf '%s\n' "$leg" >"$step_dir/streams/$i/leg.txt"
    launch_stream listen "$transport" "$i" "$leg" "$step_dir/streams/$i" "$seconds" "$scale" "$managed" -
    roles+=("$leg-recv")
    [[ "$transport" == "tcp" ]] || roles+=("$leg-proxy")
  done
  sleep "$SETTLE"
  for ((i = 0; i < streams; i++)); do
    leg="$transport-$i"
    launch_stream send "$transport" "$i" "$leg" "$step_dir/streams/$i" "$seconds" "$scale" "$managed" -
    roles+=("$leg-send")
  done
  launch_end=$(date +%s)
  event "LAUNCH-SPAN step=$CURRENT_STEP seconds=$((launch_end - start)) processes=${#roles[@]}"

  sample_loop "$((start + seconds))" "$start" "$step_dir/rss.csv" "$step_dir/proc.csv" \
    "$step_dir/host.csv" "$step_dir/pids" "$SAMPLE_CADENCE_S" sampler &
  record_pid_to "$step_dir/pids" "$EVENTS_LOG" sampler "$!"

  # Supervise: every worker ends on its own `--seconds`/`--run-seconds`;
  # the hard limit catches a wedged one.
  timed_out=0
  until ! workers_alive "${roles[@]}" || [[ $(date +%s) -ge $((start + seconds + STEP_TIMEOUT_SLACK_S)) ]]; do
    sleep "$SUPERVISE_POLL_S"
  done
  if workers_alive "${roles[@]}"; then
    timed_out=1
    event "STEP-TIMEOUT step=$CURRENT_STEP limit_s=$((seconds + STEP_TIMEOUT_SLACK_S)) — killing every process"
    echo "stress: $CURRENT_STEP exceeded its ${STEP_TIMEOUT_SLACK_S}s grace — killing it" >&2
    for role in "${roles[@]}"; do kill -9 "${PIDS[$role]}" 2>/dev/null || true; done
  fi

  # Reap every worker (soak.sh's exits.json shape), then stop the sampler.
  local -a exit_pairs=()
  for role in "${roles[@]}"; do
    rc=0
    wait "${PIDS[$role]}" || rc=$?
    exit_pairs+=("$role" "$rc")
    [[ $rc -eq 0 ]] || event "EXIT-NONZERO step=$CURRENT_STEP role=$role pid=${PIDS[$role]} status=$rc"
  done
  [[ $timed_out -eq 0 ]] || exit_pairs+=(step_timeout 124)
  printf '%s\n' "${exit_pairs[@]}" |
    jq -Rn '[inputs] | [range(0; length; 2) as $i | {key: .[$i], value: (.[$i + 1] | tonumber)}] | from_entries' \
      >"$step_dir/exits.json"
  kill "${PIDS[sampler]}" 2>/dev/null || true
  wait "${PIDS[sampler]}" 2>/dev/null || true

  cpu_max=$CPU_HEADROOM_MAX
  if [[ "$CURRENT_STEP" == "$SMOKE_FORCED_FAIL_STEP" ]]; then
    cpu_max=0.000001
    event "FORCED-FAIL step=$CURRENT_STEP cpu_headroom_max=$cpu_max (STRESS_SMOKE_FORCE_FAIL=1)"
  fi
  rc=0
  "$BIN" report step --dir "$step_dir" \
    --rss-slope-threshold-kb-per-hour "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" \
    --fd-delta-max "$FD_DELTA_MAX" --thread-delta-max "$THREAD_DELTA_MAX" \
    --cpu-headroom-max "$cpu_max" --delivery-slack "$DELIVERY_SLACK" \
    --queue-depth-fraction "$QUEUE_DEPTH_FRACTION" || rc=$?

  if [[ $rc -eq 2 ]] && any_nonzero_exit "$step_dir/exits.json"; then
    # Overload rule: the evidence is unjudgeable BECAUSE a worker died or
    # the step timed out — a FAIL rung that stops the axis.
    local -a failing=(step_unjudgeable)
    [[ $timed_out -eq 0 ]] || failing=(step_timeout step_unjudgeable)
    event "STEP-UNJUDGEABLE step=$CURRENT_STEP report step exited 2 with a nonzero worker exit or a timeout — FAIL"
    fallback_step_results "$step_dir" "$cpu_max" "${failing[@]}" >"$step_dir/.step-results.json.tmp"
    mv -f "$step_dir/.step-results.json.tmp" "$step_dir/step-results.json"
    rc=1
  elif [[ $timed_out -eq 1 ]]; then
    # A timed-out step FAILs whatever the report made of its partial evidence.
    rc=1
    if [[ -s "$step_dir/step-results.json" ]]; then
      jq '.pass = false | .failing = ((.failing // []) | if any(.[]; . == "step_timeout") then . else . + ["step_timeout"] end)' \
        "$step_dir/step-results.json" >"$step_dir/.step-results.json.tmp"
      mv -f "$step_dir/.step-results.json.tmp" "$step_dir/step-results.json"
    else
      fallback_step_results "$step_dir" "$cpu_max" step_timeout >"$step_dir/.step-results.json.tmp"
      mv -f "$step_dir/.step-results.json.tmp" "$step_dir/step-results.json"
    fi
  fi
  PIDS=()
  case "$rc" in
    0 | 1) STEP_RC=$rc ;;
    *) abort_run "report step exited $rc on $step_dir (harness error, not a verdict)" ;;
  esac
}

# sweep_axis <transport> <streams|bitrate> <load>... — run the ladder in
# order and stop at its first failing step. LAST_PASS is bash's own record
# for the hold's sizing (Task 12); `report stress` recomputes the
# authoritative ceiling from the step results.
declare -A LAST_PASS
sweep_axis() {
  local transport=$1 axis=$2 load
  shift 2
  for load in "$@"; do
    run_step "$transport" "$axis" "$load"
    if [[ $STEP_RC -eq 0 ]]; then
      LAST_PASS[$transport/$axis]=$load
      event "STEP-PASS step=$transport/$axis/$load"
    else
      event "STEP-FAIL step=$transport/$axis/$load failing=$(jq -c '.failing // []' "$OUTDIR/sweep/$transport/$axis/$load/step-results.json" 2>/dev/null || echo unknown)"
      echo "stress: $transport/$axis failed at $load — axis stops here" >&2
      break
    fi
  done
}

# ---------------------------------------------------------------------
# The hold
# ---------------------------------------------------------------------

# Supervisor poll (it also wakes exactly at each restart instant).
if [[ "$SMOKE" -eq 1 ]]; then HOLD_POLL_S=5; else HOLD_POLL_S=30; fi
# A restarted receiver gets this long to exit on SIGTERM before SIGKILL.
RESTART_TERM_GRACE_S=30
# Heartbeat clocks are sender-relative, so every sender should start
# within a few seconds of START_EPOCH; a launch span this long or longer
# is logged as a WARNING.
HOLD_LAUNCH_SPAN_WARN_S=30
# Results the summary reads. HOLD_REFUSED=1: a transport had no passing
# streams step, so no hold ran and the run exits 1.
HOLD_REFUSED=0
HOLD_RESTARTS_DONE=0
HOLD_REPORT_RC=""
START_EPOCH=0
HOLD_DEADLINE=0
SRT0_IDX=""
RESTART_EXIT_PAIRS=()

# hold_ceiling <transport> — the highest streams load whose
# step-results.json passed, or empty.
hold_ceiling() {
  local d load best=""
  for d in "$OUTDIR/sweep/$1/streams"/*/; do
    [[ -d "$d" ]] || continue
    load=$(basename "$d")
    [[ "$load" =~ ^[0-9]+$ ]] || continue
    if jq -e '.pass == true' "$d/step-results.json" >/dev/null 2>&1; then
      if [[ -z "$best" || "$load" -gt "$best" ]]; then best=$load; fi
    fi
  done
  printf '%s' "$best"
}

# assoc_json <assoc-name> — {"<t>": N, ...} over TRANSPORTS, in order.
assoc_json() {
  local -n aj=$1
  local t
  for t in "${TRANSPORTS[@]}"; do printf '%s\t%s\n' "$t" "${aj[$t]}"; done |
    jq -Rn '[inputs | split("\t") | {key: .[0], value: (.[1] | tonumber)}] | from_entries'
}

# hold_proxy_args <i> — stream i's seeded impairment schedule and seed.
hold_proxy_args() {
  PROXY_IMPAIR_ARGS=(--schedule "seed=$((SEED + $1)),phases=$HOLD_PHASES,phase_s=${HOLD_PHASE_S}s")
  PROXY_SEED=$((SEED + $1))
}

# restart_srt0 <hold_dir> — kill srt-0's receiver and relaunch it on the
# same port, report path and (appended) log, for the rest of the hold.
restart_srt0() {
  local hold_dir=$1 old new now elapsed rc=0 waited=0
  old=${PIDS[srt-0-recv]}
  now=$(date +%s)
  elapsed=$((now - START_EPOCH))
  kill -TERM "$old" 2>/dev/null || true
  until ! kill -0 "$old" 2>/dev/null || [[ $waited -ge $RESTART_TERM_GRACE_S ]]; do
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "$old" 2>/dev/null; then
    event "RESTART-SIGKILL role=srt-0-recv pid=$old (no exit ${RESTART_TERM_GRACE_S}s after SIGTERM)"
    kill -9 "$old" 2>/dev/null || true
  fi
  wait "$old" || rc=$?
  RESTART_EXIT_PAIRS+=("$elapsed" "$rc")
  printf '%s\n' "${RESTART_EXIT_PAIRS[@]}" |
    jq -Rn '[inputs] | [range(0; length; 2) as $i | {key: .[$i], value: (.[$i + 1] | tonumber)}] | from_entries' \
      >"$hold_dir/.restart-exits.json.tmp"
  mv -f "$hold_dir/.restart-exits.json.tmp" "$hold_dir/restart-exits.json"
  now=$(date +%s)
  launch_stream recv srt "$SRT0_IDX" srt-0 "$hold_dir/streams/$SRT0_IDX" "$((HOLD_DEADLINE - now))" 1 1 -
  new=${PIDS[srt-0-recv]}
  printf '%s RESTART role=srt-0-recv old_pid=%s new_pid=%s\n' "$elapsed" "$old" "$new" >>"$hold_dir/restart-events.log"
  event "RESTART role=srt-0-recv old_pid=$old old_status=$rc new_pid=$new elapsed_s=$elapsed"
  HOLD_RESTARTS_DONE=$((HOLD_RESTARTS_DONE + 1))
}

# run_hold — size the hold from the sweep, run it, judge it with `report
# hold`. Sets HOLD_REFUSED or HOLD_REPORT_RC; a harness error never
# returns (abort_run).
run_hold() {
  local hold_dir="$OUTDIR/hold"
  local t k i n ceil last cpu leg managed total now next r_at r_idx=0 role rc timed_out launch_end
  local outage="period=${HOLD_OUTAGE_PERIOD_S}s,dur=${HOLD_OUTAGE_DUR_S}s"
  local -A ceil_map=() cpu_map=()
  local -a s_t=() s_leg=() roles=() failing=()
  CURRENT_STEP=hold

  # Sizing. The ceiling is read from the step results (what `report
  # stress` will recompute); bash's LAST_PASS is the cross-check.
  for t in "${TRANSPORTS[@]}"; do
    ceil=$(hold_ceiling "$t")
    last=${LAST_PASS[$t/streams]:-}
    if [[ "$ceil" != "$last" ]]; then
      event "WARNING hold sizing: $t streams ceiling is '${ceil:-none}' from step-results.json but '${last:-none}' from this script's sweep — using step-results.json"
    fi
    ceil_map[$t]=${ceil:-0}
    [[ -n "$ceil" ]] || continue
    cpu=$(jq -r '.cpu_fraction_per_stream' "$OUTDIR/sweep/$t/streams/$ceil/step-results.json")
    [[ "$cpu" =~ ^[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$ ]] ||
      abort_run "sweep/$t/streams/$ceil/step-results.json: cpu_fraction_per_stream is not a number: $cpu"
    cpu_map[$t]=$cpu
  done
  if ! size_hold ceil_map cpu_map; then
    event "HOLD-REFUSED no passing streams step for a transport (see stderr)"
    HOLD_REFUSED=1
    CURRENT_STEP=""
    return 0
  fi

  for t in "${TRANSPORTS[@]}"; do
    for ((k = 0; k < HOLD_N[$t]; k++)); do
      s_t+=("$t")
      s_leg+=("$t-$k")
    done
  done
  total=${#s_t[@]}
  for ((i = 0; i < total; i++)); do [[ "${s_leg[$i]}" != srt-0 ]] || SRT0_IDX=$i; done

  mkdir -p "$hold_dir/streams" "$hold_dir/logs" "$hold_dir/pids"
  # Exactly the three keys `report hold` reads (HoldDeclaration).
  jq -n --argjson n "$(assoc_json HOLD_N)" --argjson c "$(assoc_json ceil_map)" \
    --argjson f "$HOLD_CPU_SCALE_FACTOR" \
    '{n_hold: $n, ceilings_declared: $c, cpu_scale_factor: $f}' >"$hold_dir/hold-config.json"
  local srt_json restart_json
  srt_json=$([[ $HOLD_HAS_SRT -eq 1 ]] && echo true || echo false)
  restart_json=$([[ ${#RESTART_INSTANTS[@]} -gt 0 ]] && echo true || echo false)
  jq -n --argjson streams "$total" --argjson warmup_s "$HOLD_WARMUP_S" --argjson hold_s "$HOLD_S" \
    --argjson vcpus "$VCPUS" --argjson clk_tck "$CLK_TCK" \
    --argjson sample_cadence_s "$SAMPLE_CADENCE_S" --argjson nominal "$(per_stream_mbps 1)" \
    --argjson srt "$srt_json" --argjson restarts "$restart_json" \
    --argjson hop "$HOLD_OUTAGE_PERIOD_S" --argjson hod "$HOLD_OUTAGE_DUR_S" \
    --argjson hrp "$HOLD_RESTART_PERIOD_S" \
    '{transport: "all", axis: "hold", streams: $streams, au_scale: 1,
      warmup_s: $warmup_s, hold_s: $hold_s, vcpus: $vcpus, clk_tck: $clk_tck,
      sample_cadence_s: $sample_cadence_s, nominal_mbps_per_stream: $nominal,
      managed: true,
      outage_period_s: (if $srt then $hop else null end),
      outage_dur_s: (if $srt then $hod else null end),
      restart_period_s: (if $restarts then $hrp else null end)}' >"$hold_dir/config.json"
  jq -n --argjson restarts "$(printf '%s\n' "${RESTART_INSTANTS[@]}" | jq -n '[inputs]')" \
    --argjson outages "$(printf '%s\n' "${OUTAGE_STARTS[@]}" | jq -n '[inputs]')" \
    --argjson warmup_s "$HOLD_WARMUP_S" --argjson hold_s "$HOLD_S" --argjson run_s "$HOLD_RUN_S" \
    --argjson hop "$HOLD_OUTAGE_PERIOD_S" --argjson hod "$HOLD_OUTAGE_DUR_S" \
    --argjson hrp "$HOLD_RESTART_PERIOD_S" --argjson hro "$HOLD_RESTART_OFFSET_S" \
    --argjson pw "$SRT_PROXY_WARMUP_S" --argjson phases "$HOLD_PHASES" \
    --argjson phases_declared "$HOLD_SCHEDULE_PHASES" --argjson phase_s "$HOLD_PHASE_S" \
    --argjson before "$((HOLD_OUTAGE_DUR_S + RESTART_LEAD_GUARD_S))" \
    --argjson after "$RESTART_RECOVERY_WINDOW_S" \
    '{clock: "seconds since the hold START_EPOCH", warmup_s: $warmup_s, hold_s: $hold_s,
      run_s: $run_s, restart_offset_s: $hro, restart_period_s: $hrp,
      restart_instants_s: $restarts, restart_guard_s: {before: $before, after: $after},
      outage_period_s: $hop, outage_dur_s: $hod, srt_proxy_warmup_s: $pw,
      outage_starts_s: $outages, schedule_phases: $phases,
      schedule_phases_declared: $phases_declared, schedule_phase_s: $phase_s}' \
    >"$hold_dir/hold-schedule.json"
  [[ $HOLD_PHASES_CLAMPED -eq 0 ]] ||
    event "WARNING hold: HOLD_SCHEDULE_PHASES=$HOLD_SCHEDULE_PHASES clamped to $HOLD_PHASES (one phase per second of a ${HOLD_S}s hold)"
  : >"$hold_dir/restart-events.log"
  printf '{}\n' >"$hold_dir/restart-exits.json"
  printf 'elapsed_s,leg,process,pid,rss_kb\n' >"$hold_dir/rss.csv"
  printf 'elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n' >"$hold_dir/proc.csv"
  printf 'elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n' >"$hold_dir/host.csv"
  for ((i = 0; i < total; i++)); do
    mkdir -p "$hold_dir/streams/$i"
    printf '%s\n' "${s_leg[$i]}" >"$hold_dir/streams/$i/leg.txt"
  done

  PIDS=()
  STREAM_PORT=()
  USED_PORTS=()
  event "HOLD-START streams=$total n_hold=$(assoc_json HOLD_N | jq -c .) cpu_scale_factor=$HOLD_CPU_SCALE_FACTOR hold_s=$HOLD_S run_s=$HOLD_RUN_S restarts=${#RESTART_INSTANTS[@]}"
  echo "stress: hold — $total stream(s) for ${HOLD_RUN_S}s (${HOLD_WARMUP_S}s warm-up + ${HOLD_S}s)..." >&2

  # SRT proxies first, SRT_PROXY_WARMUP_S ahead of the hold clock, so
  # their outage window 0 is over before any handshake (soak.sh). They
  # run that much longer too, ending with every other proxy.
  if [[ $HOLD_HAS_SRT -eq 1 ]]; then
    PROXY_EXTRA_S=$SRT_PROXY_WARMUP_S
    for ((i = 0; i < total; i++)); do
      [[ "${s_t[$i]}" == srt ]] || continue
      hold_proxy_args "$i"
      launch_stream proxy srt "$i" "${s_leg[$i]}" "$hold_dir/streams/$i" "$HOLD_RUN_S" 1 1 "$outage"
      roles+=("${s_leg[$i]}-proxy")
    done
    PROXY_EXTRA_S=0
    event "SRT-PROXY-WARMUP seconds=$SRT_PROXY_WARMUP_S"
    sleep "$SRT_PROXY_WARMUP_S"
  fi

  # The hold clock. Every stream is a triple, proxy -> recv -> send (the
  # send waits for its proxy's bound address), with no settle between
  # streams: senders start within the launch span of START_EPOCH.
  START_EPOCH=$(date +%s)
  HOLD_DEADLINE=$((START_EPOCH + HOLD_RUN_S))
  for ((i = 0; i < total; i++)); do
    t=${s_t[$i]}
    leg=${s_leg[$i]}
    managed=0
    [[ "$t" != srt ]] || managed=1
    if [[ "$t" != srt ]]; then
      hold_proxy_args "$i"
      launch_stream proxy "$t" "$i" "$leg" "$hold_dir/streams/$i" "$HOLD_RUN_S" 1 0 -
      [[ "$t" == tcp ]] || roles+=("$leg-proxy")
    fi
    launch_stream recv "$t" "$i" "$leg" "$hold_dir/streams/$i" "$HOLD_RUN_S" 1 "$managed" -
    launch_stream send "$t" "$i" "$leg" "$hold_dir/streams/$i" "$HOLD_RUN_S" 1 "$managed" -
    roles+=("$leg-recv" "$leg-send")
  done
  launch_end=$(date +%s)
  event "LAUNCH-SPAN step=hold seconds=$((launch_end - START_EPOCH)) processes=${#roles[@]}"
  if [[ $((launch_end - START_EPOCH)) -ge $HOLD_LAUNCH_SPAN_WARN_S ]]; then
    event "WARNING hold launch span $((launch_end - START_EPOCH))s >= ${HOLD_LAUNCH_SPAN_WARN_S}s — the last senders' heartbeat clocks lag START_EPOCH"
    echo "stress: WARNING: hold launch took $((launch_end - START_EPOCH))s" >&2
  fi

  sample_loop "$HOLD_DEADLINE" "$START_EPOCH" "$hold_dir/rss.csv" "$hold_dir/proc.csv" \
    "$hold_dir/host.csv" "$hold_dir/pids" "$SAMPLE_CADENCE_S" sampler &
  record_pid_to "$hold_dir/pids" "$EVENTS_LOG" sampler "$!"

  # Supervise until the deadline: any dead worker is a harness error (the
  # receiver being restarted is replaced before the next check), and each
  # restart instant is acted on when it comes due.
  now=$(date +%s)
  until [[ $now -ge $HOLD_DEADLINE ]]; do
    next=$((now + HOLD_POLL_S))
    if [[ $r_idx -lt ${#RESTART_INSTANTS[@]} ]]; then
      r_at=$((START_EPOCH + RESTART_INSTANTS[r_idx]))
      [[ $r_at -ge $next ]] || next=$r_at
    fi
    [[ $next -le $HOLD_DEADLINE ]] || next=$HOLD_DEADLINE
    [[ $next -le $now ]] || sleep "$((next - now))"
    now=$(date +%s)
    [[ $now -lt $HOLD_DEADLINE ]] || break
    # The sampler too: without it there are no CSV rows and the hold
    # cannot be judged.
    for role in "${roles[@]}" sampler; do
      kill -0 "${PIDS[$role]}" 2>/dev/null ||
        abort_run "hold worker $role (pid ${PIDS[$role]}) died at hold elapsed $((now - START_EPOCH))s"
    done
    if [[ $r_idx -lt ${#RESTART_INSTANTS[@]} && $now -ge $((START_EPOCH + RESTART_INSTANTS[r_idx])) ]]; then
      restart_srt0 "$hold_dir"
      r_idx=$((r_idx + 1))
    fi
    now=$(date +%s)
  done
  event "HOLD-DEADLINE restarts=$HOLD_RESTARTS_DONE/${#RESTART_INSTANTS[@]}"

  # Every worker ends on its own --seconds/--run-seconds; the hard limit
  # catches a wedged one, as in a sweep step.
  timed_out=0
  until ! workers_alive "${roles[@]}" || [[ $(date +%s) -ge $((HOLD_DEADLINE + STEP_TIMEOUT_SLACK_S)) ]]; do
    sleep "$SUPERVISE_POLL_S"
  done
  if workers_alive "${roles[@]}"; then
    timed_out=1
    event "HOLD-TIMEOUT limit_s=$((HOLD_RUN_S + STEP_TIMEOUT_SLACK_S)) — killing every process"
    for role in "${roles[@]}"; do kill -9 "${PIDS[$role]}" 2>/dev/null || true; done
  fi
  # Reap the CURRENT pids (a restarted receiver's predecessor is in
  # restart-exits.json), then stop the sampler.
  local -a exit_pairs=()
  for role in "${roles[@]}"; do
    rc=0
    wait "${PIDS[$role]}" || rc=$?
    exit_pairs+=("$role" "$rc")
    [[ $rc -eq 0 ]] || event "EXIT-NONZERO step=hold role=$role pid=${PIDS[$role]} status=$rc"
  done
  [[ $timed_out -eq 0 ]] || exit_pairs+=(hold_timeout 124)
  printf '%s\n' "${exit_pairs[@]}" |
    jq -Rn '[inputs] | [range(0; length; 2) as $i | {key: .[$i], value: (.[$i + 1] | tonumber)}] | from_entries' \
      >"$hold_dir/exits.json"
  kill "${PIDS[sampler]}" 2>/dev/null || true
  wait "${PIDS[sampler]}" 2>/dev/null || true

  rc=0
  "$BIN" report hold --dir "$hold_dir" \
    --rss-slope-threshold-kb-per-hour "$RSS_SLOPE_THRESHOLD_KB_PER_HOUR" \
    --fd-delta-max "$FD_DELTA_MAX" --thread-delta-max "$THREAD_DELTA_MAX" \
    --cpu-headroom-max "$CPU_HEADROOM_MAX" --delivery-slack "$DELIVERY_SLACK" \
    --queue-depth-fraction "$QUEUE_DEPTH_FRACTION" || rc=$?
  if [[ $rc -eq 2 ]] && any_nonzero_exit "$hold_dir/exits.json"; then
    # Overload rule, as for a sweep step.
    failing=(hold_unjudgeable)
    [[ $timed_out -eq 0 ]] || failing=(hold_timeout hold_unjudgeable)
    event "HOLD-UNJUDGEABLE report hold exited 2 with a nonzero worker exit or a timeout — FAIL"
    jq -n --slurpfile hc "$hold_dir/hold-config.json" \
      --argjson step "$(fallback_step_results "$hold_dir" "$CPU_HEADROOM_MAX" "${failing[@]}")" \
      '{decl: $hc[0], step: $step, hold_verdicts: [], pass: false}' >"$hold_dir/.hold-results.json.tmp"
    mv -f "$hold_dir/.hold-results.json.tmp" "$hold_dir/hold-results.json"
    rc=1
  elif [[ $rc -le 1 && $timed_out -eq 1 ]]; then
    # A timed-out hold FAILs whatever the report made of its evidence.
    jq '.pass = false | .step.pass = false | .step.failing = ((.step.failing // []) + ["hold_timeout"] | unique)' \
      "$hold_dir/hold-results.json" >"$hold_dir/.hold-results.json.tmp"
    mv -f "$hold_dir/.hold-results.json.tmp" "$hold_dir/hold-results.json"
    rc=1
  fi
  PIDS=()
  case "$rc" in
    0 | 1) HOLD_REPORT_RC=$rc ;;
    *) abort_run "report hold exited $rc on $hold_dir (harness error, not a verdict)" ;;
  esac
  event "HOLD-DONE report_hold_rc=$rc restarts=$HOLD_RESTARTS_DONE"
  CURRENT_STEP=""
}

# ---------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------

for t in "${TRANSPORTS[@]}"; do
  sweep_axis "$t" streams "${STREAM_LADDER[@]}"
  sweep_axis "$t" bitrate "${SCALE_LADDER[@]}"
done
CURRENT_STEP=""
event "SWEEP-DONE"

if [[ "$SKIP_HOLD" -eq 0 ]]; then
  run_hold
fi

echo "stress: generating stress-results.json..." >&2
REPORT_RC=0
"$BIN" report stress --outdir "$OUTDIR" || REPORT_RC=$?
event "REPORT-STRESS rc=$REPORT_RC"

{
  echo "=== stress summary ==="
  echo "outdir: $OUTDIR"
  echo "seed: $SEED  transports: ${TRANSPORTS[*]}  smoke: $SMOKE  srt_reconnect_mode: $SRT_RECONNECT_MODE"
  echo "stream ladder: ${STREAM_LADDER[*]}  scale ladder: ${SCALE_LADDER[*]}  warm-up: ${STEP_WARMUP_S}s  step hold: ${STEP_HOLD_S}s"
  [[ -z "$SMOKE_FORCED_FAIL_STEP" ]] || echo "forced-fail step (smoke): $SMOKE_FORCED_FAIL_STEP"
  echo "source: $(jq -r '.source.describe // "unknown"' "$OUTDIR/provenance.json")"
  echo
  if [[ -s "$OUTDIR/stress-results.json" ]]; then
    printf '%-6s %-8s %-8s %-10s %s\n' transport axis ceiling first_fail failing_verdicts
    # Every read is guarded: a malformed results file must not abort the
    # summary (set -e/pipefail) and lose REPORT_RC.
    {
      jq -r '.sweep[] | [.transport, .axis, (.ceiling // "none" | tostring),
                         (.first_fail // "-" | tostring),
                         ((.first_fail_verdicts // []) | join(",") | if . == "" then "-" else . end)]
             | @tsv' "$OUTDIR/stress-results.json" |
        while IFS=$'\t' read -r a b c d e; do printf '%-6s %-8s %-8s %-10s %s\n' "$a" "$b" "$c" "$d" "$e"; done
    } || echo "summary: stress-results.json unreadable"
    echo
    echo "overall_pass: $(jq -r '.overall_pass' "$OUTDIR/stress-results.json" 2>/dev/null || echo unreadable)"
    jq -r '(.limitations // [])[] | "limitation: " + .' "$OUTDIR/stress-results.json" ||
      echo "summary: stress-results.json unreadable"
  else
    echo "no stress-results.json (report stress rc=$REPORT_RC). Last passing load per axis, from this script:"
    for key in "${!LAST_PASS[@]}"; do echo "  $key: ${LAST_PASS[$key]}"; done
  fi
  echo
  if [[ "$SKIP_HOLD" -eq 1 ]]; then
    echo "hold: skipped (--skip-hold)"
  elif [[ "$HOLD_REFUSED" -eq 1 ]]; then
    echo "hold: refused — a transport had no passing streams step (exit 1)"
  else
    print_hold_sizing | sed 's/^/hold: /'
    echo "hold: restarts performed: $HOLD_RESTARTS_DONE of ${#RESTART_INSTANTS[@]} scheduled"
    echo "hold: report hold rc=$HOLD_REPORT_RC pass=$(jq -r '.pass' "$OUTDIR/hold/hold-results.json" 2>/dev/null || echo unreadable)"
  fi
} | tee "$OUTDIR/summary.txt" >&2

FINAL_RC=$REPORT_RC
# A refused hold is a refusal (exit 1) even when the sweep's report passed.
if [[ "$HOLD_REFUSED" -eq 1 && "$FINAL_RC" -eq 0 ]]; then FINAL_RC=1; fi
RUN_DONE=1
exit "$FINAL_RC"
