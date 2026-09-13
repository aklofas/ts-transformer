#!/usr/bin/env bash
# Multi-day endurance ("soak") run: two concurrent legs of the
# tst-interop driver pushing synthetic MPEG-TS/KLV traffic through an
# impaired UDP proxy for hours at a time, sampling process RSS along
# the way, then handing everything to `tst-interop report soak` for a
# verdict. This is the "impaired endurance" half of this arc's
# published evidence (run-matrix.sh's transport/format matrix is the
# "real tools, short runs" half).
#
# Topology (see docs/specs/2026-08-02-interop-evidence-and-live-tool-matrix.md
# §7 for the design, and crates/tst-interop/src/report.rs's `soak`
# module doc for the verdict shapes this run's evidence feeds):
#   - `srt` leg: `tst-interop send --managed` (SRT, wrapped in
#     `tst_pipeline::ManagedTransport` so a transport break reconnects)
#     -> impairment proxy (2% loss, 20ms jitter over a 30ms base delay,
#     1% reorder, a seeded RNG, and a 90s full-drop outage window every
#     6h) -> `tst-interop
#     recv --managed` (`ManagedRecvTransport`/`ManagedDemuxReceiver` —
#     a listener-mode SRT recv otherwise accepts exactly ONE connection
#     for its whole process lifetime, so without this flag the FIRST
#     outage would end the capture for good even though the sender
#     keeps retrying forever on the other end). BOTH sides must survive
#     each outage window and reconnect once it closes.
#   - `rist` leg: the same shape over RIST, through a SECOND impaired
#     proxy with the SAME continuous impairment but NO outage window.
#     One outage-driven reconnect assertion (the srt leg) is enough —
#     adding a second would double this run's flake surface (two
#     independent outage/reconnect state machines to reason about) for
#     no new signal: RIST has no `Managed*` reconnect wrapper of its
#     own in this codebase, so a RIST outage would just be sustained
#     data loss, not a reconnect exercise. The rist leg's job here is
#     purely "does sustained loss/jitter/reorder over many hours behave
#     the same as it does over a five-second interop-matrix cell."
#
# Every long-running process's stdout/stderr is redirected to
# `--outdir/logs/*.log`; each PID is additionally recorded under
# `--outdir/pids/*.pid` so `nohup bash soak.sh --outdir DIR &`
# (Task 16's real 72h invocation) leaves behind a way to check on or
# kill an in-progress run without needing this script's own job-control
# state.
#
# # Running the soak on a fresh host
#
# Prerequisites: Linux — validated on x86_64; aarch64 is expected to work
# (no arch-specific code anywhere in this path: pure Rust + bash + /proc)
# but hasn't actually been validated yet. `git`, a Rust toolchain via
# rustup (this repo pins 1.85 via rust-toolchain.toml — `rustup` picks
# it up automatically once you're inside the repo), `jq` + `python3` on
# PATH (this script's own port allocation and small JSON assembly — the
# SAME two tools run-matrix.sh already requires, see this directory's
# README.md), AND the native build toolchain `SRT_FORCE_VENDORED=1
# RIST_FORCE_VENDORED=1 cargo build` below actually needs even though
# this script itself never calls a C/C++ compiler directly: `tst-interop`
# depends on `tstrans-srt-sys` (vendored libsrt, built via `cmake`) and
# `tstrans-mbedtls-src` (vendored mbedTLS, ALSO built via `cmake`, as a
# build-dependency of the srt-sys build script) and `tstrans-rist-sys`
# (vendored librist, built via `meson`+`ninja` — NOT cmake), so a
# genuinely fresh host needs `build-essential` (a C/C++ compiler +
# related tooling) and `cmake` for the first two, plus `meson` +
# `ninja-build` for the third, or the very first `cargo build` below
# fails with an unhelpful "cmake: command not found"/"meson: command
# not found" instead of a clear prerequisite error. Unlike run-matrix.sh,
# this script needs NO third-party MEDIA tools at all (no ffmpeg/
# tsduck/vlc/mpv/gstreamer) — every process it launches is `tst-interop`
# talking to itself through its own impairment proxy.
#
#   git clone --recurse-submodules https://github.com/aklofas/ts-transformer.git
#   cd ts-transformer            # the clone root IS the workspace root
#   curl https://sh.rustup.rs -sSf | sh -s -- -y   # if rustup isn't already installed
#   sudo apt install -y jq python3 build-essential cmake meson ninja-build clang libclang-dev
#   SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop
#
# (`clang`/`libclang-dev`: bindgen — run by both sys crates' build
# scripts — loads libclang at build time; librist's meson build also
# wants a clang toolchain present. Both were missing from this list the
# first time it ran on a genuinely fresh Ubuntu host, 2026-08-04.)
#
# Then launch the real 72h run (the canonical seed below reproduces the
# exact same impairment decision sequence — see impair.rs's module doc
# on why the engine is deterministic given seed+config). This script's
# own send/recv invocations already pass `--no-klv-digest` on every
# process it launches — nothing extra to pass here for that; see the
# "Expected outputs" note below on what it changes in the per-leg JSON.
#
# **Launch this genuinely detached — `nohup ... & disown`, exactly as
# shown below — and NEVER through a supervising tool/session mechanism
# that can enforce its own lifetime cap on the invocation** (found the
# hard way during this arc's own fix-wave validation: a session-tool
# background-command wrapper silently killed a 1-hour smoke around the
# ~60-minute mark, well short of even that short run's own deadline —
# `nohup`+`disown` detaches the process from that supervision entirely,
# so it keeps running past the launching tool call's own return and
# past the launching session ending). Don't wait on the launching
# command itself for 72 hours; instead poll for `soak-results.json`
# (or `pids/*.pid` + `ps`) to detect completion:
#
#   nohup bash scripts/interop/soak.sh --outdir ~/interop-soak-$(date +%F) --seed 1 &
#
# Expected outputs under `--outdir`:
#   rss.csv            - elapsed_s,leg,process,pid,rss_kb (6 PIDs, 3 process names, both legs)
#   soak-config.json   - the run's declared parameters, written BEFORE any
#                         evidence exists (`report soak`'s `--config`)
#   exits.json          - every worker's reaped exit status (`report soak`'s `--exits`)
#   srt/{proxy-stats,recv-report,send-report}.json  - klv_set_sha256 is `null` in both
#   rist/{proxy-stats,recv-report,send-report}.json   report/send JSONs (--no-klv-digest;
#                                                      counts/every other field unaffected)
#   logs/*.log          - one file per launched process (each send/recv beats a
#                         one-line "heartbeat" into its log every 60s — counters +
#                         wire bytes — so a dead process is findable to the minute)
#   pids/*.pid          - one PID per launched process + the RSS sampler
#   soak-events.log     - timestamped lifecycle events (launches, premature
#                         deaths, fail-fast kills)
#   soak-FAILED         - written ONLY when the supervisor fail-fasts a run
#                         after a worker died mid-run (names the role + log)
#   soak-results.json   - `report soak`'s verdict document
#   summary.txt         - short human-readable render of the same
#
# Supervisor fail-fast: soak run 1 (2026-08-04) lost its BOTH senders to
# a fixture panic 14.5h in, and the remaining processes idled for 12+
# hours before anyone noticed — the `wait`-until-deadline shape below
# has no way to notice a death early. The supervisor loop polls every
# worker PID every 30s; a death more than SUPERVISOR_GRACE_S before the
# deadline kills the whole run immediately, records soak-FAILED +
# soak-events.log, and exits nonzero (still attempting the report over
# whatever evidence exists). Deaths within the last SUPERVISOR_GRACE_S
# are normal end-of-run staggering (send's own --seconds window starts
# before the sampler's START_EPOCH), handled by the ordinary waits.
#
# Harvest steps once it completes: read `summary.txt` for the headline
# (overall_pass, per-leg drop rates, RSS slopes), then feed the numbers
# into `docs/.../validation-evidence.md`.
#
# The RSS-slope gate defaults ON for full-length (>= 72h) runs at
# 200 KiB/hour, pinned from the first completed 72h run (launched
# 2026-08-05, completed 2026-08-08, seed 1, realistic AU sizes): the
# worst library-attributable post-warmup slope observed was 68.8
# KiB/h (srt send), and even that is warm-up
# convergence to the sender's ~176 MiB steady-state plateau, not
# growth — every process's slope over the run's final 24h was
# <= 18 KiB/h. 200 gives ~3x headroom over the worst legitimate
# whole-run slope while still catching the digest-accumulation class
# of harness leak this check was built for (349 and 1485 KiB/h in the
# pre-fix smoke runs) and any real leak >= ~0.2 MiB/h. Shorter runs
# stay provisional (no threshold) unless one is passed explicitly:
# under realistic AU sizes the senders take ~7h to reach that plateau,
# so a short run's post-warmup regression window is dominated by
# convergence and would trip any honest threshold spuriously.
#
# `--hours 1` is the smoke mode this task's own acceptance gate runs
# locally: short enough to finish in about an hour, long enough for the
# RSS sampler to collect a real post-warmup regression window (this
# module's warmup formula scales down for a short run — see report.rs).
# A 1h run never reaches even one 6h outage window, so the smoke proves
# `recv --managed`'s HAPPY path only (steady-state receiving is
# unaffected by wrapping it in `ManagedRecvTransport`/
# `ManagedDemuxReceiver` — no spurious reconnects, no throughput or
# correctness regression); the reconnect/outage-window path itself is
# exercised by `report.rs`'s own unit tests plus targeted short-outage
# dry-runs (not this script) — see this task's own report for why
# that's an accepted, well-understood trade-off ahead of the real 72h
# run, which DOES reach the schedule's outage windows for real.
#
# `--hours N` (decimal allowed, e.g. 0.05 for a 3-minute drill): fast
# enough to exercise the whole launch/supervisor/report path end-to-end
# without waiting out even the 1h smoke above — long enough for a
# handful of sampler ticks, too short for any real RSS-slope or outage
# signal (both stay provisional at that length, same as any sub-72h run).
#
# Validated on linux-x86_64; linux-aarch64 is expected to work (no
# arch-specific code: pure Rust + bash + /proc) but hasn't been
# validated yet — see lib.sh's header for the shell-portability stance
# this whole directory takes (this script is not made macOS-portable).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

# Panics from any tst-interop process land in its log WITH a backtrace —
# run 1's senders died with a bare panic line and no frames, which was
# enough this time (the panic message named the value) but won't always
# be. RUST_LOG=info additionally surfaces tst-pipeline's managed-
# reconnect attempt/backoff logs (see main.rs's init_tracing doc), the
# exact visibility a stuck reconnect loop needs; leaves libsrt/librist
# at their modest info volume.
export RUST_BACKTRACE=1
export RUST_LOG="${RUST_LOG:-info}"

HOURS=72
OUTDIR=""
SEED=1
RSS_SLOPE_THRESHOLD=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --hours)
      HOURS=$2
      shift 2
      ;;
    --outdir)
      OUTDIR=$2
      shift 2
      ;;
    --seed)
      SEED=$2
      shift 2
      ;;
    --rss-slope-threshold-kb-per-hour)
      RSS_SLOPE_THRESHOLD=$2
      shift 2
      ;;
    -h | --help)
      # Print the whole header comment block (line 2 up to the first
      # non-comment line) rather than a hardcoded line range — the old
      # '2,103p' range silently truncated the help text every time the
      # header grew.
      awk 'NR >= 2 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "soak.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

# --hours accepts a decimal (e.g. `0.05` = 3 minutes) so a drill can
# exercise the whole supervisor/report path in minutes rather than
# hours — bash's own integer arithmetic can't validate or consume that,
# so both the format check and the positivity check below shell out to
# `awk`, and every downstream use of $HOURS (TOTAL_SECONDS, the
# full-length slope-default check) does too, rather than bash's
# `$(( ))`/`10#` integer path used elsewhere in this script.
[[ "$HOURS" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk -v h="$HOURS" 'BEGIN{exit !(h>0)}' || {
  echo "soak.sh: --hours must be a positive number, got: $HOURS" >&2
  exit 2
}
[[ "$SEED" =~ ^[0-9]+$ ]] || {
  echo "soak.sh: --seed must be a non-negative integer, got: $SEED" >&2
  exit 2
}
[[ -n "$OUTDIR" ]] || OUTDIR="$HOME/interop-soak-$(date -u +%Y%m%dT%H%M%SZ)"

# Default the RSS-slope gate ON for full-length runs only — see the
# header's "RSS-slope gate" paragraph for the 200 KiB/h derivation and
# why short runs stay provisional (plateau convergence dominates their
# post-warmup window).
if [[ -z "$RSS_SLOPE_THRESHOLD" ]] && awk -v h="$HOURS" 'BEGIN{exit !(h>=72)}'; then
  RSS_SLOPE_THRESHOLD=200
fi

# Same hard-dependency rationale as run-matrix.sh's own check (this
# script cannot run at all without these — see that script's comment
# for why they're checked loudly up front rather than surfacing as an
# obscure failure hours into a multi-day run).
for dep in jq python3 awk; do
  have "$dep" || {
    echo "soak.sh: required tool '$dep' not found on PATH — install it before running this script (see this file's own header for the full prerequisite list)" >&2
    exit 2
  }
done

mkdir -p "$OUTDIR/srt" "$OUTDIR/rist" "$OUTDIR/logs" "$OUTDIR/pids"
RSS_CSV="$OUTDIR/rss.csv"
printf 'elapsed_s,leg,process,pid,rss_kb\n' >"$RSS_CSV"

# Timestamped lifecycle event log — see the header's outputs list.
EVENTS_LOG="$OUTDIR/soak-events.log"
event() {
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" >>"$EVENTS_LOG"
}

TOTAL_SECONDS=$(awk -v h="$HOURS" 'BEGIN{printf "%d", h*3600}')
# Outage schedule (srt leg only — see this file's header). Kept as
# separate numeric/unit-suffixed forms rather than duplicating "6h"
# and "21600" independently: OUTAGE_PERIOD_S is the single source of
# truth, both the proxy's own `--outage` flag string and `report
# soak`'s `--outage-period-s` are built from it.
OUTAGE_PERIOD_S=21600 # 6h
OUTAGE_DUR_S=90
LOSS_PCT=2
JITTER_MS=20
# Constant one-way base delay on top of the jitter — a realistic WAN
# hop's worth of lag (both legs' senders otherwise talk to their proxy
# over loopback's ~0ms, which no real link has).
DELAY_MS=30
REORDER="1,200" # 1%, held 200ms — several packet intervals at this traffic's rate
# How close to the nominal deadline a worker death stops being treated
# as a mid-run failure — see the header's "Supervisor fail-fast" note.
SUPERVISOR_GRACE_S=120

# Settle time between binding a listener and starting its peer — see
# run-matrix.sh's own SETTLE for the same reasoning (every scheme here
# binds near-instantly once its process starts).
SETTLE=2
# How long to poll for a proxy's `{"listening": ...}` stdout line
# before giving up — see proxy::run's doc comment: it's printed
# synchronously right after bind, so this is a generous ceiling, not
# an expected wait.
PROXY_ADDR_POLL_TIMEOUT_S=10
# Both proxies launch (and start their OWN --run-seconds countdown)
# strictly BEFORE the sampler's own $START_EPOCH is captured — by the
# time all six processes plus two `wait_for_bound_addr` polls have
# fired, real process-spawn + scheduling overhead (confirmed
# empirically: measured up to ~6.5s for the srt leg, ~2.5s for rist)
# means a proxy's own `--run-seconds` deadline, though sized to line up
# with the sampler's nominal end-of-run instant, can elapse a few
# seconds BEFORE the sampler's next 30s tick — which would otherwise
# sample a PID that's already exited and record an empty `rss_kb`,
# indistinguishable from a genuine crash (`report.rs`'s
# `zero_process_exits` verdict). Comfortably larger than one 30s
# sampler tick so the fix holds even under scheduling jitter well
# beyond what was actually measured. Applied on BOTH sides of the gap:
# the sampler stops this much EARLIER than its nominal deadline (losing
# well under 0.02% of a 72h run's own RSS coverage — negligible), and
# both proxies' `--run-seconds` gain this much extra margin, so neither
# fix alone has to carry the full burden.
SAMPLER_END_SLACK_S=35

# Declared BEFORE any evidence exists — `report soak` judges the run
# against these, never against what the artifacts happen to span
# (release-gate audit RLS-B09).
RSS_CADENCE_S=30
jq -n --argjson dur "$TOTAL_SECONDS" --argjson cad "$RSS_CADENCE_S" \
  --argjson slack "$SAMPLER_END_SLACK_S" \
  '{expected_duration_s: $dur, rss_cadence_s: $cad, warmup_fraction: 0.1667,
    sampler_end_slack_s: $slack, expected_worker_exits: {}}' >"$OUTDIR/soak-config.json"

echo "soak: building tst-interop (release)..." >&2
(cd "$REPO_ROOT" && SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop)
BIN="$REPO_ROOT/target/release/tst-interop"

declare -A PIDS

# record_pid <role> <pid> — writes both the tracking array entry and
# the on-disk pidfile Task 16's `nohup ... &` invocation needs.
record_pid() {
  local role=$1 pid=$2
  PIDS[$role]=$pid
  echo "$pid" >"$OUTDIR/pids/$role.pid"
  event "LAUNCHED role=$role pid=$pid"
}

# wait_for_bound_addr <stdout_file> -- polls (sanctioned `until ...; do
# sleep N; done` shape — see lib.sh's header on why a bare loop+sleep
# is avoided) for `proxy::run`'s `{"listening": "host:port"}` stdout
# line and echoes the address, or exits 1 after
# PROXY_ADDR_POLL_TIMEOUT_S with a named error.
wait_for_bound_addr() {
  local stdout_file=$1 deadline addr
  deadline=$(($(date +%s) + PROXY_ADDR_POLL_TIMEOUT_S))
  addr=""
  until [[ -n "$addr" || $(date +%s) -ge $deadline ]]; do
    if [[ -s "$stdout_file" ]]; then
      addr=$(jq -r '.listening // empty' "$stdout_file" 2>/dev/null) || addr=""
    fi
    [[ -n "$addr" ]] || sleep 0.2
  done
  if [[ -z "$addr" ]]; then
    echo "soak: proxy never reported its bound address within ${PROXY_ADDR_POLL_TIMEOUT_S}s (see $stdout_file)" >&2
    exit 1
  fi
  printf '%s' "$addr"
}

# ---------------------------------------------------------------------
# srt leg: send --managed -> proxy (impairment + outage) -> recv
# ---------------------------------------------------------------------
#
# The proxy must be launched FIRST here, well before recv/send, and
# given a head start past its own outage window 0 — `impair::Engine::
# in_outage`'s zero-based window numbering means window `k=0` covers
# `[0, outage_dur_s)` measured from the PROXY'S OWN launch instant
# (`proxy::run`'s `run_start`), so a proxy started immediately before
# the initial SRT handshake would spend its first `outage_dur_s`
# seconds dropping every packet — including that handshake — and both
# `recv`'s accept (15s default timeout) and `send`'s connect (2s
# default timeout) would time out well before a 90s outage window ever
# clears. Discovered empirically running this exact script during Task
# 14's own smoke validation (srt-recv/srt-send both failed with
# accept/connect timeouts on the very first attempt) — see this task's
# own report for the full diagnosis. Starting the proxy
# `SRT_PROXY_WARMUP_S` seconds ahead of recv/send sidesteps it
# entirely: only window 0 is affected by this launch-order artifact,
# and every later window (starting at `OUTAGE_PERIOD_S`, `2 *
# OUTAGE_PERIOD_S`, ...) is exactly the outage this run means to
# exercise a real mid-stream reconnect against.
SRT_PROXY_WARMUP_S=$((OUTAGE_DUR_S + 30))
# + SAMPLER_END_SLACK_S: see that constant's own doc comment — keeps
# this proxy alive past the sampler's last tick instead of exiting a
# few seconds ahead of it.
SRT_PROXY_RUN_SECONDS=$((TOTAL_SECONDS + SRT_PROXY_WARMUP_S + SAMPLER_END_SLACK_S))

SRT_RECV_PORT=$(free_port)

SRT_PROXY_STDOUT="$OUTDIR/logs/srt-proxy.stdout"
"$BIN" proxy --listen 127.0.0.1:0 --forward "127.0.0.1:$SRT_RECV_PORT" \
  --loss "$LOSS_PCT" --jitter "$JITTER_MS" --delay "$DELAY_MS" --reorder "$REORDER" --seed "$SEED" \
  --outage "period=${OUTAGE_PERIOD_S}s,dur=${OUTAGE_DUR_S}s" \
  --stats-json "$OUTDIR/srt/proxy-stats.json" --run-seconds "$SRT_PROXY_RUN_SECONDS" \
  >"$SRT_PROXY_STDOUT" 2>"$OUTDIR/logs/srt-proxy.log" &
record_pid srt-proxy $!
SRT_PROXY_ADDR=$(wait_for_bound_addr "$SRT_PROXY_STDOUT")

echo "soak: srt proxy warming up ${SRT_PROXY_WARMUP_S}s past its initial outage window before starting send/recv..." >&2
sleep "$SRT_PROXY_WARMUP_S"

# --no-klv-digest on every send/recv below: without it, both sides
# accumulate one hex digest string per KLV record for the ENTIRE run
# (needed for klv_set_sha256, an order-insensitive fingerprint that has
# to sort every digest before hashing) — confirmed during Task 14's own
# smoke run to cost ~3.6-5.7 MiB/h of RSS growth that's pure harness
# bookkeeping, unrelated to the library code this soak measures, and
# would otherwise swamp the RSS-slope evidence over 72h. The short
# interop-matrix cells run-matrix.sh drives do NOT pass this flag —
# their transparent-tier byte comparisons need the hash, and their runs
# are seconds long, so the accumulation never mattered there.
"$BIN" recv --url "srt://:$SRT_RECV_PORT?mode=listener" --expect baseline \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/srt/recv-report.json" --no-klv-digest \
  --managed \
  >"$OUTDIR/logs/srt-recv.log" 2>&1 &
record_pid srt-recv $!
sleep "$SETTLE"

# --au-sizes realistic (both legs' senders): GOP-structured multi-KB
# AUs at ~1.7 Mb/s — the soak measures endurance under a real encoder's
# traffic shape, not the interop matrix's tiny compact fixtures. See
# `fixtures::AuSizeMode`.
"$BIN" send --profile baseline --url "srt://$SRT_PROXY_ADDR" --managed \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/srt/send-report.json" --no-klv-digest \
  --au-sizes realistic \
  >"$OUTDIR/logs/srt-send.log" 2>&1 &
record_pid srt-send $!

# ---------------------------------------------------------------------
# rist leg: send -> proxy (impairment, NO outage) -> recv
# ---------------------------------------------------------------------

RIST_RECV_PORT=$(free_port)
"$BIN" recv --url "rist://@0.0.0.0:$RIST_RECV_PORT" --expect baseline \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rist/recv-report.json" --no-klv-digest \
  >"$OUTDIR/logs/rist-recv.log" 2>&1 &
record_pid rist-recv $!
sleep "$SETTLE"

RIST_PROXY_STDOUT="$OUTDIR/logs/rist-proxy.stdout"
# + SAMPLER_END_SLACK_S: same reasoning as the srt proxy's own
# --run-seconds above.
RIST_PROXY_RUN_SECONDS=$((TOTAL_SECONDS + SAMPLER_END_SLACK_S))
"$BIN" proxy --listen 127.0.0.1:0 --forward "127.0.0.1:$RIST_RECV_PORT" \
  --loss "$LOSS_PCT" --jitter "$JITTER_MS" --delay "$DELAY_MS" --reorder "$REORDER" --seed "$SEED" \
  --stats-json "$OUTDIR/rist/proxy-stats.json" --run-seconds "$RIST_PROXY_RUN_SECONDS" \
  >"$RIST_PROXY_STDOUT" 2>"$OUTDIR/logs/rist-proxy.log" &
record_pid rist-proxy $!
RIST_PROXY_ADDR=$(wait_for_bound_addr "$RIST_PROXY_STDOUT")

"$BIN" send --profile baseline --url "rist://$RIST_PROXY_ADDR" \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rist/send-report.json" --no-klv-digest \
  --au-sizes realistic \
  >"$OUTDIR/logs/rist-send.log" 2>&1 &
record_pid rist-send $!

# ---------------------------------------------------------------------
# RSS sampler: every 30s, VmRSS of all 6 PIDs -> rss.csv
# ---------------------------------------------------------------------

START_EPOCH=$(date +%s)
DEADLINE=$((START_EPOCH + TOTAL_SECONDS))
# The sampler itself stops SAMPLER_END_SLACK_S before this nominal
# deadline — see that constant's own doc comment. $DEADLINE stays the
# "official" end-of-run instant used for the log line below (and
# matches what an operator would expect from --hours), not what the
# sampler loop actually polls against.
SAMPLER_DEADLINE=$((DEADLINE - SAMPLER_END_SLACK_S))
echo "soak: running until $(date -u -d "@$DEADLINE" +%Y-%m-%dT%H:%M:%SZ) (${HOURS}h, seed=$SEED)..." >&2

# sample_rss_loop <deadline_epoch> <start_epoch> <out_csv> <leg:process:pid>...
#
# Sanctioned `until <cond>; do ...; sleep N; done` shape (see lib.sh's
# header and this file's own header) rather than a bare `while true`
# loop with `sleep` in its body. Runs as its own background job for the
# whole soak duration; a missing PID (process already gone) writes an
# empty rss_kb field, which is exactly report.rs's `soak::RssSample`
# crash signal — never fails this loop itself, so one dead process
# doesn't stop sampling the other five.
sample_rss_loop() {
  local deadline=$1 start=$2 out=$3
  shift 3
  local -a entries=("$@")
  local entry leg rest process pid rss_kb elapsed
  until [[ $(date +%s) -ge $deadline ]]; do
    elapsed=$(($(date +%s) - start))
    for entry in "${entries[@]}"; do
      leg=${entry%%:*}
      rest=${entry#*:}
      process=${rest%%:*}
      pid=${rest#*:}
      rss_kb=""
      if [[ -r "/proc/$pid/status" ]]; then
        rss_kb=$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null) || rss_kb=""
      fi
      printf '%s,%s,%s,%s,%s\n' "$elapsed" "$leg" "$process" "$pid" "$rss_kb" >>"$out"
    done
    sleep "$RSS_CADENCE_S"
  done
}

sample_rss_loop "$SAMPLER_DEADLINE" "$START_EPOCH" "$RSS_CSV" \
  "srt:send:${PIDS[srt-send]}" "srt:proxy:${PIDS[srt-proxy]}" "srt:recv:${PIDS[srt-recv]}" \
  "rist:send:${PIDS[rist-send]}" "rist:proxy:${PIDS[rist-proxy]}" "rist:recv:${PIDS[rist-recv]}" &
record_pid sampler $!

# ---------------------------------------------------------------------
# Supervisor: fail fast on a mid-run worker death
# ---------------------------------------------------------------------
#
# See the header's "Supervisor fail-fast" note for why this exists
# (run 1's senders died 14.5h in; the waits below can't notice until
# the 72h deadline). Polls every worker + the sampler every 30s until
# SUPERVISOR_GRACE_S before the nominal deadline; end-of-run process
# exits inside that grace window are normal staggering and are left to
# the ordinary waits below.
ALL_ROLES=(srt-recv srt-proxy srt-send rist-recv rist-proxy rist-send)
PREMATURE_DEATH=""
until [[ $(date +%s) -ge $((DEADLINE - SUPERVISOR_GRACE_S)) || -n "$PREMATURE_DEATH" ]]; do
  for role in "${ALL_ROLES[@]}" sampler; do
    kill -0 "${PIDS[$role]}" 2>/dev/null || {
      PREMATURE_DEATH=$role
      break
    }
  done
  [[ -n "$PREMATURE_DEATH" ]] || sleep 30
done

if [[ -n "$PREMATURE_DEATH" ]]; then
  # The sampler is an in-script background loop with no log file of its
  # own — pointing at logs/sampler.log would send the operator to a
  # file that never existed; its evidence trail is rss.csv itself.
  if [[ "$PREMATURE_DEATH" == "sampler" ]]; then
    DEATH_EVIDENCE="rss.csv (sampler is the in-script RSS loop; it has no log file)"
  else
    DEATH_EVIDENCE="logs/$PREMATURE_DEATH.log"
  fi
  event "PREMATURE-DEATH role=$PREMATURE_DEATH pid=${PIDS[$PREMATURE_DEATH]} — failing fast, killing every worker"
  echo "soak: $PREMATURE_DEATH (pid ${PIDS[$PREMATURE_DEATH]}) died mid-run — failing the whole run NOW rather than idling to the deadline; see $DEATH_EVIDENCE" >&2
  {
    echo "role=$PREMATURE_DEATH"
    echo "detected_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "log=$DEATH_EVIDENCE"
  } >"$OUTDIR/soak-FAILED"
  for role in "${ALL_ROLES[@]}"; do
    kill "${PIDS[$role]}" 2>/dev/null || true
  done
fi

# ---------------------------------------------------------------------
# Wait for the run to finish
# ---------------------------------------------------------------------
#
# `recv`'s own exit code reflects its VerifyReport's `pass` (0/1), a
# legitimate outcome this script doesn't treat as a hard failure —
# `report soak`'s `recv_invariants_<leg>` verdict is the real judge of
# that number, computed from the JSON, not this process's exit code.
# `send`/`proxy` exiting nonzero is unexpected (send retries forever
# under `--managed`; proxy has no failure path once bound) — every
# worker's exit status is recorded below in exits.json and judged by
# `report soak`'s `worker_exits` verdict (including a death inside the
# supervisor's end-grace window above, which this script itself never
# polls for), but this script still proceeds to build the report
# regardless of what it finds: discarding hours of already-collected
# evidence over one process's exit code would be a worse outcome than
# a report that names the problem. (On the fail-fast path above, the
# freshly-killed workers reap here too — nonzero, so their status
# lands in exits.json as well.)
declare -A EXIT_STATUS
for role in "${ALL_ROLES[@]}"; do
  rc=0
  wait "${PIDS[$role]}" || rc=$?
  EXIT_STATUS[$role]=$rc
  if [[ $rc -ne 0 ]]; then
    echo "soak: $role (pid ${PIDS[$role]}) exited $rc — see logs/$role.log" >&2
    event "EXIT-NONZERO role=$role pid=${PIDS[$role]} status=$rc"
  fi
done
# Every worker's exit status, for `report soak`'s worker_exits verdict —
# a nonzero exit anywhere, INCLUDING inside the end-grace window the
# supervisor deliberately stops polling, fails the run through the
# report rather than only through this script's summary line.
{
  printf '{'
  sep=''
  for role in "${ALL_ROLES[@]}"; do
    printf '%s"%s": %d' "$sep" "$role" "${EXIT_STATUS[$role]}"
    sep=', '
  done
  printf '}\n'
} >"$OUTDIR/exits.json"

# Stop the sampler now rather than waiting out its own up-to-30s tail
# past the leg processes' shared deadline.
kill "${PIDS[sampler]}" 2>/dev/null || true
wait "${PIDS[sampler]}" 2>/dev/null || true

# ---------------------------------------------------------------------
# report soak
# ---------------------------------------------------------------------

echo "soak: generating soak-results.json..." >&2
REPORT_ARGS=(
  report soak
  --rss "$RSS_CSV"
  --config "$OUTDIR/soak-config.json"
  --exits "$OUTDIR/exits.json"
  --proxy-stats "$OUTDIR/srt/proxy-stats.json"
  --recv-report "$OUTDIR/srt/recv-report.json"
  --send-report "$OUTDIR/srt/send-report.json"
  --outage-period-s "$OUTAGE_PERIOD_S"
  --rist-proxy-stats "$OUTDIR/rist/proxy-stats.json"
  --rist-recv-report "$OUTDIR/rist/recv-report.json"
  --rist-send-report "$OUTDIR/rist/send-report.json"
  --out "$OUTDIR/soak-results.json"
)
[[ -z "$RSS_SLOPE_THRESHOLD" ]] || REPORT_ARGS+=(--rss-slope-threshold-kb-per-hour "$RSS_SLOPE_THRESHOLD")

REPORT_RC=0
"$BIN" "${REPORT_ARGS[@]}" || REPORT_RC=$?

{
  echo "=== soak summary ==="
  echo "outdir: $OUTDIR"
  echo "hours: $HOURS  seed: $SEED  outage_period_s: $OUTAGE_PERIOD_S  outage_dur_s: $OUTAGE_DUR_S"
  echo "loss_pct: $LOSS_PCT  jitter_ms: $JITTER_MS  delay_ms: $DELAY_MS  reorder: $REORDER  au_sizes: realistic"
  echo "worker exits: $(cat "$OUTDIR/exits.json")"
  [[ -z "$PREMATURE_DEATH" ]] || echo "PREMATURE DEATH: $PREMATURE_DEATH (fail-fast — see soak-FAILED + soak-events.log)"
  echo
  # On the fail-fast path `report soak` typically exits 2 with no
  # output file (a killed recv never writes its report JSON) — the
  # summary must still get written rather than dying here under
  # `pipefail` on the missing file.
  if [[ -s "$OUTDIR/soak-results.json" ]]; then
    jq '{overall_pass, run_duration_s, expected_duration_s, warmup_s, rss_slope_threshold_kb_per_hour,
         rss_slopes, coverage, process_exits, worker_exits, legs, limitations}' "$OUTDIR/soak-results.json"
  else
    echo "no soak-results.json (report soak rc=$REPORT_RC — run did not produce a complete artifact set)"
  fi
} | tee "$OUTDIR/summary.txt" >&2

# A fail-fasted run must exit nonzero regardless of what the report
# step managed to salvage.
if [[ -n "$PREMATURE_DEATH" && "$REPORT_RC" -eq 0 ]]; then
  REPORT_RC=1
fi
exit "$REPORT_RC"
