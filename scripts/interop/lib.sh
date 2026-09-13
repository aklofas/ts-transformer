#!/usr/bin/env bash
# Shared helpers for the transport-axis interop orchestrator
# (run-matrix.sh). Meant to be SOURCED, not executed.
#
# Validated on linux-x86_64; linux-aarch64 is expected to work for the
# orchestration itself (no arch-specific code: pure bash + the tools it
# shells out to) but hasn't been validated yet, and separately depends
# on whichever peer tools (ffmpeg/tsp/gst-launch-1.0/vlc/cvlc) are
# available as aarch64 apt/deb packages on a given box — they're
# discovered at runtime via `have()` rather than assumed, so a missing
# one degrades to a SKIPPED cell rather than a hard failure, but that's
# a per-tool availability question this directory doesn't track. The
# shell itself is not made macOS-portable (bash arrays, `mapfile`-style
# idioms, and GNU `timeout --kill-after` are all fair game; see
# scripts/check/**'s opposite convention — this directory is
# intentionally NOT part of that rail sweep). No bare `sleep N` as a
# standalone statement inside a loop (the sandbox blocks that shape);
# the short fixed settle sleeps below are one-shot, not loop bodies.
#
# NOT sourced with `set -e` baked in on purpose: the whole point of this
# file's callers is to keep going after one cell's peer command fails.
# Every external command a caller cares about the exit code of must use
# the `cmd || rc=$?` form (never a bare `cmd` on its own line followed by
# reading `$?`) so a failure doesn't blow up the calling script instead.
set -uo pipefail

# ---------------------------------------------------------------------
# Tool discovery
# ---------------------------------------------------------------------

have() { command -v "$1" >/dev/null 2>&1; }

# First line of `$1 $2`'s output (every peer tool below prints its
# version banner on line 1, to stdout or stderr depending on the tool).
_tool_version_line() {
  "$1" "$2" 2>&1 | head -1
}

# JSON object of {tool: "version string"} for every peer tool this
# matrix knows about that's actually installed on this box. A missing
# tool is simply an absent key (not a null value) — keeps the --meta
# blob small and lets a reader tell "not installed" apart from "we
# failed to read its version" at a glance.
tool_versions_json() {
  local json='{}'
  local spec name flag v
  for spec in ffmpeg:-version tsp:--version gst-launch-1.0:--version vlc:--version mpv:--version; do
    name=${spec%%:*}
    flag=${spec##*:}
    if have "$name"; then
      v=$(_tool_version_line "$name" "$flag")
      json=$(jq --arg k "$name" --arg v "$v" '. + {($k): $v}' <<<"$json")
    fi
  done
  printf '%s' "$json"
}

# ---------------------------------------------------------------------
# Port allocation
# ---------------------------------------------------------------------

# Ask the OS for an unused loopback port via a throwaway UDP bind —
# mirrors crates/tst-interop/tests/loopback.rs's own `free_port()`
# (small TOCTOU race between this probe's close and the real bind that
# follows; same accepted trade-off documented there).
free_port() {
  python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

# ---------------------------------------------------------------------
# Cell-id <-> filesystem-safe name
# ---------------------------------------------------------------------

# Cell ids use `/` (e.g. "srt/us-to-ffmpeg"); filenames can't. One
# escape for every log/work/cell-json filename this orchestrator writes.
slug() {
  printf '%s' "${1//\//_}"
}

# ---------------------------------------------------------------------
# Cell selection (--cells GLOB)
# ---------------------------------------------------------------------

# True iff cell id "$1" matches the orchestrator's --cells glob
# (default "*", i.e. everything). A plain bash `case` glob — supports
# the same `*`/`?`/`[...]` shapes run-matrix.sh's own --help documents.
cell_selected() {
  case "$1" in
    $CELLS_GLOB) return 0 ;;
    *) return 1 ;;
  esac
}

# ---------------------------------------------------------------------
# Timeout budgets
# ---------------------------------------------------------------------

# Wall-clock multiplier + flat floor applied to a cell's --seconds
# duration to get the `timeout` budget for ONE side of a cell (our
# process or the peer's): generous enough to absorb connection setup
# (SRT/RIST handshake, DNS-free loopback TCP connect) without being so
# loose that a genuinely wedged peer stalls the whole matrix for
# minutes. See `serve_timeout` for the HLS/RTSP-serve variant, which
# additionally has to outlive the driver's own 10s post-push LINGER
# (crates/tst-interop/src/serve.rs's `LINGER` const).
CELL_TIMEOUT_MULTIPLIER=3
CELL_TIMEOUT_FLOOR=15
# Extra margin added on top of `cell_timeout` for the side of a cell
# that binds/serves (HLS/RTSP `send --url hls://...` / `rtsp://...`):
# covers the driver's own LINGER (10s) plus mux/HTTP-server setup slop.
SERVE_LINGER_MARGIN=20

# `$1` = --seconds value for this cell (a plain non-negative integer —
# run-matrix.sh's own --seconds parsing enforces that, see its --help).
# Two defensive details, both load-bearing even though today's one
# caller (run-matrix.sh's own --seconds flag) already validates its
# input before this ever runs:
# - The explicit digits-only guard turns a malformed/empty `$1` into a
#   clear message instead of a cryptic `$(( ))` syntax error, and stops
#   it here rather than propagating an empty/garbage timeout budget to
#   every `timeout` call this script makes.
# - The `10#` base-10 prefix on the arithmetic itself is REQUIRED even
#   after the digits-only guard passes: bash's arithmetic evaluator
#   treats a leading-zero literal as octal, and "08"/"09" aren't valid
#   octal digits — `$(( 08 * 3 + 15 ))` fails with "value too large for
#   base" despite `08` passing a plain `^[0-9]+$` check. `10#08` forces
#   base-10 interpretation regardless of leading zeros.
cell_timeout() {
  [[ "$1" =~ ^[0-9]+$ ]] || {
    echo "cell_timeout: expected a non-negative integer, got: ${1:-<empty>}" >&2
    return 1
  }
  echo $(( 10#$1 * CELL_TIMEOUT_MULTIPLIER + CELL_TIMEOUT_FLOOR ))
}

serve_timeout() {
  echo $(( $(cell_timeout "$1") + SERVE_LINGER_MARGIN ))
}

# Budget for operations that aren't tied to a cell's --seconds duration at
# all (the bootstrap `gen`/`verify` of the per-profile source file, and the
# final `report merge`/`report render`) but still must never hang the whole
# matrix unattended (CI/soak runs — Tasks 13/14 — depend on that). Every
# one of these operates on data this task's own findings show can arrive
# malformed from a third-party remuxer (`verify`) or, for `report
# merge`/`render`, on N small already-written JSON files — none of them
# have a legitimate reason to run long, so one generous flat floor covers
# all of them rather than scaling a multiplier that doesn't apply.
REPORT_TIMEOUT=120

# ---------------------------------------------------------------------
# Format axis: profile registry + per-tool analyzer/decode helpers
# ---------------------------------------------------------------------
#
# The 12 canonical profile names, in crates/tst-interop/src/profiles.rs's
# registry order — hardcoded here since this script has no Rust-
# introspection path (registry-shape drift is caught by that file's own
# `registry_has_twelve_unique_named_profiles` test, not by this list).
ALL_PROFILE_NAMES="baseline,klv-sync,misp,h265-klv,av1-klv-a,av1-klv-b,h266-klv,audio,two-program,pcr-tight,pcr-sparse,pts-rollover"

# Expected total `ffprobe -show_streams` stream count for `$1` (a profile
# name): one video + one KLV PID per program, +1 more if the profile
# carries an audio stream — mirrors profiles.rs's `programs`/`audio`
# fields (only `two-program` sets programs=2; only `audio` sets
# audio=true; every other profile is 1 program, no audio, hence the `*`
# fallback). Deliberately NOT a `codec_type` assertion: ffmpeg's mpegts
# demuxer classifies our AV1 carriage's PMT stream_type 0x06 as generic
# `"data"`, not `"video"` (verified: `av1-klv-a`/`av1-klv-b` both show
# `["data","data"]` via `ffprobe -show_streams -of json` even though PID
# *count* is correct) — a known, already-documented AV1-in-TS carriage
# gap (see README), not something this structural probe should paper
# over by asserting a codec_type it can't get right.
expected_stream_count() {
  case "$1" in
    two-program) echo 4 ;;
    audio) echo 3 ;;
    *) echo 2 ;;
  esac
}

# tsanalyze_ts_line_counters_zero <tsanalyze --normalized output>
#
# Extracts the `invalidsyncs`/`transporterrors`/`suspectignored` fields
# from the single `ts:`-prefixed summary line `tsanalyze --normalized`
# emits (verified stable across all 12 profiles' synthetic fixtures —
# see README) and echoes a human-readable verdict line: "0" (clean) or
# a nonzero-counters/unparseable message. Caller checks the echoed text
# for the literal string "0" alone vs anything else.
tsanalyze_ts_line_counters_zero() {
  local out="$1"
  local tsline invalidsyncs transporterrors suspectignored
  tsline=$(grep -m1 '^ts:' <<<"$out") || tsline=""
  invalidsyncs=$(grep -oE 'invalidsyncs=[0-9]+' <<<"$tsline" | cut -d= -f2) || invalidsyncs=""
  transporterrors=$(grep -oE 'transporterrors=[0-9]+' <<<"$tsline" | cut -d= -f2) || transporterrors=""
  suspectignored=$(grep -oE 'suspectignored=[0-9]+' <<<"$tsline" | cut -d= -f2) || suspectignored=""
  if [[ -z "$invalidsyncs" || -z "$transporterrors" || -z "$suspectignored" ]]; then
    echo "could not parse ts: line counters from --normalized output (got: ${tsline:-<no ts: line found>})"
    return
  fi
  if [[ "$invalidsyncs" -ne 0 || "$transporterrors" -ne 0 || "$suspectignored" -ne 0 ]]; then
    echo "nonzero counters: invalidsyncs=$invalidsyncs transporterrors=$transporterrors suspectignored=$suspectignored"
    return
  fi
  echo "0"
}

# tsp_analyze_counters_zero <tsp -P analyze combined output>
#
# Same idea for `tsp -P analyze`'s "TRANSPORT STREAM ANALYSIS REPORT"
# header block, which carries the same three global counters under
# different, human-formatted labels ("With invalid sync: .... N"):
# verified stable (same 3 fields, same block position) across all 12
# profiles. NOTE: `tsp -P analyze`'s report freely uses the bare word
# "error" as a static field LABEL ("With transport error: .... 0")
# regardless of the counter's value — the generic marker-grep this
# script uses elsewhere would false-positive on every clean run, which
# is why this cell extracts the actual numeric counters instead of
# grepping for error-shaped words (see run_analyze_tsp's caller).
tsp_analyze_counters_zero() {
  local out="$1"
  local invalid_sync transport_error suspect_ignored
  invalid_sync=$(grep -oE 'With invalid sync: *\.+ *[0-9]+' <<<"$out" | grep -oE '[0-9]+$') || invalid_sync=""
  transport_error=$(grep -oE 'With transport error: *\.+ *[0-9]+' <<<"$out" | grep -oE '[0-9]+$') || transport_error=""
  suspect_ignored=$(grep -oE 'Suspect and ignored: *\.+ *[0-9]+' <<<"$out" | grep -oE '[0-9]+$') || suspect_ignored=""
  if [[ -z "$invalid_sync" || -z "$transport_error" || -z "$suspect_ignored" ]]; then
    echo "could not parse tsp analyze counters (invalid_sync=${invalid_sync:-?} transport_error=${transport_error:-?} suspect_ignored=${suspect_ignored:-?})"
    return
  fi
  if [[ "$invalid_sync" -ne 0 || "$transport_error" -ne 0 || "$suspect_ignored" -ne 0 ]]; then
    echo "nonzero counters: invalid_sync=$invalid_sync transport_error=$transport_error suspect_ignored=$suspect_ignored"
    return
  fi
  echo "0"
}

# ---------------------------------------------------------------------
# decode/* cells: exclusion filters (verified, not guessed)
# ---------------------------------------------------------------------
#
# `decode/{ffplay,vlc,mpv,gst-play}/<profile>` cells are
# CONTAINER-ACCEPTANCE probes (controller ruling, task 12 dispatch): pass
# iff the player opens the file and no container/TS/PSI-level error
# appears — NOT a full-decode assertion. `crates/tst-interop/src/fixtures.rs`'s
# H.264/H.265/H.266/AV1/AAC generators only build real, decodable data on
# keyframes; every inter-frame AU is filler bytes wrapped in a bare
# NAL/OBU/frame header by design (see README's rtsp-serve/vlc-probe
# finding for the original instance of this). Every player this script
# drives therefore logs a stream of codec-payload decode complaints on
# EVERY profile it can open at all — verified line-for-line against real
# captured logs (all 12 profiles x ffplay/vlc/mpv on this box, then all
# 12 profiles x gst-play in the 2026-08-20 pre-evidence run that first
# enabled those cells) before being added here, not guessed:
#   - h264: "crop values invalid", "sps_id N out of range",
#     "non-existing PPS/SPS N referenced", "decode_slice_header error",
#     "no frame!", "Error decoding the extradata"
#   - h265/h266 (vvc): "PPS id N not available"/"PPS id out of range",
#     "vps_video_parameter_set_id out of range", "Failed to read unit N
#     (type N)", "Failed to parse picture unit", "Error parsing NAL
#     unit #N"
#   - aac: "Reserved bit set", "Number of bands (N) exceeds limit (N)",
#     "Scalefactor (N) out of range", "channel element N is not
#     allocated", "Error decoding audio."
#   - mpv-specific decode-loop phrasing (only surfaces once mpv's own
#     `--vo=null` genuinely selects+decodes a track, unlike its old
#     `--no-video` invocation — see MPV_NO_STREAMS_SELECTED below):
#     "Error while decoding frame!"
#   - gst-play-specific phrasings for the same filler-AU mechanism
#     (playbin instantiates the decoder/parser, the payload yields zero
#     frames, the element reports it at EOS): "No valid frames decoded
#     before end of stream" (gstvideodecoder via avdec_h264 — 8 of the
#     12 profiles) / "No valid frames found before end of stream"
#     (gstbaseparse via h265parse — h265-klv), each followed by an
#     "ERROR debug information: ..." companion line naming the emitting
#     base-class function (excluded by that function name, the stable
#     anchor across GStreamer builds' differing source paths). The audio
#     profile's AAC filler yields the same "No valid frames decoded
#     before end of stream" through gstaudiodecoder (via avdec_aac),
#     whose companion line names gst_audio_decoder_sink_eventfunc — a
#     different suffix from the video/parse base classes, so it slipped
#     past the anchor above and FAILed decode/gst-play/audio on 3 of 4
#     PR #185 runs (2026-09-05; identical harness, tool versions and
#     runner image on the one PASS — the EOS report is timing-dependent
#     against the headless pulsesink failure, the phrasing is not).
#     av1-klv-*
#     and h266-klv produce NO output at all under --quiet (GStreamer
#     1.24 wires no AV1-in-TS / VVC decode path in playbin, so no
#     decoder ever instantiates to complain) — nothing to exclude.
# None of these indicate a container/PSI-level problem; all are excluded
# from the marker-grep every decode cell applies. Every alternative below
# is anchored to the surrounding fixed text actually observed (not a
# bare, generic word like "is not allocated" or "Scalefactor" alone) so
# this can't swallow an unrelated real error that happens to share a
# word — re-verified against the same captured logs (12 profiles x
# ffplay/vlc/mpv) this was originally built from: identical PASS/FAIL
# outcome per cell before and after tightening.
DECODE_PAYLOAD_NOISE='crop values invalid|sps_id [0-9]+ out of range|non-existing (PPS|SPS) [0-9]+ referenced|decode_slice_header error|no frame!|vps_video_parameter_set_id out of range|Failed to read unit [0-9]+ \(type [0-9]+\)|Failed to parse picture unit|PPS id [0-9]+ not available|PPS id out of range|Error parsing NAL unit #[0-9]+|Error decoding the extradata|Reserved bit set|Number of bands \([0-9]+\) exceeds limit \([0-9]+\)|Scalefactor \([-0-9]+\) out of range|channel element [0-9.]+ is not allocated|Error decoding audio\.|Error while decoding frame!|No valid frames (decoded|found) before end of stream|debug information: .*gst_((video_decoder|base_parse)_sink_event_default|audio_decoder_sink_eventfunc)'

# gst-play-specific: headless-sandbox audio-sink setup noise, observed
# only on the `audio` profile (the sole fixture with an audio track, so
# the only cell where playbin builds an audio sink): ALSA userspace
# config probing fails without a sound card ("ALSA lib conf.c:...:
# ... returned error" and friends), OpenAL's PipeWire backend can't
# create an event context, and pulsesink's connect is refused — none of
# it says anything about the INPUT FILE. Anchored on the ALSA-lib
# source-file prefix / the exact observed sink lines so a real
# container error can't be swallowed. Decode cells are local-file
# probes with no transport, so "Connection refused" here can only be
# the audio server. Verified against the same 2026-08-20 12-profile
# gst-play capture as the DECODE_PAYLOAD_NOISE additions above.
GST_PLAY_ENV_NOISE='^ALSA lib (conf|confmisc|pcm)\.c:[0-9]+:|Failed to create PipeWire event context|Failed to connect: Connection refused'

# ffplay-specific: `-nodisp` in this headless sandbox (no $DISPLAY, no
# real video output device) makes ffplay print "Failed to open file '...'
# or configure filtergraph" at exit REGARDLESS of whether the input was
# ever a valid, fully-decodable stream — verified with a control file
# (plain `ffmpeg -f lavfi testsrc ... -c:v libx264` H.264 file, zero
# relation to this crate's code): ffplay prints the exact same line on
# it. SDL/headless-display noise, not a signal about the input file.
FFPLAY_ENV_NOISE="Failed to open file .* or configure filtergraph"

# vlc-specific: reuses run_serve_peer_probe's already-established,
# already-verified sandbox-startup exclusion list (PulseAudio/D-Bus/
# $DISPLAY-less headless noise — see that function's doc comment).
VLC_ENV_NOISE='vlcpulse audio output error|dbus interface error|globalhotkeys|no suitable interface module|main libvlc error: interface'

# mpv-specific, NOT a text exclusion: mpv's invocation is
# `--vo=null --frames=10 --ao=null <file>` — deliberately `--vo=null`
# (discard rendered video frames), NOT `--no-video` (an earlier version
# of this cell used `--no-video`, which disables video TRACK SELECTION
# outright, not just display — on every profile but `audio`, this
# crate's synthetic fixtures carry no audio track either, so mpv ended
# up with literally nothing to select and validated NOTHING about the
# file at all; switched to `--vo=null` specifically to make this a
# genuine container-acceptance probe). With `--vo=null`, mpv genuinely
# selects and attempts to decode the video track — this codebase's
# H.264/H.265 filler content decodes with the same "Error while
# decoding frame!" noise `DECODE_PAYLOAD_NOISE` already excludes, so
# `baseline`-shaped profiles now PASS this cell. Two profile classes
# still hit "No video or audio streams selected." even with genuine
# track selection enabled, for their own real, verified reasons: `av1`
# profiles (this codebase's AV1 carriage, PMT stream_type 0x06, is
# never classified as a video track by mpv's ffmpeg-based demuxer, same
# root cause as the AV1-in-TS gap documented elsewhere) and `h266-klv`
# (mpv identifies the VVC video track but "Failed to initialize a
# decoder for codec 'vvc'" — this box's mpv build has no working VVC
# decoder at all, a distinct finding from ffmpeg's own vps_id=0
# rejection — both are left as unexcluded, real marker matches). Confirmed
# on a plain ffmpeg-authored control file too that "No video or audio
# streams selected." is deterministic given these conditions, not
# input-specific. That phrase does NOT itself match the generic
# error-marker grep (case-insensitively, `\berror\b` doesn't match the
# plural "Errors" in mpv's own trailing "Exiting... (Errors when loading
# file)" line) — so unlike ffplay/vlc, mpv needs an EXPLICIT substring
# check in addition to the marker-grep, or a genuine no-track-selected
# failure would silently read as a clean PASS. See run_decode_probe's
# caller.
MPV_NO_STREAMS_SELECTED="No video or audio streams selected"

# ---------------------------------------------------------------------
# Cell JSON emission (RawCell — see crates/tst-interop/src/report.rs)
# ---------------------------------------------------------------------
#
# Callers set these globals before sourcing this file's emit_* helpers:
#   CELLS_DIR  - directory RawCell JSON files are written into
#   OUTDIR     - the run's top-level --outdir (logs are recorded
#                relative to this, for a portable results.json)
#   PROFILE    - the profile name every cell in the current loop
#                iteration is testing (see run-matrix.sh's per-profile
#                loop)

# emit_cell <id> <peer> <direction> <tier> <verdict> <logfile> \
#           <metrics_json_file|-> [failure_text...]
#
# The one place that actually writes a `$CELLS_DIR/<slug>.json` file.
# `metrics_json_file` is a path to a file holding one JSON CellMetrics
# object (e.g. what `tst-interop send/verify --json FILE` wrote), or the
# literal `-` for `metrics: null` (SKIPPED_TOOL_MISSING, or a FAIL where
# neither side ever produced a parseable capture). Every remaining
# argument is one human-readable failure string.
emit_cell() {
  local id=$1 peer=$2 direction=$3 tier=$4 verdict=$5 logfile=$6 metrics_file=$7
  shift 7
  local failures_json='[]'
  if [[ $# -gt 0 ]]; then
    failures_json=$(printf '%s\n' "$@" | jq -R . | jq -s .)
  fi
  local metrics_json='null'
  if [[ "$metrics_file" != "-" && -s "$metrics_file" ]]; then
    metrics_json=$(cat "$metrics_file")
  fi
  local log_rel=$logfile
  if [[ "$logfile" == "$OUTDIR"/* ]]; then
    log_rel=${logfile#"$OUTDIR"/}
  fi
  local out_file="$CELLS_DIR/$(slug "$id").json"
  jq -n \
    --arg id "$id" --arg profile "$PROFILE" --arg peer "$peer" \
    --arg direction "$direction" --arg tier "$tier" --arg verdict "$verdict" \
    --arg log "$log_rel" --argjson failures "$failures_json" --argjson metrics "$metrics_json" \
    '{id: $id, profile: $profile, peer: $peer, direction: $direction, tier: $tier,
      verdict: $verdict, failures: $failures, metrics: $metrics, log: $log}' \
    >"$out_file"
}

emit_pass() {
  local id=$1 peer=$2 direction=$3 tier=$4 logfile=$5 metrics_file=$6
  emit_cell "$id" "$peer" "$direction" "$tier" PASS "$logfile" "$metrics_file"
}

emit_fail() {
  local id=$1 peer=$2 direction=$3 tier=$4 logfile=$5 metrics_file=$6
  shift 6
  emit_cell "$id" "$peer" "$direction" "$tier" FAIL "$logfile" "$metrics_file" "$@"
}

# metrics_only <verify_report_file> <out_file>
#
# `tst-interop recv|verify --json` both write a VerifyReport
# (`{pass, failures, metrics: {...}}`) — one level of nesting deeper
# than `send --json`'s bare CellMetrics, and deeper than what
# `emit_cell`'s `metrics_file` argument expects (a bare CellMetrics
# object, matching RawCell.metrics's shape in report.rs). Callers that
# only have a VerifyReport file (every `recv`/`verify` call site) must
# route it through this first — passing a VerifyReport straight to
# emit_cell embeds the wrong shape and `report merge` fails to parse
# the cell (missing `video_aus` etc., since it lands one level too
# high).
# Never fails (always returns 0), even if `$1` is missing/truncated/
# malformed JSON — every one of its 4 call sites in run-matrix.sh is a
# bare statement (`metrics_only "$vjson" "$mjson"`), and this function's
# own exit status IS that bare statement's exit status under `set -e`;
# a real-world truncated VerifyReport (e.g. a peer or `verify` process
# killed mid-write by a `timeout --kill-after`) would otherwise abort the
# whole matrix run right here. On failure `$2` ends up empty (the `>`
# redirection still creates/truncates it before jq runs and fails) —
# `emit_cell`'s own `-s "$metrics_file"` check already treats an empty
# file the same as "no metrics" (`metrics: null`), so no caller-side
# fallback is needed beyond never dying.
metrics_only() {
  jq '.metrics' "$1" >"$2" 2>/dev/null || true
}

emit_skipped() {
  local id=$1 peer=$2 direction=$3 tier=$4 reason=$5
  local logfile="$LOGS_DIR/$(slug "$id").log"
  printf 'SKIPPED_TOOL_MISSING: %s\n' "$reason" >"$logfile"
  emit_cell "$id" "$peer" "$direction" "$tier" SKIPPED_TOOL_MISSING "$logfile" - "$reason"
}

# declare_cell <id> — inventory pass (run-matrix.sh sets DECLARE_ONLY=1
# and calls every cell shape once per profile BEFORE executing anything;
# each shape records the id it WOULD run and returns). `report merge
# --inventory` later compares the produced cells against this exact
# multiset (release-gate audit RLS-B08).
declare_cell() {
  printf '%s\t%s\n' "$1" "$PROFILE" >>"$INVENTORY_TSV"
}
