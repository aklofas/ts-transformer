#!/usr/bin/env bash
# Multi-day endurance ("soak") run: tst-interop legs pushing synthetic
# MPEG-TS/KLV through impaired UDP proxies (srt, rist), two external RTSP
# publishers (rtsp-publish: MP2T; rtsp-publish-es: H.264 + KLV elementary
# tracks) and a WebRTC playback leg (whep: a headless browser playing a
# served mount, reconnecting on a schedule) for hours, sampled for RSS /
# CPU / descriptors and judged by `tst-interop report soak`. Topology,
# realism knobs, outputs, verdicts and the fresh-host recipe:
# scripts/interop/README.md, "Multi-day soak".
#
# Usage:
#   SRT_RECONNECT_MODE=blocking|background bash scripts/interop/soak.sh
#       [--outdir DIR] [--hours 72] [--seed 1] [--profile auto|NAME]
#       [--no-corrupt] [--fixed-impairment] [--schedule-phases 12]
#       [--schedule-phase-s S] [--no-rtsp-publish] [--no-rtsp-publish-es]
#       [--no-whep] [--rss-slope-threshold-kb-per-hour N]
#
#   --outdir DIR        where every artefact lands (default
#                       ~/interop-soak-<UTC timestamp>)
#   --hours H           run length, decimal allowed (default 72; 1 = the
#                       smoke, 0.05 = a ~3-minute wiring drill)
#   --seed N            seeds the profile draw, impairment schedule,
#                       corruption taps and rich KLV (default 1)
#   --profile auto|NAME auto draws one profile per leg from the seed; NAME
#                       pins both legs (the bisect form)
#   --no-corrupt        sender corruption tap off (bisect form)
#   --fixed-impairment  one fixed loss / jitter / delay / reorder level
#                       instead of the seeded schedule (bisect form)
#   --schedule-phases N, --schedule-phase-s S
#                       schedule shape (default 12 phases spanning the run)
#   --no-rtsp-publish   drop the rtsp-publish leg (no GStreamer needed; the
#                       leg needs ~1.7 GB free per hour of run, checked at
#                       launch)
#   --no-rtsp-publish-es
#                       drop the rtsp-publish-es leg (H.264 + KLV pushed as
#                       separate RTP tracks; needs ~0.85 GB free per hour of
#                       one publisher generation, checked at launch)
#   --no-whep           drop the whep leg (no Google Chrome / Chromium needed)
#   --rss-slope-threshold-kb-per-hour N
#                       RSS-slope gate (default 200 for runs of 72 h or
#                       more; shorter runs are provisional unless set)
#
# Environment:
#   SRT_RECONNECT_MODE         REQUIRED, blocking|background; no default — the
#                              two modes exercise different library code
#   OUTAGE_PERIOD_S=21600      srt leg: full-drop outage every 6 h,
#   OUTAGE_DUR_S=90              90 s long
#   LOSS_PCT=2                 loss under --fixed-impairment (0 isolates outages)
#   PUBLISHER_DROP_PERIOD_S=600  rtsp-publish and rtsp-publish-es legs:
#                              publisher drop / re-ANNOUNCE period, shared
#                              (the run must span at least two)
#   WHEP_RECONNECT_S=600       whep leg: the viewer ends its browser and
#                              starts a new one every this many seconds
#                              (the run must span at least two)
#   SOAK_MEM_BUDGET=0.70       fraction of MemTotal the run may use; the
#   SOAK_MEM_HEADROOM=1.5      summed expected RSS of its roles x headroom
#                              must fit it, else exit 2 before any launch
#   RUST_LOG=info              passed to every tst-interop process
#
# Prerequisites: Linux; a Rust toolchain via rustup; jq, python3,
# build-essential, cmake, meson, ninja-build, clang, libclang-dev; for the
# rtsp-publish leg gst-launch-1.0 with rtspclientsink (gstreamer1.0-tools,
# -plugins-bad, -rtsp); the rtsp-publish-es leg additionally tsdemux and
# h264parse (gstreamer1.0-plugins-bad); the whep leg google-chrome or
# chromium on PATH (run headless).
#
# Exit status: `report soak`'s (0 = overall pass); 2 = bad argument, missing
# prerequisite or setup error. A worker death mid-run writes soak-FAILED
# and the run exits nonzero.
#
# Launch a real run detached, never under a session tool that caps its
# lifetime:
#   SRT_RECONNECT_MODE=background nohup bash scripts/interop/soak.sh \
#     --outdir ~/interop-soak-$(date +%F) --seed 1 &
#
# Validated on linux-x86_64; see lib.sh for this directory's portability
# stance (not macOS-portable).

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

# Kept verbatim for provenance.json (the parse loop below consumes "$@").
SCRIPT_ARGV=("$@")

HOURS=72
OUTDIR=""
SEED=1
RSS_SLOPE_THRESHOLD=""
# See README.md's "Realism knobs" (this directory) for what each of these does
# and which one to reach for when bisecting a failure.
PROFILE=auto
CORRUPT=1
FIXED_IMPAIRMENT=0
SCHEDULE_PHASES=12
# Empty = derive from TOTAL_SECONDS / SCHEDULE_PHASES once --hours is
# known (both are parsed below, in either order).
SCHEDULE_PHASE_S=""
RTSP_PUBLISH=1
RTSP_PUBLISH_ES=1
WHEP=1

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
    --profile)
      PROFILE=$2
      shift 2
      ;;
    --no-corrupt)
      CORRUPT=0
      shift
      ;;
    --fixed-impairment)
      FIXED_IMPAIRMENT=1
      shift
      ;;
    --no-rtsp-publish)
      RTSP_PUBLISH=0
      shift
      ;;
    --no-rtsp-publish-es)
      RTSP_PUBLISH_ES=0
      shift
      ;;
    --no-whep)
      WHEP=0
      shift
      ;;
    --schedule-phases)
      SCHEDULE_PHASES=$2
      shift 2
      ;;
    --schedule-phase-s)
      SCHEDULE_PHASE_S=$2
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
# Canonicalize + bound-check every user-supplied integer WITHOUT bash
# arithmetic, BEFORE anything does arithmetic on it.
#
# Two independent hazards, both of which bit this script during review:
#
#   - A leading zero is an ordinary way to type a number, and the three
#     consumers each read it differently: bash's `$(( ))` reads it as
#     OCTAL (so `--seed 08` used to abort the launch under `set -e` with
#     "value too great for base"), `jq --argjson` accepts and silently
#     normalizes it even though RFC 8259 forbids it in a JSON number, and
#     the proxy's Rust parser normalizes too. The declared value in
#     soak-config.json must be the SAME literal the proxy is launched
#     with, since `schedule_declared_<leg>` compares them — so it is
#     normalized once, here, rather than relying on two of three
#     consumers being lenient.
#   - Bash arithmetic is SIGNED 64-bit, so a value the Rust side accepts
#     can wrap negative on the way through: `$((10#18446744073709551615))`
#     is `-1`, and `$((10#99999999999999999999))` is 7766279631452241919.
#     Either would reach the proxy as a number nobody typed, and the
#     resulting error would name that number rather than the input.
#
# So: strip leading zeros as a STRING, then compare by length and
# lexicographically (exact for digit strings of any size). Only after
# this is the arithmetic further down safe by construction. Each cap is
# the field's real Rust type, narrowed where this script's own
# arithmetic needs headroom. The helper itself, canon_int, lives in
# lib.sh (shared with stress.sh).

[[ "$SEED" =~ ^[0-9]+$ ]] || {
  echo "soak.sh: --seed must be a non-negative integer, got: $SEED" >&2
  exit 2
}
# u64 on the Rust side, minus room for the `SEED + 1` / `SEED + 2` the
# per-leg corruption seeds need — the one place arithmetic is unavoidable.
SEED=$(canon_int "$SEED" 9223372036854775805 --seed)

[[ "$SCHEDULE_PHASES" =~ ^[0-9]+$ ]] || {
  echo "soak.sh: --schedule-phases must be a positive integer, got: $SCHEDULE_PHASES" >&2
  exit 2
}
# `ScheduleEcho::phases` is a u32; anything larger could never reach the
# proxy anyway, and would wrap here first.
SCHEDULE_PHASES=$(canon_int "$SCHEDULE_PHASES" 4294967295 --schedule-phases)
[[ "$SCHEDULE_PHASES" -gt 0 ]] || {
  echo "soak.sh: --schedule-phases must be a positive integer, got: $SCHEDULE_PHASES" >&2
  exit 2
}

if [[ -n "$SCHEDULE_PHASE_S" ]]; then
  [[ "$SCHEDULE_PHASE_S" =~ ^[0-9]+$ ]] || {
    echo "soak.sh: --schedule-phase-s must be a positive integer (seconds), got: $SCHEDULE_PHASE_S" >&2
    exit 2
  }
  # `ScheduleEcho::phase_s` is a u64; capped at i64::MAX because this
  # script compares it arithmetically below.
  SCHEDULE_PHASE_S=$(canon_int "$SCHEDULE_PHASE_S" 9223372036854775807 --schedule-phase-s)
  [[ "$SCHEDULE_PHASE_S" -gt 0 ]] || {
    echo "soak.sh: --schedule-phase-s must be a positive integer (seconds), got: $SCHEDULE_PHASE_S" >&2
    exit 2
  }
fi

# NON-EMPTINESS ONLY. This guard exists to catch `--profile ''`, which
# would otherwise reach the launch commands as an empty string. It
# deliberately does NOT check that the value is `auto` or a known profile
# name: bash has no business holding a second copy of the profile
# registry (see run-matrix.sh's own stance on duplicated inventories).
# `auto` is acted on further down (the `pick-profiles` branch); a NAMED
# profile is rejected before any worker launches by `report soak
# --validate-only`, which checks the declaration this script writes
# against the registry ("legs.srt.profile \"X\" is not a known profile",
# exit 2), and again by `send --profile` / `recv --expect`.
[[ -n "$PROFILE" ]] || {
  echo "soak.sh: --profile requires a value ('auto' or a profile name)" >&2
  exit 2
}
[[ -n "$OUTDIR" ]] || OUTDIR="$HOME/interop-soak-$(date -u +%Y%m%dT%H%M%SZ)"

# Default the RSS-slope gate ON for full-length runs only — see the
# README.md's "Outputs and the supervisor" for the 200 KiB/h derivation and
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
# The rtsp-publish leg's publisher is GStreamer's rtspclientsink (package
# gstreamer1.0-rtsp on Debian/Ubuntu). Checked here, before the build, so
# a host without it is told at once rather than after minutes of cargo.
if [[ "$RTSP_PUBLISH" -eq 1 ]]; then
  { have gst-launch-1.0 && gst-inspect-1.0 rtspclientsink >/dev/null 2>&1; } || {
    echo "soak.sh: the rtsp-publish leg needs gst-launch-1.0 with the rtspclientsink element \
(gstreamer1.0-tools + gstreamer1.0-rtsp) — install them, or pass --no-rtsp-publish to run without that leg" >&2
    exit 2
  }
fi
# The rtsp-publish-es leg's publisher splits the generated TS back into
# its elementary streams (tsdemux, h264parse) and pushes them as two RTP
# tracks through the same rtspclientsink.
if [[ "$RTSP_PUBLISH_ES" -eq 1 ]]; then
  { have gst-launch-1.0 && gst-inspect-1.0 rtspclientsink >/dev/null 2>&1 &&
    gst-inspect-1.0 tsdemux >/dev/null 2>&1 && gst-inspect-1.0 h264parse >/dev/null 2>&1; } || {
    echo "soak.sh: the rtsp-publish-es leg needs gst-launch-1.0 with the rtspclientsink, tsdemux and \
h264parse elements (gstreamer1.0-tools + gstreamer1.0-rtsp + gstreamer1.0-plugins-bad) — install them, \
or pass --no-rtsp-publish-es to run without that leg" >&2
    exit 2
  }
fi
# The whep leg's viewer is headless Google Chrome (or Chromium), the
# browser the matrix's whep/* cells drive (lib.sh's whep_browser).
WHEP_BROWSER=""
if [[ "$WHEP" -eq 1 ]]; then
  WHEP_BROWSER=$(whep_browser) || {
    echo "soak.sh: the whep leg needs google-chrome or chromium on PATH (run headless, as the \
interop matrix's whep/* cells run it) — install one, or pass --no-whep to run without that leg" >&2
    exit 2
  }
fi

mkdir -p "$OUTDIR/srt" "$OUTDIR/rist" "$OUTDIR/logs" "$OUTDIR/pids"
[[ "$RTSP_PUBLISH" -eq 0 ]] || mkdir -p "$OUTDIR/rtsp-publish/segments"
[[ "$RTSP_PUBLISH_ES" -eq 0 ]] || mkdir -p "$OUTDIR/rtsp-publish-es"
[[ "$WHEP" -eq 0 ]] || mkdir -p "$OUTDIR/whep/viewer"
RSS_CSV="$OUTDIR/rss.csv"
printf 'elapsed_s,leg,process,pid,rss_kb\n' >"$RSS_CSV"
# Two more time series from the same sampler tick — see README.md's
# outputs list. Separate files rather than new columns on rss.csv so the
# RSS-slope verdict's input keeps its exact shape (`report soak --rss`).
PROC_CSV="$OUTDIR/proc.csv"
printf 'elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n' >"$PROC_CSV"
HOST_CSV="$OUTDIR/host.csv"
printf 'elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n' >"$HOST_CSV"

# Timestamped lifecycle event log — see README.md's outputs list.
EVENTS_LOG="$OUTDIR/soak-events.log"
event() { event_to "$EVENTS_LOG" "$@"; }

TOTAL_SECONDS=$(awk -v h="$HOURS" 'BEGIN{printf "%d", h*3600 + 0.5}')
# Outage schedule (srt leg only — see README.md's "Topology"). Kept as
# separate numeric/unit-suffixed forms rather than duplicating "6h"
# and "21600" independently: OUTAGE_PERIOD_S is the single source of
# truth, both the proxy's own `--outage` flag string and `report
# soak`'s `--outage-period-s` are built from it.
OUTAGE_PERIOD_S="${OUTAGE_PERIOD_S:-21600}" # 6h; env-overridable for a short outage-focused drill
OUTAGE_DUR_S="${OUTAGE_DUR_S:-90}"
# How the srt leg's managed sender spends an outage — see `send
# --reconnect-mode`. `blocking` stalls the producer for the outage and
# replays it as a burst; `background` keeps it moving and drains a bounded
# gap buffer. REQUIRED, no default (see README.md's "The reconnect mode"): the
# first three runs defaulted to `blocking` and the 0.7.0 RC soak needs
# `background`; an inherited default is the one misconfiguration the
# declared-vs-observed verdict cannot catch, because the declaration would
# be wrong in the same way the run is.
[[ -n "${SRT_RECONNECT_MODE:-}" ]] || {
  echo "soak.sh: SRT_RECONNECT_MODE is required (no default) — set it to 'blocking' or 'background' in the \
environment; it picks how the srt leg's managed sender spends an outage and is declared in soak-config.json" >&2
  exit 2
}
# Validate the env-overridable knobs HERE, before anything derives a flag
# string from them. A typo would otherwise surface minutes later inside a
# backgrounded worker (or not at all, as a silently wrong outage schedule),
# which is exactly the kind of failure a drill cannot afford to diagnose.
[[ "$OUTAGE_PERIOD_S" =~ ^[0-9]+$ ]] && [ "$OUTAGE_PERIOD_S" -gt 0 ] || {
  echo "soak.sh: OUTAGE_PERIOD_S must be a positive integer (seconds), got: $OUTAGE_PERIOD_S" >&2
  exit 2
}
[[ "$OUTAGE_DUR_S" =~ ^[0-9]+$ ]] && [ "$OUTAGE_DUR_S" -gt 0 ] || {
  echo "soak.sh: OUTAGE_DUR_S must be a positive integer (seconds), got: $OUTAGE_DUR_S" >&2
  exit 2
}
[ "$OUTAGE_DUR_S" -lt "$OUTAGE_PERIOD_S" ] || {
  echo "soak.sh: OUTAGE_DUR_S ($OUTAGE_DUR_S) must be shorter than OUTAGE_PERIOD_S ($OUTAGE_PERIOD_S) \
— an outage at least as long as its period never lets the link recover" >&2
  exit 2
}
case "$SRT_RECONNECT_MODE" in
  blocking | background) ;;
  *)
    echo "soak.sh: SRT_RECONNECT_MODE must be blocking|background, got: $SRT_RECONNECT_MODE" >&2
    exit 2
    ;;
esac
# How often the rtsp-publish leg's publisher session ends and a new one
# ANNOUNCEs (see that leg's block below). The run is cut into
# TOTAL_SECONDS / PUBLISHER_DROP_PERIOD_S generations, which must be at
# least two — one generation drops nothing.
PUBLISHER_DROP_PERIOD_S="${PUBLISHER_DROP_PERIOD_S:-600}"
[[ "$PUBLISHER_DROP_PERIOD_S" =~ ^[0-9]+$ ]] || {
  echo "soak.sh: PUBLISHER_DROP_PERIOD_S must be a positive integer (seconds), got: $PUBLISHER_DROP_PERIOD_S" >&2
  exit 2
}
# Canonical base 10 before any arithmetic, like --seed: `0600` would
# otherwise be octal 384 and `090` would abort under set -e.
PUBLISHER_DROP_PERIOD_S=$(canon_int "$PUBLISHER_DROP_PERIOD_S" 9223372036854775807 PUBLISHER_DROP_PERIOD_S)
[[ "$PUBLISHER_DROP_PERIOD_S" -gt 0 ]] || {
  echo "soak.sh: PUBLISHER_DROP_PERIOD_S must be a positive integer (seconds), got: $PUBLISHER_DROP_PERIOD_S" >&2
  exit 2
}
PUBLISH_GENERATIONS=$((TOTAL_SECONDS / PUBLISHER_DROP_PERIOD_S))
# Either publish leg on: both share this generation count.
ANY_PUBLISH=0
[[ "$RTSP_PUBLISH" -eq 0 && "$RTSP_PUBLISH_ES" -eq 0 ]] || ANY_PUBLISH=1
if [[ "$ANY_PUBLISH" -eq 1 && "$PUBLISH_GENERATIONS" -lt 2 ]]; then
  echo "soak.sh: PUBLISHER_DROP_PERIOD_S ($PUBLISHER_DROP_PERIOD_S) leaves fewer than two publisher \
generations in a ${TOTAL_SECONDS}s run — lower it, or pass --no-rtsp-publish --no-rtsp-publish-es" >&2
  exit 2
fi
# How often the whep leg's viewer ends its browser and starts a new one
# (see that leg's block below). The run holds WHEP_CONNECTIONS =
# TOTAL_SECONDS / WHEP_RECONNECT_S browser connections, one per slot,
# which must be at least two — one connection reconnects nothing.
# The knob of a leg that will not run is not checked under --no-whep.
WHEP_RECONNECT_S="${WHEP_RECONNECT_S:-600}"
# Each browser is ended WHEP_RELAUNCH_GAP_S before its slot ends, so the
# next one starts on its own slot rather than late by a browser's
# teardown and launch; the slots themselves are absolute, counted from
# the loop's start, so the gaps never accumulate. `report soak` holds the
# viewer report's own window to the slot less this gap
# (`WHEP_RELAUNCH_GAP_S` in report/soak.rs).
WHEP_RELAUNCH_GAP_S=3
WHEP_CONNECTION_S=0
WHEP_CONNECTIONS=0
if [[ "$WHEP" -eq 1 ]]; then
  [[ "$WHEP_RECONNECT_S" =~ ^[0-9]+$ ]] || {
    echo "soak.sh: WHEP_RECONNECT_S must be a positive integer (seconds), got: $WHEP_RECONNECT_S" >&2
    exit 2
  }
  WHEP_RECONNECT_S=$(canon_int "$WHEP_RECONNECT_S" 9223372036854775807 WHEP_RECONNECT_S)
  [[ "$WHEP_RECONNECT_S" -gt "$WHEP_RELAUNCH_GAP_S" ]] || {
    echo "soak.sh: WHEP_RECONNECT_S must exceed the ${WHEP_RELAUNCH_GAP_S}s browser relaunch gap, got: $WHEP_RECONNECT_S" >&2
    exit 2
  }
  WHEP_CONNECTION_S=$((WHEP_RECONNECT_S - WHEP_RELAUNCH_GAP_S))
  WHEP_CONNECTIONS=$((TOTAL_SECONDS / WHEP_RECONNECT_S))
  [[ "$WHEP_CONNECTIONS" -ge 2 ]] || {
    echo "soak.sh: WHEP_RECONNECT_S ($WHEP_RECONNECT_S) leaves fewer than two browser connections \
in a ${TOTAL_SECONDS}s run — lower it, or pass --no-whep" >&2
    exit 2
  }
fi
# The four FIXED-impairment knobs. Used only under --fixed-impairment:
# the default run drives both proxies from a seeded --schedule instead,
# whose phases override all four per phase (which is why the proxy
# refuses to accept them together). Kept — rather than deleted — because
# "is this failure the schedule's doing?" is a question a long run will
# eventually raise, and the old single-level shape is the answer to it.
LOSS_PCT="${LOSS_PCT:-2}" # env-overridable: LOSS_PCT=0 --fixed-impairment isolates outage effects from loss
# Decimal-allowed (like --hours), so awk does the range check, not bash.
[[ "$LOSS_PCT" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk -v l="$LOSS_PCT" 'BEGIN{exit !(l>=0 && l<100)}' || {
  echo "soak.sh: LOSS_PCT must be a number in [0, 100), got: $LOSS_PCT" >&2
  exit 2
}
JITTER_MS=20
# Constant one-way base delay on top of the jitter — a realistic WAN
# hop's worth of lag (both legs' senders otherwise talk to their proxy
# over loopback's ~0ms, which no real link has).
DELAY_MS=30
REORDER="1,200" # 1%, held 200ms — several packet intervals at this traffic's rate
# Sender corruption tap (see README.md's "Realism knobs"): 5 injections
# per 10,000 packets, no two closer than 1000 packets. The gap is the
# load-bearing half — `corrupt::CorruptConfig::validate` refuses a gap
# short enough for two injections to both plausibly explain one receiver
# event, and an ambiguous attribution would make the corruption verdicts
# unfalsifiable. Each leg draws from its OWN seed (`SEED + 1` / `SEED +
# 2`) so the two taps damage different packets: the same seed on both
# would inject at the same stream offsets, and a single systematic
# attribution bug could then look like agreement between two independent
# legs.
# `$SEED` is already base-10 canonical (normalized at validation, above),
# so plain arithmetic is safe here.
CORRUPT_SPEC="rate=5,min_gap=1000"
SRT_CORRUPT_SEED=$((SEED + 1))
RIST_CORRUPT_SEED=$((SEED + 2))

# The KLV set both ends of every leg run. Named here, above the config
# write, because the run DECLARES it (see `klv_declared_<leg>`): a leg
# whose receiver was launched in compact mode runs no rich-KLV oracle at
# all, and every rich verdict would be skipped rather than failed.
KLV_SET=rich

# $CORRUPT_SPEC broken out for the declaration below. Mirrors
# `corrupt::parse_corrupt`'s grammar: comma-separated key=value, classes
# `+`-separated, an absent `classes=` meaning all seven, an absent
# `min_gap=` meaning corrupt/tap.rs's DEFAULT_MIN_GAP. This is deliberately a
# re-derivation, not a second source of truth — `report soak` compares
# what is declared here against the spec the SENDER wrote into its own
# corruption log header, so a mistake in this block fails
# `corruption_declared_<leg>` loudly on the first run rather than
# publishing a spec the run did not use.
CORRUPT_RATE=""
CORRUPT_MIN_GAP=1000
CORRUPT_CLASSES="body_flip+header+truncate+garbage+drop+dup+psi_flip"
IFS=',' read -r -a CORRUPT_SPEC_PARTS <<<"$CORRUPT_SPEC"
for part in "${CORRUPT_SPEC_PARTS[@]}"; do
  case "$part" in
    rate=*) CORRUPT_RATE="${part#rate=}" ;;
    min_gap=*) CORRUPT_MIN_GAP="${part#min_gap=}" ;;
    classes=*) CORRUPT_CLASSES="${part#classes=}" ;;
    *)
      echo "soak: cannot declare corruption spec part '$part' (CORRUPT_SPEC=$CORRUPT_SPEC)" >&2
      exit 2
      ;;
  esac
done
if [[ -z "$CORRUPT_RATE" ]]; then
  echo "soak: CORRUPT_SPEC ($CORRUPT_SPEC) has no rate=" >&2
  exit 2
fi

# Default the phase length so the declared phases tile the whole run
# (12 phases over 72h = one new link condition every 6h). Integer
# division deliberately: a run that outlives `phases * phase_s` simply
# stays on the last phase (`impair::Engine::phase_index`), which is the
# right behaviour for the remainder seconds this truncation leaves. The
# floor of 1 keeps a very short drill (`--hours 0.02` with 12 phases)
# from asking the proxy for `phase_s=0s`, which it rejects.
if [[ -z "$SCHEDULE_PHASE_S" ]]; then
  SCHEDULE_PHASE_S=$((TOTAL_SECONDS / SCHEDULE_PHASES))
  [[ "$SCHEDULE_PHASE_S" -ge 1 ]] || SCHEDULE_PHASE_S=1
fi
# How close to the nominal deadline a worker death stops being treated
# as a mid-run failure — see README.md's "Outputs and the supervisor".
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
# time every worker plus two `wait_for_bound_addr` polls have
# fired, real process-spawn + scheduling overhead (confirmed
# empirically: measured up to ~6.5s for the srt leg, ~2.5s for rist)
# means a proxy's own `--run-seconds` deadline, though sized to line up
# with the sampler's nominal end-of-run instant, can elapse a few
# seconds BEFORE the sampler's next 30s tick — which would otherwise
# sample a PID that's already exited and record an empty `rss_kb`,
# indistinguishable from a genuine crash (`report/soak.rs`'s
# `zero_process_exits` verdict). Comfortably larger than one 30s
# sampler tick so the fix holds even under scheduling jitter well
# beyond what was actually measured. Applied on BOTH sides of the gap:
# the sampler stops this much EARLIER than its nominal deadline (losing
# well under 0.02% of a 72h run's own RSS coverage — negligible), and
# both proxies' `--run-seconds` gain this much extra margin, so neither
# fix alone has to carry the full burden.
SAMPLER_END_SLACK_S=35

# Provenance: WHAT ran, from WHICH tree, on WHAT host — written before the
# build so even a run that dies in `cargo build` leaves it behind. Every
# earlier archive recorded its source SHA by hand in a status note (and the
# forced-break drill's note had to be corrected after the fact); a 72-hour
# result that cannot be tied to an exact tree, submodule set and toolchain
# is not reproducible evidence. Recorded, not gated: `report soak` never
# reads this file. `git` is optional here — a tarball checkout records
# `null`s rather than failing the launch.
#
# `argv` is this script's own argument vector; `env` is the five knobs the
# header documents plus RUST_LOG, i.e. everything that can change the run
# without appearing in argv. `clk_tck` is what proc.csv's CPU tick columns
# divide by to become seconds (`getconf CLK_TCK`, 100 on every Linux this
# has run on, recorded rather than assumed).
ARGV_JSON=$(printf '%s\n' "${SCRIPT_ARGV[@]}" | jq -Rn '[inputs]')
ENV_JSON=$(jq -n \
  --arg outage_period_s "$OUTAGE_PERIOD_S" --arg outage_dur_s "$OUTAGE_DUR_S" \
  --arg srt_reconnect_mode "$SRT_RECONNECT_MODE" --arg loss_pct "$LOSS_PCT" \
  --arg publisher_drop_period_s "$PUBLISHER_DROP_PERIOD_S" --arg rust_log "$RUST_LOG" \
  --arg whep_reconnect_s "$WHEP_RECONNECT_S" \
  '{OUTAGE_PERIOD_S: $outage_period_s, OUTAGE_DUR_S: $outage_dur_s,
    SRT_RECONNECT_MODE: $srt_reconnect_mode, LOSS_PCT: $loss_pct,
    PUBLISHER_DROP_PERIOD_S: $publisher_drop_period_s, WHEP_RECONNECT_S: $whep_reconnect_s,
    RUST_LOG: $rust_log}')
write_provenance "$REPO_ROOT" "$OUTDIR/provenance.json" "$ARGV_JSON" "$ENV_JSON"

# The build comes BEFORE the config declaration (it used to follow it)
# because the declaration now includes the drawn per-leg profiles, and
# `pick-profiles` — the one place that owns the seed-to-profile mapping
# — lives in the binary being built. The invariant the declaration
# actually needs is unchanged and still holds: the config is written
# before any EVIDENCE exists, i.e. before a single worker launches.
echo "soak: building tst-interop (release)..." >&2
(cd "$REPO_ROOT" && SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop)
BIN="$REPO_ROOT/target/release/tst-interop"

# Per-leg profile draw. `auto` asks the binary for two DISTINCT names
# derived from this run's seed; a named profile pins both legs (the
# reproduction form). `pick-profiles` exits 2 on an impossible draw, and
# an unknown NAME is caught either by `report soak --validate-only`
# below (which rejects a declaration naming a profile the registry
# doesn't have) or by `send`/`recv` themselves — bash never holds its
# own copy of the profile list.
if [[ "$PROFILE" == "auto" ]]; then
  mapfile -t PICKED < <("$BIN" pick-profiles --seed "$SEED" --legs 2)
  [[ ${#PICKED[@]} -eq 2 ]] || {
    echo "soak.sh: pick-profiles did not return two profile names (got: ${PICKED[*]-none})" >&2
    exit 2
  }
  SRT_PROFILE=${PICKED[0]}
  RIST_PROFILE=${PICKED[1]}
else
  SRT_PROFILE=$PROFILE
  RIST_PROFILE=$PROFILE
fi
# The rtsp-publish leg is not part of the seeded draw (adding a third leg
# to it would change which profiles the srt and rist legs get for an
# existing seed). Under `auto` it runs `baseline`, the profile the
# interop matrix's rtsp-publish/gst-push-mp2t cell pushes through the same
# publisher; a named --profile pins it like the other two.
if [[ "$PROFILE" == "auto" ]]; then
  PUBLISH_PROFILE=baseline
else
  PUBLISH_PROFILE=$PROFILE
fi
# The rtsp-publish-es leg always runs klv-sync, whatever --profile says:
# KLV over RTP needs a timestamp per unit, which only synchronous KLV (PES
# with a PTS) gives tsdemux to hand on; see the matrix's
# rtsp-publish/gst-push-es-klv cell.
PUBLISH_ES_PROFILE=klv-sync
# The whep leg, like rtsp-publish, is outside the seeded draw: `baseline`
# under auto (the profile the matrix's whep/* cells serve), pinned by a
# named --profile. serve-whep serves the profile's KLV schedule with the
# compact record set and a real H.264 GOP as the video (see
# whep_serve.rs's module doc for why not the generator's slices).
if [[ "$PROFILE" == "auto" ]]; then
  WHEP_PROFILE=baseline
else
  WHEP_PROFILE=$PROFILE
fi
echo "soak: profiles — srt=$SRT_PROFILE rist=$RIST_PROFILE rtsp-publish=$PUBLISH_PROFILE \
rtsp-publish-es=$PUBLISH_ES_PROFILE whep=$WHEP_PROFILE (--profile $PROFILE, seed $SEED)" >&2
# serve-whep pushes an H.264 GOP as the video, from a one-program
# profile: any other named --profile would give a leg that serves nothing
# from launch and fails only at report time, hours later. Refused here,
# before any worker launches, by the same check serve-whep itself runs.
if [[ "$WHEP" -eq 1 ]]; then
  "$BIN" serve-whep --profile "$WHEP_PROFILE" --validate-profile || {
    echo "soak.sh: the whep leg cannot serve profile '$WHEP_PROFILE' (see above) — pass --no-whep, or \
name an H.264 one-program profile" >&2
    exit 2
  }
fi

# rtsp-publish leg geometry (see that leg's block below). The publisher
# pushes PUBLISH_CONTENT_S seconds of generated stream, cut into
# PUBLISH_GENERATIONS segments, one publisher session each, so the
# declared drop period is the segment length and the declared generation
# count is PUBLISH_GENERATIONS — the count `report soak` judges against.
# The content stops PUBLISH_TAIL_S short of the run: each restart costs
# the publisher a fraction of a second (measured 0.15-0.6 s per relaunch
# on loopback), and the last segment has to be fully delivered before
# `recv`'s own deadline, `--seconds` after its first event. So the tail
# is 30 s plus a full second per generation, which covers the measured
# worst case at any period. The rtsp-publish-es leg replays one
# PUBLISH_SEGMENT_S source per generation, so it spans the same content
# and shares the tail.
PUBLISH_TAIL_S=$((30 + PUBLISH_GENERATIONS))
PUBLISH_CONTENT_S=$((TOTAL_SECONDS - PUBLISH_TAIL_S))
PUBLISH_SEGMENT_S=0
if [[ "$ANY_PUBLISH" -eq 1 ]]; then
  [[ "$PUBLISH_CONTENT_S" -ge "$PUBLISH_GENERATIONS" ]] || {
    echo "soak.sh: a ${TOTAL_SECONDS}s run is too short for the publish legs' ${PUBLISH_TAIL_S}s tail — \
lengthen --hours, or pass --no-rtsp-publish --no-rtsp-publish-es" >&2
    exit 2
  }
  PUBLISH_SEGMENT_S=$((PUBLISH_CONTENT_S / PUBLISH_GENERATIONS))
fi

# Declared BEFORE any evidence exists — `report soak` judges the run
# against these, never against what the artifacts happen to span. `legs`
# carries the same declare-then-check stance one level down: which
# profile each leg was launched with, and which impairment schedule its
# proxy was given (`null` under
# --fixed-impairment), checked afterwards against the recv report's own
# `profile` field and the proxy's own schedule echo. `reconnect_mode`
# is the mode the leg's MANAGED sender is launched with, checked against
# the send report's `managed_send.reconnect_mode`; the rist leg's sender
# is not managed (see README.md's "Topology"), so it declares `null`.
RSS_CADENCE_S=30
if [[ "$CORRUPT" -eq 1 ]]; then
  CORRUPTION_DECL=true
  CORRUPTION_SPEC_DECL=$(jq -n --argjson rate "$CORRUPT_RATE" \
    --argjson min_gap "$CORRUPT_MIN_GAP" --arg classes "$CORRUPT_CLASSES" \
    '{rate_per_10k: $rate, min_gap: $min_gap, classes: ($classes | split("+"))}')
else
  CORRUPTION_DECL=false
  CORRUPTION_SPEC_DECL=null
fi
if [[ "$FIXED_IMPAIRMENT" -eq 1 ]]; then
  SCHEDULE_DECL=null
else
  SCHEDULE_DECL=$(jq -n --argjson seed "$SEED" --argjson phases "$SCHEDULE_PHASES" \
    --argjson phase_s "$SCHEDULE_PHASE_S" \
    '{seed: $seed, phases: $phases, phase_s: $phase_s}')
fi
# The rtsp-publish leg's declaration, or null for a --no-rtsp-publish run
# (then the leg is absent from `legs` altogether). It has no proxy, so no
# schedule, and no managed sender, so no reconnect mode; what it declares
# instead is the publisher it runs, how often that publisher drops, and
# how many publisher sessions the run launches.
# The publisher's port is drawn at launch and recorded in its log.
PUBLISHER_DECL="gst-launch-1.0 filesrc location=rtsp-publish/segments/gen-N.ts ! tsparse set-timestamps=true ! rtspclientsink location=rtsp://127.0.0.1:<port>/soak protocols=tcp"
if [[ "$RTSP_PUBLISH" -eq 1 ]]; then
  PUBLISH_LEG_DECL=$(jq -n --arg profile "$PUBLISH_PROFILE" --arg klv_set "$KLV_SET" \
    --argjson klv_seed "$SEED" --arg publisher "$PUBLISHER_DECL" \
    --argjson period "$PUBLISH_SEGMENT_S" --argjson generations "$PUBLISH_GENERATIONS" \
    '{profile: $profile, schedule: null, klv_set: $klv_set, klv_seed: $klv_seed,
      reconnect_mode: null, publisher: $publisher, publisher_drop_period_s: $period,
      publisher_generations: $generations}')
else
  PUBLISH_LEG_DECL=null
fi
# The rtsp-publish-es leg's declaration, or null for --no-rtsp-publish-es.
# Same shape as the MPEG-TS leg's, plus `klv_placement`: the receiver
# judges every KLV record against the video access unit it was generated
# with. `remuxed` and `klv_records_per_generation` are added together once
# the source exists (below): validation refuses a remuxed leg with no
# count, and the count is read from the generated source.
PUBLISHER_ES_DECL="gst-launch-1.0 filesrc location=rtsp-publish-es/source.ts ! tsdemux name=d d. ! queue ! h264parse ! s.sink_0 d. ! queue ! meta/x-klv ! s.sink_1 rtspclientsink name=s location=rtsp://127.0.0.1:<port>/soak-es protocols=tcp"
PUBLISH_ES_KLV_SET=compact
if [[ "$RTSP_PUBLISH_ES" -eq 1 ]]; then
  PUBLISH_ES_LEG_DECL=$(jq -n --arg profile "$PUBLISH_ES_PROFILE" --arg klv_set "$PUBLISH_ES_KLV_SET" \
    --argjson klv_seed "$SEED" --arg publisher "$PUBLISHER_ES_DECL" \
    --argjson period "$PUBLISH_SEGMENT_S" --argjson generations "$PUBLISH_GENERATIONS" \
    '{profile: $profile, schedule: null, klv_set: $klv_set, klv_seed: $klv_seed,
      reconnect_mode: null, publisher: $publisher, publisher_drop_period_s: $period,
      publisher_generations: $generations, klv_placement: true}')
else
  PUBLISH_ES_LEG_DECL=null
fi
# The whep leg's declaration, or null for --no-whep. No proxy, so no
# schedule; no managed sender, so no reconnect mode. What it declares is
# the mount kind serve-whep runs (mp2t: the schedule is muxed in-process
# and pushed as TS bytes, the path a muxed source takes), the viewer
# command, the reconnect slot and how many browser connections the loop
# makes — the count `report soak` holds BOTH the viewer's own report and
# the server's session total to (`whep_reconnects_observed`).
# The server's port is drawn at launch and recorded in its log.
WHEP_MOUNT_KIND=mp2t
WHEP_VIEWER_DECL="$WHEP_BROWSER --headless=new --no-sandbox --disable-gpu --autoplay-policy=no-user-gesture-required --enable-logging=stderr --log-level=0 --no-first-run --disable-background-networking --allow-file-access-from-files --user-data-dir=whep/viewer/profile file://scripts/interop/whep-page.html?url=http://127.0.0.1:<port>/soak"
if [[ "$WHEP" -eq 1 ]]; then
  WHEP_LEG_DECL=$(jq -n --arg profile "$WHEP_PROFILE" --arg mount_kind "$WHEP_MOUNT_KIND" \
    --arg viewer "$WHEP_VIEWER_DECL" --argjson reconnect_s "$WHEP_RECONNECT_S" \
    --argjson connections "$WHEP_CONNECTIONS" \
    '{profile: $profile, schedule: null, klv_set: "compact", klv_seed: null,
      reconnect_mode: null, mount_kind: $mount_kind, viewer: $viewer,
      viewer_reconnect_s: $reconnect_s, viewer_connections: $connections}')
else
  WHEP_LEG_DECL=null
fi
# ---------------------------------------------------------------------
# Pre-launch memory gate
# ---------------------------------------------------------------------
#
# The run's expected resident memory, summed over the roles it launches
# (the plateau figures of the 0.7.0 and 2026-10-08 runs, KiB), times
# SOAK_MEM_HEADROOM, must fit SOAK_MEM_BUDGET of MemTotal. A host that
# cannot hold the run refuses it here, before any worker launches, and
# names the --no-rtsp-publish* / --no-whep flags that would fit. The raw
# sum, the budget, the headroom and MemTotal go into soak-config.json,
# which `report soak` holds to the same inequality.
SOAK_MEM_BUDGET=${SOAK_MEM_BUDGET:-0.70}
SOAK_MEM_HEADROOM=${SOAK_MEM_HEADROOM:-1.5}
declare -A ROLE_RSS_KB=(
  [srt-send]=180000 [rist-send]=190000 [srt-recv]=60000 [rist-recv]=60000
  [srt-proxy]=20000 [rist-proxy]=20000
  [rtsp-publish-recv]=60000 [rtsp-publish-publisher]=80000
  [rtsp-publish-es-recv]=60000 [rtsp-publish-es-publisher]=80000
  # whep-viewer is the browser's whole process tree, per-process RSS
  # summed (14 processes of Chrome 155 playing the mount, measured on the
  # 2026-10-21 wiring run; shared pages count once per process, so this
  # overstates), though only the viewer loop's shell is sampled, like the
  # publisher loops; whep-serve the server with its in-process muxer
  # (15 MB measured, the plateau not yet known).
  [whep-serve]=100000 [whep-viewer]=1200000
  [sampler]=10000
)
# gate_predicted_kb <publish 0|1> <publish-es 0|1> <whep 0|1> — the raw role sum.
gate_predicted_kb() {
  local roles=(srt-send rist-send srt-recv rist-recv srt-proxy rist-proxy sampler) r sum=0
  [[ "$1" -eq 0 ]] || roles+=(rtsp-publish-recv rtsp-publish-publisher)
  [[ "$2" -eq 0 ]] || roles+=(rtsp-publish-es-recv rtsp-publish-es-publisher)
  [[ "$3" -eq 0 ]] || roles+=(whep-serve whep-viewer)
  for r in "${roles[@]}"; do sum=$((sum + ROLE_RSS_KB[$r])); done
  echo "$sum"
}
MEM_TOTAL_KB=$(awk '/^MemTotal:/ {print $2; exit}' /proc/meminfo 2>/dev/null)
if ! [[ "$MEM_TOTAL_KB" =~ ^[0-9]+$ ]] || [[ "$MEM_TOTAL_KB" -eq 0 ]]; then
  echo "soak: memory gate: MemTotal unreadable" >&2
  exit 2
fi
if ! awk -v b="$SOAK_MEM_BUDGET" -v h="$SOAK_MEM_HEADROOM" \
  'BEGIN{exit !(b ~ /^[0-9]*\.?[0-9]+$/ && b > 0 && b <= 1 && h ~ /^[0-9]*\.?[0-9]+$/ && h >= 1)}'; then
  echo "soak: SOAK_MEM_BUDGET must be in (0, 1] and SOAK_MEM_HEADROOM >= 1.0 (got $SOAK_MEM_BUDGET, $SOAK_MEM_HEADROOM)" >&2
  exit 2
fi
GATE_BUDGET_KB=$(awk -v m="$MEM_TOTAL_KB" -v b="$SOAK_MEM_BUDGET" 'BEGIN{printf "%d", m * b}')
# gate_fits <predicted-kb> — 0 when predicted x headroom <= budget.
gate_fits() {
  awk -v p="$1" -v h="$SOAK_MEM_HEADROOM" -v b="$GATE_BUDGET_KB" 'BEGIN{exit !(p * h <= b)}'
}
GATE_PREDICTED_KB=$(gate_predicted_kb "$RTSP_PUBLISH" "$RTSP_PUBLISH_ES" "$WHEP")
if ! gate_fits "$GATE_PREDICTED_KB"; then
  GATE_FIT_FLAGS=""
  # Fewer legs than the current run, fewest flags first; each that fits is named.
  for combo in "1 1 0 --no-whep" "1 0 1 --no-rtsp-publish-es" "0 1 1 --no-rtsp-publish" \
    "1 0 0 --no-rtsp-publish-es --no-whep" "0 1 0 --no-rtsp-publish --no-whep" \
    "0 0 1 --no-rtsp-publish --no-rtsp-publish-es" \
    "0 0 0 --no-rtsp-publish --no-rtsp-publish-es --no-whep"; do
    read -r cp ce cw cflags <<<"$combo"
    [[ "$cp" -le "$RTSP_PUBLISH" && "$ce" -le "$RTSP_PUBLISH_ES" && "$cw" -le "$WHEP" ]] || continue
    [[ "$cp" -ne "$RTSP_PUBLISH" || "$ce" -ne "$RTSP_PUBLISH_ES" || "$cw" -ne "$WHEP" ]] || continue
    gate_fits "$(gate_predicted_kb "$cp" "$ce" "$cw")" || continue
    GATE_FIT_FLAGS+="${GATE_FIT_FLAGS:+; or }$cflags"
  done
  GATE_FIT_MSG="no combination of --no-rtsp-publish* / --no-whep flags fits this host"
  [[ -z "$GATE_FIT_FLAGS" ]] || GATE_FIT_MSG="would fit: $GATE_FIT_FLAGS"
  echo "soak: memory gate: predicted $((GATE_PREDICTED_KB / 1024)) MB x$SOAK_MEM_HEADROOM > budget $((GATE_BUDGET_KB / 1024)) MB ($SOAK_MEM_BUDGET of $((MEM_TOTAL_KB / 1024)) MB MemTotal); $GATE_FIT_MSG" >&2
  exit 2
fi
MEMORY_GATE_DECL=$(jq -n --argjson p "$GATE_PREDICTED_KB" --argjson b "$GATE_BUDGET_KB" \
  --argjson h "$SOAK_MEM_HEADROOM" --argjson m "$MEM_TOTAL_KB" \
  '{predicted_kb: $p, budget_kb: $b, headroom: $h, mem_total_kb: $m}')
jq -n --argjson dur "$TOTAL_SECONDS" --argjson cad "$RSS_CADENCE_S" \
  --argjson slack "$SAMPLER_END_SLACK_S" \
  --argjson corruption "$CORRUPTION_DECL" \
  --argjson corruption_spec "$CORRUPTION_SPEC_DECL" \
  --arg srt_profile "$SRT_PROFILE" --arg rist_profile "$RIST_PROFILE" \
  --argjson schedule "$SCHEDULE_DECL" \
  --arg klv_set "$KLV_SET" --argjson klv_seed "$SEED" \
  --arg srt_reconnect_mode "$SRT_RECONNECT_MODE" \
  --argjson publish_leg "$PUBLISH_LEG_DECL" \
  --argjson publish_es_leg "$PUBLISH_ES_LEG_DECL" \
  --argjson whep_leg "$WHEP_LEG_DECL" \
  --argjson memory_gate "$MEMORY_GATE_DECL" \
  '{expected_duration_s: $dur, memory_gate: $memory_gate, rss_cadence_s: $cad, warmup_fraction: 0.1667,
    sampler_end_slack_s: $slack, expected_worker_exits: {},
    corruption: $corruption, corruption_spec: $corruption_spec,
    legs: ({srt: {profile: $srt_profile, schedule: $schedule,
                 klv_set: $klv_set, klv_seed: $klv_seed,
                 reconnect_mode: $srt_reconnect_mode},
           rist: {profile: $rist_profile, schedule: $schedule,
                  klv_set: $klv_set, klv_seed: $klv_seed,
                  reconnect_mode: null}}
          + (if $publish_leg == null then {} else {"rtsp-publish": $publish_leg} end)
          + (if $publish_es_leg == null then {} else {"rtsp-publish-es": $publish_es_leg} end)
          + (if $whep_leg == null then {} else {whep: $whep_leg} end))}' \
  >"$OUTDIR/soak-config.json"

# Fail fast on a declared config that could never pass its own
# completeness verdicts (e.g. an `--hours` value small enough that the
# fixed sampler end slack and warmup consume the whole run) — the same
# `parse_soak_config` checks `report soak` always runs, invoked here
# before any worker launches rather than only discovered hours later at
# teardown. See `report soak --config FILE --validate-only`'s own doc
# comment in main.rs.
"$BIN" report soak --config "$OUTDIR/soak-config.json" --validate-only || {
  echo "soak: soak-config.json failed validation (see above) — refusing to launch a run \
that could never pass its own completeness verdicts" >&2
  exit 2
}

# The rtsp-publish leg's stream, generated and cut BEFORE any worker
# launches so the launch timing below stays as tight as the other legs'.
# One stream of PUBLISH_CONTENT_S seconds, the same generator settings as
# the other legs (rich KLV on this run's seed, realistic AU sizes), cut
# into PUBLISH_GENERATIONS segments at PAT packets — a segment that
# starts on a PAT is a stream a fresh publisher can open. Concatenated,
# the segments are the source byte for byte, so every generation
# continues the previous one's PTS, PCR and continuity counters and the
# receiver judges one unbroken stream. The source's sha256 is kept beside
# the segments and declared in soak-config.json before any worker
# launches: `report soak`'s `delivery_complete_rtsp-publish` requires
# recv's `metrics.stream_sha256` to equal it, which holds only when every
# byte of every generation arrived, the last one included. Segments are
# cut by byte share, so their lengths in seconds only approximate
# PUBLISH_SEGMENT_S. The publisher deletes each segment once it has
# pushed it.
#
# Disk: while the cutter runs, the source and every segment exist
# together, so the peak is twice the stream: about 1.7 GB per hour of run
# for baseline (~0.85 GB/h of stream), ~122 GB for a 72-hour run. Checked
# before anything is written: a 10 s sample of the leg's own profile gives
# the byte rate, and a run whose outdir has less than 1.2 x that peak free
# exits 2 here, naming --no-rtsp-publish.
PUBLISH_SEGMENTS=()
if [[ "$RTSP_PUBLISH" -eq 1 ]]; then
  PUBLISH_SAMPLE="$OUTDIR/rtsp-publish/rate-sample.ts"
  "$BIN" gen --profile "$PUBLISH_PROFILE" --seconds 10 --out "$PUBLISH_SAMPLE" \
    --klv-set "$KLV_SET" --klv-seed "$SEED" --au-sizes realistic ||
    { echo "soak.sh: rate sample gen failed (see above)" >&2; exit 2; }
  PUBLISH_PEAK_KB=$(awk -v b="$(wc -c <"$PUBLISH_SAMPLE")" -v s="$PUBLISH_CONTENT_S" \
    'BEGIN{printf "%d", 2 * b / 10 * s / 1024 + 1}')
  rm -f "$PUBLISH_SAMPLE"
  PUBLISH_FREE_KB=$(df -Pk "$OUTDIR" | awk 'NR==2{print $4}')
  if awk -v f="$PUBLISH_FREE_KB" -v p="$PUBLISH_PEAK_KB" 'BEGIN{exit !(f < 1.2 * p)}'; then
    echo "soak.sh: the rtsp-publish leg pre-generates its stream: peak ~$((PUBLISH_PEAK_KB / 1048576)) GiB \
($PUBLISH_PEAK_KB KiB, source + segments) in $OUTDIR, which has $PUBLISH_FREE_KB KiB free (< 1.2 x peak) — \
free space, shorten --hours, or pass --no-rtsp-publish" >&2
    exit 2
  fi
  echo "soak: generating the rtsp-publish leg's ${PUBLISH_CONTENT_S}s stream in $PUBLISH_GENERATIONS segments..." >&2
  PUBLISH_SOURCE="$OUTDIR/rtsp-publish/source.ts"
  "$BIN" gen --profile "$PUBLISH_PROFILE" --seconds "$PUBLISH_CONTENT_S" --out "$PUBLISH_SOURCE" \
    --klv-set "$KLV_SET" --klv-seed "$SEED" --au-sizes realistic
  sha256sum "$PUBLISH_SOURCE" | awk '{print $1}' >"$OUTDIR/rtsp-publish/source.sha256"
  # Declared before any worker launches, like the rest of the leg; the
  # file stays beside it for a report run against an older config.
  jq --arg sha "$(cat "$OUTDIR/rtsp-publish/source.sha256")" \
    '.legs["rtsp-publish"].source_sha256 = $sha' "$OUTDIR/soak-config.json" \
    >"$OUTDIR/soak-config.json.tmp" &&
    mv "$OUTDIR/soak-config.json.tmp" "$OUTDIR/soak-config.json" &&
    "$BIN" report soak --config "$OUTDIR/soak-config.json" --validate-only ||
    { echo "soak.sh: could not declare the rtsp-publish source digest in soak-config.json" >&2; exit 2; }
  python3 - "$PUBLISH_SOURCE" "$PUBLISH_GENERATIONS" "$OUTDIR/rtsp-publish/segments" <<'PY'
import os, sys
src, n, outdir = sys.argv[1], int(sys.argv[2]), sys.argv[3]
size = os.path.getsize(src)
if size % 188:
    sys.exit(f"soak: {src} is not a whole number of TS packets")
packets = size // 188
# Segment k + 1 opens on the first PAT at or after packet packets * k / n.
targets = [packets * k // n for k in range(1, n)]
seg = 1
out = open(os.path.join(outdir, "gen-1.ts"), "wb")
with open(src, "rb") as f:
    for i in range(packets):
        p = f.read(188)
        is_pat = p[0] == 0x47 and p[1] & 0x40 and ((p[1] & 0x1F) << 8 | p[2]) == 0
        if seg < n and i >= targets[seg - 1] and is_pat:
            out.close()
            seg += 1
            out = open(os.path.join(outdir, f"gen-{seg}.ts"), "wb")
        out.write(p)
out.close()
if seg != n:
    sys.exit(f"soak: found a PAT for only {seg} of {n} segments")
PY
  rm -f "$PUBLISH_SOURCE"
  for ((k = 1; k <= PUBLISH_GENERATIONS; k++)); do
    PUBLISH_SEGMENTS+=("$OUTDIR/rtsp-publish/segments/gen-$k.ts")
  done
fi

# The rtsp-publish-es leg's source: ONE generation (PUBLISH_SEGMENT_S
# seconds) of klv-sync with compact KLV and realistic AU sizes, replayed
# by every publisher session and kept for the whole run. Compact, not
# rich: the rich oracles judge each record by its position in one seeded
# schedule for the whole capture, and this leg restarts that position
# every generation; the count and placement oracles are this leg's KLV
# content check, and rich content stays with the MPEG-TS leg. The server
# re-muxes the push onto its own PIDs, so no byte digest can hold;
# instead `verify` counts the source's KLV records, and
# `delivery_complete_rtsp-publish-es` requires recv to see exactly that
# many per declared generation.
#
# Disk: the source alone, about 0.85 GB per hour of generation length
# (~140 MB for the default 600 s), checked like the MPEG-TS leg's: a 10 s
# sample gives the byte rate, and less than 1.2 x the source free exits 2
# naming --no-rtsp-publish-es.
if [[ "$RTSP_PUBLISH_ES" -eq 1 ]]; then
  PUBLISH_ES_GEN_ARGS=(--profile "$PUBLISH_ES_PROFILE" --klv-set "$PUBLISH_ES_KLV_SET" --klv-seed "$SEED"
    --au-sizes realistic)
  PUBLISH_ES_SAMPLE="$OUTDIR/rtsp-publish-es/rate-sample.ts"
  "$BIN" gen --seconds 10 --out "$PUBLISH_ES_SAMPLE" "${PUBLISH_ES_GEN_ARGS[@]}" ||
    { echo "soak.sh: rtsp-publish-es rate sample gen failed (see above)" >&2; exit 2; }
  PUBLISH_ES_PEAK_KB=$(awk -v b="$(wc -c <"$PUBLISH_ES_SAMPLE")" -v s="$PUBLISH_SEGMENT_S" \
    'BEGIN{printf "%d", b / 10 * s / 1024 + 1}')
  rm -f "$PUBLISH_ES_SAMPLE"
  PUBLISH_ES_FREE_KB=$(df -Pk "$OUTDIR" | awk 'NR==2{print $4}')
  if awk -v f="$PUBLISH_ES_FREE_KB" -v p="$PUBLISH_ES_PEAK_KB" 'BEGIN{exit !(f < 1.2 * p)}'; then
    echo "soak.sh: the rtsp-publish-es leg pre-generates its ${PUBLISH_SEGMENT_S}s source: ~$PUBLISH_ES_PEAK_KB KiB \
in $OUTDIR, which has $PUBLISH_ES_FREE_KB KiB free (< 1.2 x that) — free space, lower PUBLISHER_DROP_PERIOD_S, \
or pass --no-rtsp-publish-es" >&2
    exit 2
  fi
  echo "soak: generating the rtsp-publish-es leg's ${PUBLISH_SEGMENT_S}s source (replayed $PUBLISH_GENERATIONS times)..." >&2
  PUBLISH_ES_SOURCE="$OUTDIR/rtsp-publish-es/source.ts"
  "$BIN" gen --seconds "$PUBLISH_SEGMENT_S" --out "$PUBLISH_ES_SOURCE" "${PUBLISH_ES_GEN_ARGS[@]}" ||
    { echo "soak.sh: rtsp-publish-es source gen failed (see above)" >&2; exit 2; }
  "$BIN" verify --file "$PUBLISH_ES_SOURCE" --expect "$PUBLISH_ES_PROFILE" --seconds "$PUBLISH_SEGMENT_S" \
    --klv-set "$PUBLISH_ES_KLV_SET" --klv-seed "$SEED" --json "$OUTDIR/rtsp-publish-es/source-verify.json" ||
    { echo "soak.sh: the rtsp-publish-es source failed its own verify (see above)" >&2; exit 2; }
  PUBLISH_ES_KLV_PER_GEN=$(jq -r '.metrics.klv_records' "$OUTDIR/rtsp-publish-es/source-verify.json")
  [[ "$PUBLISH_ES_KLV_PER_GEN" =~ ^[1-9][0-9]*$ ]] || {
    echo "soak.sh: the rtsp-publish-es source has no KLV records to count (metrics.klv_records: $PUBLISH_ES_KLV_PER_GEN)" >&2
    exit 2
  }
  jq --argjson n "$PUBLISH_ES_KLV_PER_GEN" \
    '.legs["rtsp-publish-es"].remuxed = true | .legs["rtsp-publish-es"].klv_records_per_generation = $n' \
    "$OUTDIR/soak-config.json" >"$OUTDIR/soak-config.json.tmp" &&
    mv "$OUTDIR/soak-config.json.tmp" "$OUTDIR/soak-config.json" &&
    "$BIN" report soak --config "$OUTDIR/soak-config.json" --validate-only ||
    { echo "soak.sh: could not declare the rtsp-publish-es KLV count in soak-config.json" >&2; exit 2; }
fi

declare -A PIDS

# record_pid <role> <pid> — record_pid_to (lib.sh) with this run's
# pids dir and event log: PIDS[role], `pids/<role>.pid`, LAUNCHED event.
record_pid() { record_pid_to "$OUTDIR/pids" "$EVENTS_LOG" "$1" "$2"; }

# ---------------------------------------------------------------------
# Shared launch arguments
# ---------------------------------------------------------------------
#
# Both legs are launched from the same assembled arrays rather than two
# hand-kept copies of the same flags: the only thing that legitimately
# differs between them is the srt leg's `--outage`/`--managed` and each
# leg's own profile, seed offset and log path.

# The seeded phase schedule (default) or the four fixed knobs. `proxy`
# REFUSES the two together — a schedule overrides loss/jitter/reorder/
# delay per phase, so accepting both would silently run something other
# than what was typed — which is exactly why this is one array rather
# than two independently-passed sets of flags. `--seed` and the srt
# leg's `--outage` are per-run, not per-phase, and stay outside it.
if [[ "$FIXED_IMPAIRMENT" -eq 1 ]]; then
  IMPAIR_ARGS=(--loss "$LOSS_PCT" --jitter "$JITTER_MS" --delay "$DELAY_MS" --reorder "$REORDER")
else
  IMPAIR_ARGS=(--schedule "seed=$SEED,phases=$SCHEDULE_PHASES,phase_s=${SCHEDULE_PHASE_S}s")
fi

# Corruption tap arguments, per leg and per side — empty under
# --no-corrupt. `send --corrupt` without `--corruption-log` (and the
# reverse) is a deliberate usage error, so the two always travel
# together; the receiver's log path is the same file its own sender
# writes, which is the whole attribution mechanism.
SRT_CORRUPT_LOG="$OUTDIR/srt/corruption.jsonl"
RIST_CORRUPT_LOG="$OUTDIR/rist/corruption.jsonl"
SRT_SEND_CORRUPT_ARGS=()
SRT_RECV_CORRUPT_ARGS=()
RIST_SEND_CORRUPT_ARGS=()
RIST_RECV_CORRUPT_ARGS=()
if [[ "$CORRUPT" -eq 1 ]]; then
  SRT_SEND_CORRUPT_ARGS=(--corrupt "$CORRUPT_SPEC" --corruption-log "$SRT_CORRUPT_LOG" --seed "$SRT_CORRUPT_SEED")
  SRT_RECV_CORRUPT_ARGS=(--corruption-log "$SRT_CORRUPT_LOG")
  RIST_SEND_CORRUPT_ARGS=(--corrupt "$CORRUPT_SPEC" --corruption-log "$RIST_CORRUPT_LOG" --seed "$RIST_CORRUPT_SEED")
  RIST_RECV_CORRUPT_ARGS=(--corruption-log "$RIST_CORRUPT_LOG")
fi

# Rich KLV, both sides of both legs. A receiver MUST be told the same
# --klv-set/--klv-seed its sender used: the rich oracles check each
# record's tag set against `fixtures::rich_presence(seed, seq)`, and a
# receiver holding a different seed would judge every record against the
# wrong schedule.
KLV_ARGS=(--klv-set "$KLV_SET" --klv-seed "$SEED")

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
# clears. Starting the proxy
# `SRT_PROXY_WARMUP_S` seconds ahead of recv/send sidesteps it
# entirely: only window 0 is affected by this launch-order artifact,
# and every later window (starting at `OUTAGE_PERIOD_S`, `2 *
# OUTAGE_PERIOD_S`, ...) is exactly the outage this run means to
# exercise a real mid-stream reconnect against.
# SRT receive latency (TSBPD budget), in milliseconds, for BOTH ends of
# this leg's connection. libsrt's 120ms default is SMALLER than the
# impairment this run deliberately applies, and a packet that arrives
# past its play time is DROPPED by TSBPD — real, unrecoverable loss that
# no retransmission can undo. The generated schedule's per-phase ranges
# (see `impair::generate_schedule`'s determinism contract) top out at a
# 300ms reorder hold + 40ms jitter + 60ms base delay = 400ms of
# worst-case one-way delay, and SRT additionally needs room for a couple
# of retransmission rounds over an RTT that the same base delay inflates
# to ~200ms. 1200ms covers both with margin.
#
# Observed: with the
# default budget the srt leg logged 43 `RCV-DROPPED` warnings and its
# receiver reported 38 unattributable `ContinuityJump` events — gaps the
# corruption tap could not explain because nothing in the corruption log
# caused them. Before the tap was switched on, those same gaps were
# merely counted (the soak verifies in `Lossy` mode), which is why this
# mis-sizing went unnoticed until now. The fix is to size the receiver
# for the link being emulated, NOT to relax any verdict.
SRT_LATENCY_MS=1200
SRT_PROXY_WARMUP_S=$((OUTAGE_DUR_S + 30))
# + SAMPLER_END_SLACK_S: see that constant's own doc comment — keeps
# this proxy alive past the sampler's last tick instead of exiting a
# few seconds ahead of it.
SRT_PROXY_RUN_SECONDS=$((TOTAL_SECONDS + SRT_PROXY_WARMUP_S + SAMPLER_END_SLACK_S))

SRT_RECV_PORT=$(free_port)

SRT_PROXY_STDOUT="$OUTDIR/logs/srt-proxy.stdout"
"$BIN" proxy --listen 127.0.0.1:0 --forward "127.0.0.1:$SRT_RECV_PORT" \
  "${IMPAIR_ARGS[@]}" --seed "$SEED" \
  --outage "period=${OUTAGE_PERIOD_S}s,dur=${OUTAGE_DUR_S}s" \
  --stats-json "$OUTDIR/srt/proxy-stats.json" --run-seconds "$SRT_PROXY_RUN_SECONDS" \
  >"$SRT_PROXY_STDOUT" 2>"$OUTDIR/logs/srt-proxy.log" &
record_pid srt-proxy $!
SRT_PROXY_ADDR=$(wait_for_bound_addr "$SRT_PROXY_STDOUT" "$PROXY_ADDR_POLL_TIMEOUT_S" soak)

echo "soak: srt proxy warming up ${SRT_PROXY_WARMUP_S}s past its initial outage window before starting send/recv..." >&2
sleep "$SRT_PROXY_WARMUP_S"

# --no-klv-digest on every send/recv below: without it, both sides
# accumulate one hex digest string per KLV record for the ENTIRE run
# (needed for klv_set_sha256, an order-insensitive fingerprint that has
# to sort every digest before hashing) — confirmed to cost ~3.6-5.7 MiB/h
# of RSS growth that's pure harness
# bookkeeping, unrelated to the library code this soak measures, and
# would otherwise swamp the RSS-slope evidence over 72h. The short
# interop-matrix cells run-matrix.sh drives do NOT pass this flag —
# their transparent-tier byte comparisons need the hash, and their runs
# are seconds long, so the accumulation never mattered there.
#
# The receiver starts BEFORE its sender on both legs — it must not miss
# the start of the stream, and under --corrupt it additionally waits
# (bounded by recv's own 15s no-data budget) for the sender to create
# the corruption log it is going to read. The $SETTLE between them is
# comfortably inside that budget.
"$BIN" recv --url "srt://:$SRT_RECV_PORT?mode=listener&latency=$SRT_LATENCY_MS" --expect "$SRT_PROFILE" \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/srt/recv-report.json" --no-klv-digest \
  --managed "${KLV_ARGS[@]}" "${SRT_RECV_CORRUPT_ARGS[@]}" \
  >"$OUTDIR/logs/srt-recv.log" 2>&1 &
record_pid srt-recv $!
sleep "$SETTLE"

# --au-sizes realistic (both legs' senders): GOP-structured multi-KB
# AUs at ~1.7 Mb/s — the soak measures endurance under a real encoder's
# traffic shape, not the interop matrix's tiny compact fixtures. See
# `fixtures::AuSizeMode`.
"$BIN" send --profile "$SRT_PROFILE" --url "srt://$SRT_PROXY_ADDR?latency=$SRT_LATENCY_MS" --managed \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/srt/send-report.json" --no-klv-digest \
  --au-sizes realistic --reconnect-mode "$SRT_RECONNECT_MODE" \
  "${KLV_ARGS[@]}" "${SRT_SEND_CORRUPT_ARGS[@]}" \
  >"$OUTDIR/logs/srt-send.log" 2>&1 &
record_pid srt-send $!

# ---------------------------------------------------------------------
# rist leg: send -> proxy (impairment, NO outage) -> recv
# ---------------------------------------------------------------------

# librist recovery buffer (`RistConfig::buffer` -> librist's
# `recovery_length_min`), in milliseconds, for BOTH ends of this leg.
# Same derivation and the same number as SRT_LATENCY_MS above: the
# generated schedule's worst case is a 300ms reorder hold + 40ms jitter +
# 60ms base delay = 400ms of one-way delay, plus room for a couple of
# retransmission rounds over an RTT the same base delay inflates to
# ~200ms.
#
# tst-rist's default is 200ms — under the one-way delay on its own, and
# the srt leg's counterpart was the one that got sized. Found on this
# arc's 1-hour smoke: with the default, librist auto-scaled its flow
# buffer down to 450ms, logged 148 "Dropped N late packets" and 49
# "Dropped N packets due to buffers being full" errors, and this leg's
# receiver reported 155 events no injection explained — against ZERO on
# the srt leg over the same schedule and the same hour. Late-dropped
# packets are real, unrecoverable loss no retransmission can undo, which
# is exactly the mis-sizing SRT_LATENCY_MS documents. The fix is to size
# the receiver for the link being emulated, NOT to relax any verdict.
RIST_BUFFER_MS=$SRT_LATENCY_MS

RIST_RECV_PORT=$(free_port)
"$BIN" recv --url "rist://@0.0.0.0:$RIST_RECV_PORT?buffer=$RIST_BUFFER_MS" --expect "$RIST_PROFILE" \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rist/recv-report.json" --no-klv-digest \
  "${KLV_ARGS[@]}" "${RIST_RECV_CORRUPT_ARGS[@]}" \
  >"$OUTDIR/logs/rist-recv.log" 2>&1 &
record_pid rist-recv $!
sleep "$SETTLE"

RIST_PROXY_STDOUT="$OUTDIR/logs/rist-proxy.stdout"
# + SAMPLER_END_SLACK_S: same reasoning as the srt proxy's own
# --run-seconds above.
RIST_PROXY_RUN_SECONDS=$((TOTAL_SECONDS + SAMPLER_END_SLACK_S))
"$BIN" proxy --listen 127.0.0.1:0 --forward "127.0.0.1:$RIST_RECV_PORT" \
  "${IMPAIR_ARGS[@]}" --seed "$SEED" \
  --stats-json "$OUTDIR/rist/proxy-stats.json" --run-seconds "$RIST_PROXY_RUN_SECONDS" \
  >"$RIST_PROXY_STDOUT" 2>"$OUTDIR/logs/rist-proxy.log" &
record_pid rist-proxy $!
RIST_PROXY_ADDR=$(wait_for_bound_addr "$RIST_PROXY_STDOUT" "$PROXY_ADDR_POLL_TIMEOUT_S" soak)

"$BIN" send --profile "$RIST_PROFILE" --url "rist://$RIST_PROXY_ADDR?buffer=$RIST_BUFFER_MS" \
  --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rist/send-report.json" --no-klv-digest \
  --au-sizes realistic "${KLV_ARGS[@]}" "${RIST_SEND_CORRUPT_ARGS[@]}" \
  >"$OUTDIR/logs/rist-send.log" 2>&1 &
record_pid rist-send $!

# ---------------------------------------------------------------------
# rtsp-publish leg: publisher loop -> recv's publish mount (no proxy)
# ---------------------------------------------------------------------
#
# `recv` binds an RTSP server with one publish mount (`rtsp-publish://`)
# and an external publisher RECORDs into it: GStreamer's rtspclientsink,
# pushing the segments cut above as RFC 2250 MP2T over TCP-interleaved
# RTP, which the mount hands on byte for byte. One publisher session per
# segment: at each segment's end gst-launch-1.0 sends EOS and exits, the
# session ends, and the loop's next publisher ANNOUNCEs again — the drop
# this leg exists to exercise, every PUBLISH_SEGMENT_S seconds. The drop
# is graceful (EOS, then the session closes); an abrupt publisher loss is
# not exercised here. The mount stays open between publishers, and recv's
# report counts the publishers that ended (`publish_mount.generation`),
# which `report soak` holds to the declared PUBLISH_GENERATIONS as
# `publisher_generations_rtsp-publish`: the PUBLISH_TAIL_S tail lets the
# last publisher end before recv's deadline (one cut off there exits
# nonzero and fails `worker_exits` too).
#
# A restart gap cannot end the capture: recv's 15 s no-data deadline
# applies only until its FIRST event; from then on its deadline is fixed
# at first event + --seconds + 2 s, and silence between publishers does
# not move it (`recv::recv_over_transport_with_layout`).
#
# Judged --strict, like the matrix's rtsp-publish/gst-push-mp2t cell: the
# path is lossless (loopback TCP, no proxy) and the generations
# concatenate to one unbroken stream, so a discontinuity is a finding.
# No corruption tap: the publisher replays a generated file, it is not a
# tst-interop sender. --no-klv-digest for the same RSS reason as the other
# legs.
#
# Worker roles: `rtsp-publish-recv`, and `rtsp-publish-publisher` = the
# loop below, a long-lived subshell. Each gst-launch-1.0 it starts exits
# by design at the end of its generation, so the tool is never a worker
# itself; a generation that exits nonzero (or overruns its segment by 60 s
# and is killed) is logged and makes the loop exit 1 at the end of the run,
# which fails `worker_exits`. After the last generation the loop idles
# until TOTAL_SECONDS after its own start, so the sampler and supervisor
# see it alive for the whole run like every other worker.
if [[ "$RTSP_PUBLISH" -eq 1 ]]; then
  RTSP_PUBLISH_PORT=$(free_port tcp)
  RTSP_PUBLISH_URL="rtsp://127.0.0.1:$RTSP_PUBLISH_PORT/soak"
  "$BIN" recv --url "rtsp-publish://127.0.0.1:$RTSP_PUBLISH_PORT/soak" --expect "$PUBLISH_PROFILE" \
    --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rtsp-publish/recv-report.json" --no-klv-digest \
    --strict "${KLV_ARGS[@]}" \
    >"$OUTDIR/logs/rtsp-publish-recv.log" 2>&1 &
  record_pid rtsp-publish-recv $!
  sleep "$SETTLE"

  publisher_loop() {
    local end=$(($(date +%s) + TOTAL_SECONDS)) k=0 failed=0 rc seg gst_pid=""
    trap '[[ -z "$gst_pid" ]] || kill "$gst_pid" 2>/dev/null; exit 143' TERM
    echo "publisher: ${#PUBLISH_SEGMENTS[@]} generation(s) of ~${PUBLISH_SEGMENT_S}s into $RTSP_PUBLISH_URL"
    for seg in "${PUBLISH_SEGMENTS[@]}"; do
      k=$((k + 1))
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: generation $k ANNOUNCE ($seg)"
      # In the background and reaped with `wait`, so the TERM trap above
      # runs at once on a fail-fast kill instead of after the generation.
      timeout --kill-after=5 "$((PUBLISH_SEGMENT_S + 60))" \
        gst-launch-1.0 filesrc "location=$seg" ! tsparse set-timestamps=true ! \
        rtspclientsink "location=$RTSP_PUBLISH_URL" protocols=tcp &
      gst_pid=$!
      rc=0
      wait "$gst_pid" || rc=$?
      gst_pid=""
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: generation $k ended rc=$rc"
      [[ "$rc" -eq 0 ]] || failed=$((failed + 1))
      rm -f "$seg"
    done
    until [[ $(date +%s) -ge $end ]]; do
      sleep 5 &
      gst_pid=$!
      wait "$gst_pid" || true
      gst_pid=""
    done
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: done, $failed of $k generation(s) failed"
    [[ "$failed" -eq 0 ]]
  }
  publisher_loop >"$OUTDIR/logs/rtsp-publish-publisher.log" 2>&1 &
  record_pid rtsp-publish-publisher $!
fi

# ---------------------------------------------------------------------
# rtsp-publish-es leg: elementary publisher loop -> recv's publish mount
# ---------------------------------------------------------------------
#
# The same shape as the rtsp-publish leg, but the publisher splits the
# source back into its elementary streams and rtspclientsink pushes them
# as two RTP tracks: H.264 (RFC 6184) and KLV (RFC 6597). The mount
# re-muxes them onto its own PIDs, so recv judges the capture --remuxed
# (the generator-layout oracles are skipped, every content oracle runs)
# and --klv-placement holds each KLV record to the video access unit it
# was generated with, across PUBLISH_GENERATIONS replays of one source.
#
# NOT --strict: every generation restarts the source's PTS and PCR, a
# legitimate backward step on this leg. --klv-placement declares the
# budget: the placement oracle accepts at most generations - 1 boundaries,
# and the receiver's invariants excuse one backward PTS step per PID per
# accepted boundary, plus the demuxer's PTS / PCR anomalies that are that
# boundary's signature (counted in the report's klv_placement block).
# Anything beyond the budget fails as on any other leg, and the generation
# count is held to the declaration by `publisher_generations_rtsp-publish-es`.
#
# Worker roles `rtsp-publish-es-recv` and `rtsp-publish-es-publisher`,
# with the MPEG-TS leg's lifecycle: one gst-launch-1.0 per generation, a
# failed or overrun generation makes the loop exit 1 at the end of the
# run, and the loop idles to TOTAL_SECONDS. The source is never deleted:
# every generation replays it.
if [[ "$RTSP_PUBLISH_ES" -eq 1 ]]; then
  RTSP_PUBLISH_ES_PORT=$(free_port tcp)
  RTSP_PUBLISH_ES_URL="rtsp://127.0.0.1:$RTSP_PUBLISH_ES_PORT/soak-es"
  "$BIN" recv --url "rtsp-publish://127.0.0.1:$RTSP_PUBLISH_ES_PORT/soak-es" --expect "$PUBLISH_ES_PROFILE" \
    --seconds "$TOTAL_SECONDS" --json "$OUTDIR/rtsp-publish-es/recv-report.json" --no-klv-digest \
    --remuxed --klv-placement "$PUBLISH_GENERATIONS" --klv-set "$PUBLISH_ES_KLV_SET" --klv-seed "$SEED" \
    >"$OUTDIR/logs/rtsp-publish-es-recv.log" 2>&1 &
  record_pid rtsp-publish-es-recv $!
  sleep "$SETTLE"

  publisher_es_loop() {
    local end=$(($(date +%s) + TOTAL_SECONDS)) k=0 failed=0 rc gst_pid=""
    trap '[[ -z "$gst_pid" ]] || kill "$gst_pid" 2>/dev/null; exit 143' TERM
    echo "publisher: $PUBLISH_GENERATIONS generation(s) of ${PUBLISH_SEGMENT_S}s, each replaying $PUBLISH_ES_SOURCE, into $RTSP_PUBLISH_ES_URL"
    for ((k = 1; k <= PUBLISH_GENERATIONS; k++)); do
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: generation $k ANNOUNCE"
      timeout --kill-after=5 "$((PUBLISH_SEGMENT_S + 60))" \
        gst-launch-1.0 filesrc "location=$PUBLISH_ES_SOURCE" ! tsdemux name=d \
        d. ! queue ! h264parse ! s.sink_0 \
        d. ! queue ! meta/x-klv ! s.sink_1 \
        rtspclientsink name=s "location=$RTSP_PUBLISH_ES_URL" protocols=tcp &
      gst_pid=$!
      rc=0
      wait "$gst_pid" || rc=$?
      gst_pid=""
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: generation $k ended rc=$rc"
      [[ "$rc" -eq 0 ]] || failed=$((failed + 1))
    done
    k=$((k - 1))
    until [[ $(date +%s) -ge $end ]]; do
      sleep 5 &
      gst_pid=$!
      wait "$gst_pid" || true
      gst_pid=""
    done
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) publisher: done, $failed of $k generation(s) failed"
    [[ "$failed" -eq 0 ]]
  }
  publisher_es_loop >"$OUTDIR/logs/rtsp-publish-es-publisher.log" 2>&1 &
  record_pid rtsp-publish-es-publisher $!
fi

# ---------------------------------------------------------------------
# whep leg: serve-whep -> headless browser, relaunched every WHEP_RECONNECT_S
# ---------------------------------------------------------------------
#
# `tst-interop serve-whep` binds a WHEP server with one MP2T mount and
# serves TOTAL_SECONDS of the leg's profile (its KLV schedule, muxed
# in-process and fed through `push_ts`, with a real H.264 GOP as the
# video) wall-clock paced, for the whole run. The viewer loop plays it
# with headless Chrome on the matrix's page (scripts/interop/whep-page.html
# loading the reference player under docs/webrtc/ unmodified, `dedupe:
# false` so a record arrives from BOTH carriages), the way the matrix's
# whep/* cells do — the browser command and the console parsing below
# are theirs, kept in step by hand (run-matrix.sh's run_whep_browser).
# One browser per WHEP_RECONNECT_S slot, counted from the loop's start:
# each is ended by `timeout` WHEP_RELAUNCH_GAP_S before its slot ends,
# and the next starts on its own slot — the reconnect this leg exists to
# exercise, WHEP_CONNECTIONS - 1 times. The server is never told: a
# viewer leaving is a session the server closes about 40 s later, after
# ICE reports it disconnected (`sessions_reaped_disconnected`, the 30 s
# disconnect grace), a viewer arriving a session it starts at the next
# keyframe, with the data channel opening a moment later.
#
# The console of every connection is parsed into `whep/viewer-report.json`
# (one object per connection: the once-a-second FRAMES and AUDIO samples,
# the records per carriage and their set digests, the records seen on one
# carriage only and where, the copies disagreeing on (pts, rtp timestamp),
# the error lines), which `report soak --whep-viewer-report` judges
# beside the server's own counters (`serve-whep --json`,
# `--whep-serve-report`): `whep_frames_continuous` (every browser was
# ended by its own timeout, reported no error, decoded its first frame
# within 5 samples, never stalled over 2 s and decoded at least 0.9 of
# the served frame rate over its remaining samples; the server dropped
# no packet for a full queue), `whep_klv_sei_equals_dc` (per connection
# the SEI set equals the data-channel set, each carriage at least 0.9 of
# the served record rate, every record's two copies on the same (pts,
# rtp timestamp); a record on one carriage only allowed in the first 3
# or last 2 of its carriage — the data channel opens after the session
# is connected, and the kill lands between two copies; the server
# dropped no record), `whep_reconnects_observed` (the viewer made
# exactly the declared connections on the declared slot, and the server
# saw exactly that many sessions). The matrix cells, not this leg, gate
# the KLV content against the generator: here a connection's window is
# wherever its slot fell in the schedule.
#
# Worker roles: `whep-serve`, and `whep-viewer` = the loop below, a
# long-lived subshell; each Chrome it starts exits by design at the end
# of its slot, so the browser is never a worker itself and, like the
# publish legs' tool, its memory is not sampled. A browser that exits
# other than by its timeout is logged and makes the loop exit 1 at the
# end of the run, which fails `worker_exits`. After the last connection
# the loop idles until TOTAL_SECONDS after its own start.
#
# serve-whep waits up to WHEP_VIEWER_WAIT_S for the first browser's
# session before its first push, so the first connection sees the
# schedule from its start (the matrix cells wait the same way); its push
# then runs TOTAL_SECONDS from that moment and the server lingers 3 s,
# so it outlives the last connection, which ends WHEP_RELAUNCH_GAP_S
# before the loop's TOTAL_SECONDS.
WHEP_VIEWER_WAIT_S=15
if [[ "$WHEP" -eq 1 ]]; then
  WHEP_PORT=$(free_port tcp)
  WHEP_MEDIA_PORT=$(free_port udp)
  WHEP_SERVE_STDOUT="$OUTDIR/logs/whep-serve.stdout"
  "$BIN" serve-whep --url "whep://127.0.0.1:$WHEP_PORT?media_port=$WHEP_MEDIA_PORT" --mount /soak \
    --profile "$WHEP_PROFILE" --mount-kind "$WHEP_MOUNT_KIND" --seconds "$TOTAL_SECONDS" \
    --wait-viewer "$WHEP_VIEWER_WAIT_S" --json "$OUTDIR/whep/serve-counters.json" \
    >"$WHEP_SERVE_STDOUT" 2>"$OUTDIR/logs/whep-serve.log" &
  record_pid whep-serve $!
  WHEP_URL=$(wait_for_bound_addr "$WHEP_SERVE_STDOUT" "$PROXY_ADDR_POLL_TIMEOUT_S" soak serving)
  WHEP_PAGE="file://$SCRIPT_DIR/whep-page.html?url=$WHEP_URL"

  # whep_connection_json <k> <started_utc> <rc> <console> <stderr> — one
  # viewer-report connection object from a browser's console, read the
  # way run-matrix.sh's whep cells read theirs: the FRAMES / AUDIO
  # samples in order, the records per carriage with their set digests
  # (sha256 over the LC_ALL=C-sorted per-record digests, verify.rs's
  # klv_set_hash), every record on one carriage only with its position
  # on that carriage (from its head and from its tail, so the report can
  # tell an edge from a mid-connection loss), the copies disagreeing on
  # (pts, rtp timestamp), and the page's / the browser's error lines.
  whep_connection_json() {
    local k=$1 started=$2 rc=$3 console=$4 err=$5
    local frames audio sei dc sei_sha dc_sha unpaired disagree page_errors console_errors first_error
    frames=$(awk '$1 == "FRAMES" { print $2 + 0 }' "$console" | jq -s '.')
    audio=$(awk '$1 == "AUDIO" { print $2 + 0 }' "$console" | jq -s '.')
    sei=$(awk '$1 == "KLV" && $4 == "sei"' "$console" | wc -l)
    dc=$(awk '$1 == "KLV" && $4 == "dc"' "$console" | wc -l)
    sei_sha=$(awk '$1 == "KLV" && $4 == "sei" { print $3 }' "$console" | LC_ALL=C sort | tr -d '\n' | sha256sum | cut -d' ' -f1)
    dc_sha=$(awk '$1 == "KLV" && $4 == "dc" { print $3 }' "$console" | LC_ALL=C sort | tr -d '\n' | sha256sum | cut -d' ' -f1)
    unpaired=$(awk '
      $1 == "KLV" && $4 == "sei" { ns++; sei[$3] = ns }
      $1 == "KLV" && $4 == "dc" { nd++; dc[$3] = nd }
      END {
        for (d in sei) if (!(d in dc)) printf "{\"via\":\"sei\",\"from_head\":%d,\"from_tail\":%d}\n", sei[d] - 1, ns - sei[d]
        for (d in dc) if (!(d in sei)) printf "{\"via\":\"dc\",\"from_head\":%d,\"from_tail\":%d}\n", dc[d] - 1, nd - dc[d]
      }' "$console" | jq -s '.')
    disagree=$(awk '
      $1 == "KLV" { key = $3; val = $2 " " $5; if ($4 == "sei") sei[key] = val; else if ($4 == "dc") dc[key] = val }
      END { n = 0; for (k in sei) if ((k in dc) && sei[k] != dc[k]) n++; print n + 0 }' "$console")
    page_errors=$(grep -c '^ERROR' "$console") || page_errors=0
    console_errors=$(grep -c 'ERROR:CONSOLE' "$err") || console_errors=0
    first_error=$({ grep -m1 '^ERROR' "$console" || true; grep -m1 'ERROR:CONSOLE' "$err" | sed 's/.*:CONSOLE[^]]*\] //' || true; } | head -1) || first_error=""
    jq -n --argjson index "$k" --arg started "$started" --argjson rc "$rc" \
      --argjson frames "$frames" --argjson audio "$audio" --argjson sei "$sei" --argjson dc "$dc" \
      --arg sei_sha "$sei_sha" --arg dc_sha "$dc_sha" --argjson unpaired "$unpaired" \
      --argjson disagree "$disagree" --argjson page_errors "$page_errors" \
      --argjson console_errors "$console_errors" --arg first_error "$first_error" \
      '{index: $index, started_utc: $started, browser_rc: $rc, frames: $frames, audio: $audio,
        klv_sei: $sei, klv_dc: $dc, klv_sei_set_sha256: $sei_sha, klv_dc_set_sha256: $dc_sha,
        klv_unpaired: $unpaired, klv_copies_disagreeing: $disagree,
        page_errors: $page_errors, console_errors: $console_errors,
        first_error: (if $first_error == "" then null else $first_error end)}'
  }

  whep_viewer_loop() {
    local start end k slot rc failed=0 chrome_pid="" console err started
    local profile_dir="$OUTDIR/whep/viewer/profile" connections="$OUTDIR/whep/viewer/connections.jsonl"
    start=$(date +%s)
    end=$((start + TOTAL_SECONDS))
    trap '[[ -z "$chrome_pid" ]] || kill "$chrome_pid" 2>/dev/null; exit 143' TERM
    echo "viewer: $WHEP_CONNECTIONS connection(s) of ${WHEP_CONNECTION_S}s, one every ${WHEP_RECONNECT_S}s, $WHEP_BROWSER ($("$WHEP_BROWSER" --version 2>/dev/null | head -1)) on $WHEP_PAGE"
    : >"$connections"
    for ((k = 1; k <= WHEP_CONNECTIONS; k++)); do
      # Absolute slots, like the sampler's ticks: a slow teardown never
      # delays the connections after it.
      slot=$((start + (k - 1) * WHEP_RECONNECT_S))
      until [[ $(date +%s) -ge $slot ]]; do
        sleep 1 &
        chrome_pid=$!
        wait "$chrome_pid" || true
        chrome_pid=""
      done
      console="$OUTDIR/whep/viewer/conn-$k.console.txt"
      err="$OUTDIR/whep/viewer/conn-$k.stderr"
      rm -rf "$profile_dir"
      started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
      echo "$started viewer: connection $k start"
      # In the background and reaped with `wait`, so the TERM trap above
      # runs at once on a fail-fast kill instead of after the slot. The
      # flags are run_whep_browser's (run-matrix.sh); 124 = ended by
      # timeout, the intended end.
      timeout --kill-after=5 "${WHEP_CONNECTION_S}s" \
        "$WHEP_BROWSER" --headless=new --no-sandbox --disable-gpu \
        --autoplay-policy=no-user-gesture-required --enable-logging=stderr --log-level=0 \
        --no-first-run --disable-background-networking --allow-file-access-from-files \
        "--user-data-dir=$profile_dir" "$WHEP_PAGE" >/dev/null 2>"$err" &
      chrome_pid=$!
      rc=0
      wait "$chrome_pid" || rc=$?
      chrome_pid=""
      # Chrome 155 writes `[...:INFO:CONSOLE:33] "text", source: ...`
      # (older builds `CONSOLE(33)`); keep the text of every console line.
      sed -n 's/.*:CONSOLE[^]]*\] "\(.*\)", source: .*/\1/p' "$err" >"$console"
      # Nothing reads the raw stderr again once the console is extracted
      # (whep_connection_json greps it for ERROR:CONSOLE, so the gzip
      # comes after): about 3.5 MB per 10-minute connection, 2 GB over a
      # 72-hour run uncompressed, ~0.3 MB each gzipped.
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) viewer: connection $k ended rc=$rc: $(grep -c '' "$console") console line(s), $(awk '$1 == "FRAMES" { v = $2 } END { print v + 0 }' "$console") frames, KLV via sei $(awk '$1 == "KLV" && $4 == "sei"' "$console" | wc -l) / dc $(awk '$1 == "KLV" && $4 == "dc"' "$console" | wc -l)"
      [[ "$rc" -eq 124 ]] || failed=$((failed + 1))
      whep_connection_json "$k" "$started" "$rc" "$console" "$err" >>"$connections"
      gzip -f "$err"
    done
    rm -rf "$profile_dir"
    jq -s --argjson reconnect_s "$WHEP_RECONNECT_S" --argjson connection_s "$WHEP_CONNECTION_S" \
      '{reconnect_s: $reconnect_s, connection_s: $connection_s, connections: .}' \
      "$connections" >"$OUTDIR/whep/viewer-report.json"
    until [[ $(date +%s) -ge $end ]]; do
      sleep 5 &
      chrome_pid=$!
      wait "$chrome_pid" || true
      chrome_pid=""
    done
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) viewer: done, $failed of $WHEP_CONNECTIONS connection(s) ended other than by their timeout"
    [[ "$failed" -eq 0 ]]
  }
  whep_viewer_loop >"$OUTDIR/logs/whep-viewer.log" 2>&1 &
  record_pid whep-viewer $!
fi

# ---------------------------------------------------------------------
# Sampler: every 30s, VmRSS of every worker PID -> rss.csv; on the same
# tick, CPU ticks/threads/fds per PID -> proc.csv and host load/memory ->
# host.csv
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

# sample_loop (lib.sh) re-reads pids/*.pid every tick and writes one
# rss.csv + proc.csv row per worker and one host.csv row per tick; the
# sampler's own pidfile is excluded. Every worker's pidfile is written
# above, before this launch.
SAMPLE_EVENTS_LOG="$EVENTS_LOG"
sample_loop "$SAMPLER_DEADLINE" "$START_EPOCH" "$RSS_CSV" "$PROC_CSV" "$HOST_CSV" \
  "$OUTDIR/pids" "$RSS_CADENCE_S" sampler &
record_pid sampler $!

# ---------------------------------------------------------------------
# Supervisor: fail fast on a mid-run worker death
# ---------------------------------------------------------------------
#
# See README.md's "Outputs and the supervisor" for why this exists
# (run 1's senders died 14.5h in; the waits below can't notice until
# the 72h deadline). Polls every worker + the sampler every 30s until
# SUPERVISOR_GRACE_S before the nominal deadline; end-of-run process
# exits inside that grace window are normal staggering and are left to
# the ordinary waits below.
ALL_ROLES=(srt-recv srt-proxy srt-send rist-recv rist-proxy rist-send)
[[ "$RTSP_PUBLISH" -eq 0 ]] || ALL_ROLES+=(rtsp-publish-recv rtsp-publish-publisher)
[[ "$RTSP_PUBLISH_ES" -eq 0 ]] || ALL_ROLES+=(rtsp-publish-es-recv rtsp-publish-es-publisher)
[[ "$WHEP" -eq 0 ]] || ALL_ROLES+=(whep-serve whep-viewer)
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
# `recv`'s own exit code reflects its VerifyReport's `pass` (0/1) — a
# verify-failing recv now fails the run TWICE over: `report soak`'s
# `recv_invariants_<leg>` verdict judges the JSON content, and this
# process's own nonzero exit status, recorded below in exits.json, also
# fails `worker_exits` (an empty `expected_worker_exits` means every
# role must exit 0). `send`/`proxy` exiting nonzero is unexpected (send
# retries forever under `--managed`; proxy has no failure path once
# bound) and is likewise judged purely through `worker_exits`. This
# script still proceeds to build the report regardless of what it
# finds: discarding hours of already-collected evidence over one
# process's exit code would be a worse outcome than a report that names
# the problem. (On the fail-fast path above, the freshly-killed workers
# reap here too — nonzero, so their status lands in exits.json as
# well.)
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
[[ "$RTSP_PUBLISH" -eq 0 ]] || REPORT_ARGS+=(--rtsp-publish-recv-report "$OUTDIR/rtsp-publish/recv-report.json")
[[ "$RTSP_PUBLISH_ES" -eq 0 ]] || REPORT_ARGS+=(--publish-leg "rtsp-publish-es=$OUTDIR/rtsp-publish-es/recv-report.json")
[[ "$WHEP" -eq 0 ]] || REPORT_ARGS+=(--whep-viewer-report "$OUTDIR/whep/viewer-report.json" --whep-serve-report "$OUTDIR/whep/serve-counters.json")
[[ -z "$RSS_SLOPE_THRESHOLD" ]] || REPORT_ARGS+=(--rss-slope-threshold-kb-per-hour "$RSS_SLOPE_THRESHOLD")

REPORT_RC=0
"$BIN" "${REPORT_ARGS[@]}" || REPORT_RC=$?

{
  echo "=== soak summary ==="
  echo "outdir: $OUTDIR"
  echo "sample misses: $(grep -c ' SAMPLE-MISS ' "$EVENTS_LOG" 2>/dev/null || true) (live process, empty RSS read, no row written)"
  echo "hours: $HOURS  seed: $SEED  outage_period_s: $OUTAGE_PERIOD_S  outage_dur_s: $OUTAGE_DUR_S"
  echo "profiles: srt=$SRT_PROFILE rist=$RIST_PROFILE rtsp-publish=$PUBLISH_PROFILE rtsp-publish-es=$PUBLISH_ES_PROFILE whep=$WHEP_PROFILE (--profile $PROFILE)  klv_set: rich (rtsp-publish-es, whep: compact)  au_sizes: realistic (whep: a real GOP)"
  if [[ "$RTSP_PUBLISH" -eq 1 ]]; then
    echo "rtsp-publish: $PUBLISH_GENERATIONS publisher generation(s) of ~${PUBLISH_SEGMENT_S}s (PUBLISHER_DROP_PERIOD_S=$PUBLISHER_DROP_PERIOD_S), source sha256 $(cat "$OUTDIR/rtsp-publish/source.sha256")"
  else
    echo "rtsp-publish: leg disabled (--no-rtsp-publish)"
  fi
  if [[ "$RTSP_PUBLISH_ES" -eq 1 ]]; then
    echo "rtsp-publish-es: $PUBLISH_GENERATIONS publisher generation(s), each replaying one ${PUBLISH_SEGMENT_S}s source (PUBLISHER_DROP_PERIOD_S=$PUBLISHER_DROP_PERIOD_S), $PUBLISH_ES_KLV_PER_GEN KLV record(s) per generation"
  else
    echo "rtsp-publish-es: leg disabled (--no-rtsp-publish-es)"
  fi
  if [[ "$WHEP" -eq 1 ]]; then
    echo "whep: $WHEP_CONNECTIONS browser connection(s) of ${WHEP_CONNECTION_S}s, one every ${WHEP_RECONNECT_S}s (WHEP_RECONNECT_S=$WHEP_RECONNECT_S), $WHEP_MOUNT_KIND mount, $WHEP_BROWSER ($("$WHEP_BROWSER" --version 2>/dev/null | head -1))"
  else
    echo "whep: leg disabled (--no-whep)"
  fi
  echo "reconnect_mode (declared): srt=$SRT_RECONNECT_MODE  rist=none (sender not managed)"
  if [[ "$FIXED_IMPAIRMENT" -eq 1 ]]; then
    echo "impairment: FIXED  loss_pct: $LOSS_PCT  jitter_ms: $JITTER_MS  delay_ms: $DELAY_MS  reorder: $REORDER"
  else
    echo "impairment: SCHEDULE seed=$SEED phases=$SCHEDULE_PHASES phase_s=$SCHEDULE_PHASE_S"
  fi
  if [[ "$CORRUPT" -eq 1 ]]; then
    echo "corruption: $CORRUPT_SPEC  seeds: srt=$SRT_CORRUPT_SEED rist=$RIST_CORRUPT_SEED"
  else
    echo "corruption: disabled (--no-corrupt)"
  fi
  echo "worker exits: $(cat "$OUTDIR/exits.json")"
  echo "recorded (not gated): proc.csv (cpu ticks/threads/fds per pid), host.csv (load/mem), provenance.json (source $(jq -r '.source.describe // "unknown"' "$OUTDIR/provenance.json"))"
  [[ -z "$PREMATURE_DEATH" ]] || echo "PREMATURE DEATH: $PREMATURE_DEATH (fail-fast — see soak-FAILED + soak-events.log)"
  echo
  # On the fail-fast path `report soak` typically exits 2 with no
  # output file (a killed recv never writes its report JSON) — the
  # summary must still get written rather than dying here under
  # `pipefail` on the missing file.
  if [[ -s "$OUTDIR/soak-results.json" ]]; then
    # Per leg: what was sent and received, and beside it the managed
    # sender's own account of its reconnects and gap buffer. Recorded,
    # not gated — and the gap counters are not the whole of an outage's
    # loss, which is why the access-unit totals are on the same line.
    jq -r '.legs[]
      | "\(.leg): video AUs sent=\(.send_video_aus) received=\(.recv_video_aus)  managed send (recorded, not gated): "
        + (if .managed_send == null then "none (sender not managed)"
           else (.managed_send
             | "mode=\(.reconnect_mode // "unrecorded") overflow_policy=\(.overflow_policy // "unrecorded")"
               + " reconnect_attempts=\(.reconnect_attempts) reconnect_successes=\(.reconnect_successes)"
               + " gap_messages_dropped=\(.gap_messages_dropped) gap_bytes_dropped=\(.gap_bytes_dropped)"
               + " gap_buffer_capacity=\(.gap_buffer_capacity)")
           end)' "$OUTDIR/soak-results.json"
    echo "note: gap_messages_dropped/gap_bytes_dropped count what the gap buffer evicted after the sender"
    echo "      knew the link was down. What the transport had already accepted and never delivered in the"
    echo "      seconds before it noticed the break is not in them (measured: most of an outage's loss)."
    jq -r '(.verdicts | map({(.name): .pass}) | add) as $v
      | .publish_legs[]
      | "\(.leg): video AUs received=\(.recv_video_aus)  publishers ended=\(.publish_mount.generation // "unrecorded")"
        + " (floor \(.min_generations // "none"))  delivery_complete: "
        + ($v["delivery_complete_" + .leg] | if . == null then "not judged" else tostring end)' \
      "$OUTDIR/soak-results.json"
    jq -r '(.verdicts | map({(.name): .pass}) | add) as $v
      | (.whep_legs // [])[]
      | "\(.leg): browser connections=\(.connections_observed) (declared \(.viewer_connections_declared // "none"),"
        + " server sessions \(.server_sessions_total))  frames decoded=\(.frames_decoded)"
        + "  KLV via sei=\(.klv_sei_records) dc=\(.klv_dc_records) (unpaired at edges \(.klv_unpaired_at_edges))"
        + "  frames_continuous: " + ($v["whep_frames_continuous_" + .leg] | if . == null then "not judged" else tostring end)
        + "  klv_sei_equals_dc: " + ($v["whep_klv_sei_equals_dc_" + .leg] | if . == null then "not judged" else tostring end)
        + "  reconnects_observed: " + ($v["whep_reconnects_observed_" + .leg] | if . == null then "not judged" else tostring end)' \
      "$OUTDIR/soak-results.json"
    echo
    jq '{overall_pass, run_duration_s, expected_duration_s, warmup_s, rss_slope_threshold_kb_per_hour,
         rss_slopes, coverage, process_exits, worker_exits, legs, publish_legs, whep_legs, limitations}' "$OUTDIR/soak-results.json"
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
