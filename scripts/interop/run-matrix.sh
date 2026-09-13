#!/usr/bin/env bash
# Interop matrix: exchange the `tst-interop` driver's synthetic
# MPEG-TS/KLV traffic with real third-party tools (ffmpeg, TSDuck's
# `tsp`, GStreamer, VLC, mpv) over live SRT/RIST/UDP/TCP/HLS/RTSP
# sessions AND local per-profile analyzer/decode probes on this box,
# and write one evidence JSON file per cell, across two axes:
#   - transport axis: 25 cells, pinned to the "baseline" profile
#     (srt/udp/rist/tcp/hls/rtsp — task 11's inventory).
#   - format axis: analyze/{ffprobe,tsanalyze,tsp-analyze}/<profile>,
#     decode/{ffplay,vlc,mpv,gst-play}/<profile>, and
#     srt-live/{us-to-ffmpeg,ffmpeg-to-us,us-to-tsp,tsp-to-us}/<profile>,
#     once per --profiles entry (task 12's inventory).
#
# Validated on linux-x86_64; linux-aarch64 is expected to work for the
# orchestration itself but hasn't been validated yet, and separately
# depends on per-arch apt/deb availability of the peer tools below (see
# lib.sh's header for the full platform-support statement and the
# shell-portability stance this whole directory takes). Requires `jq`
# and `python3`
# (port allocation) on PATH; peer tools are individually optional —
# `have()` gates every cell that needs one, emitting
# SKIPPED_TOOL_MISSING instead of a fake pass/fail when it's absent.
#
# Usage:
#   run-matrix.sh --outdir DIR [--seconds N] [--cells GLOB] [--profiles LIST]
#
#   --outdir DIR      required. Cell JSON -> DIR/cells/*.json, per-cell
#                      combined (us + peer) logs -> DIR/logs/*.log,
#                      merged report -> DIR/results.json + results.md.
#   --seconds N        stream duration per cell, a positive integer
#                      (default 10). Every cell's process-kill budget
#                      scales off this (see lib.sh's cell_timeout).
#   --cells GLOB       bash-glob filter over cell ids (default "*", i.e.
#                      every cell). Cell ids look like
#                      "<transport>/<direction>-<peer>[-encrypted]" (25
#                      transport-axis cells, e.g. "srt/us-to-ffmpeg") or
#                      "<axis>/<peer>/<profile>" (format-axis cells, e.g.
#                      "decode/mpv/h266-klv") — pass 'srt/*' to run just
#                      the SRT block, 'decode/*' for every decode cell
#                      across every profile, etc. Quote it so the shell
#                      doesn't expand the glob itself.
#   --profiles LIST    comma-separated profile names (default: all 12
#                      canonical profiles, lib.sh's $ALL_PROFILE_NAMES).
#                      Only the format axis scales with this list — the
#                      transport axis always runs against "baseline"
#                      regardless of what's listed (see the per-profile
#                      loop below for why: task 11's 25-cell inventory
#                      was designed and evidenced against "baseline"
#                      only; scaling it by profile too would multiply
#                      that count for no new signal the format axis
#                      doesn't already cover more precisely). Pass a
#                      short list (e.g. "baseline,h266-klv") for a
#                      faster local iteration loop.
#
# Before running anything, a declare pass calls every cell shape once
# per --profiles entry with DECLARE_ONLY=1 (each shape records the id
# it WOULD run and returns immediately) and writes the resulting exact
# {id, profile} multiset to DIR/inventory.json, whose `shape` field is
# "full-157" iff --cells is the default "*" AND --profiles is the
# default full 12-profile list, else "subset". `tst-interop report
# merge` is handed this file via --inventory and hard-fails (exit 2, no
# results.json written) if the cells actually produced don't exactly
# match the declared multiset (missing/duplicate/extra/undeclared skip).
#
# Exit code: `tst-interop report merge`'s (0 iff every FAIL matched an
# expectations.toml entry — see that file's header for the grammar).
#
# Cell naming / tier semantics: see README.md in this directory.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

OUTDIR=""
SECONDS_ARG=10
CELLS_GLOB="*"
PROFILES_ARG="$ALL_PROFILE_NAMES"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --outdir)
      OUTDIR=$2
      shift 2
      ;;
    --seconds)
      SECONDS_ARG=$2
      shift 2
      ;;
    --cells)
      CELLS_GLOB=$2
      shift 2
      ;;
    --profiles)
      PROFILES_ARG=$2
      shift 2
      ;;
    -h | --help)
      sed -n '2,68p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "run-matrix.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

[[ -n "$OUTDIR" ]] || {
  echo "run-matrix.sh: --outdir is required" >&2
  exit 2
}
[[ "$SECONDS_ARG" =~ ^[0-9]+$ && "$SECONDS_ARG" -gt 0 ]] || {
  echo "run-matrix.sh: --seconds must be a positive integer, got: $SECONDS_ARG" >&2
  exit 2
}

# Hard dependencies, distinct from the optional per-cell peer tools
# `have()`/emit_skipped gate below: this script cannot run AT ALL
# without these (jq builds meta.json + every cell JSON; python3 backs
# free_port(); timeout wraps every process this script spawns). Fail
# loudly here with an actionable message rather than letting a missing
# one surface many minutes in as an obscure "command not found" from
# deep inside a cell shape — matters for the cloud-VM portability story
# (a fresh box may be missing one of these), not just this dev box/CI.
for dep in jq python3 timeout; do
  have "$dep" || {
    echo "run-matrix.sh: required tool '$dep' not found on PATH — install it before running this script (see README.md's header for the full prerequisite list)" >&2
    exit 2
  }
done

mkdir -p "$OUTDIR/cells" "$OUTDIR/logs" "$OUTDIR/work"
CELLS_DIR="$OUTDIR/cells"
LOGS_DIR="$OUTDIR/logs"
WORK="$OUTDIR/work"

# Settle time between binding/starting the listening side of a cell and
# starting its peer — every scheme here binds/listens near-instantly
# once its process starts (confirmed for SRT/RIST/TCP/UDP/HLS/RTSP
# while developing this script), so a short fixed sleep is enough; no
# scheme needs the port to be independently probed as "ready".
SETTLE=2

# AES passphrase shared by every "encrypted" cell below. Length must
# clear SRT's 10-byte floor (RFC/libsrt: 10..79 chars); arbitrary
# otherwise — this is test-fixture traffic, not a real secret.
ENCRYPTION_PASSPHRASE="tst-interop-matrix-fixture-secret"

echo "run-matrix: building tst-interop (release)..." >&2
# Both *_FORCE_VENDORED vars matter, not just SRT's: rist-sys's build.rs
# registers `rerun-if-env-changed=RIST_FORCE_VENDORED`, so leaving it
# unset here (a) makes cargo re-run rist-sys's build script on every
# invocation of this file (its cached-build fingerprint never matches
# across runs that do vs. don't set it) and (b) on some future host
# where librist happens to be pkg-config-discoverable, would silently
# link the system copy instead of the vendored one this matrix's
# evidence is meant to be gathered against.
(cd "$REPO_ROOT" && SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop)
BIN="$REPO_ROOT/target/release/tst-interop"

jq -n \
  --arg host "$(hostname)" \
  --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg seconds "$SECONDS_ARG" \
  --arg cells_glob "$CELLS_GLOB" \
  --argjson tools "$(tool_versions_json)" \
  '{host: $host, date: $date, seconds_per_cell: ($seconds | tonumber),
    cells_glob: $cells_glob, tools: $tools}' \
  >"$OUTDIR/meta.json"

# ---------------------------------------------------------------------
# Cell shapes
# ---------------------------------------------------------------------
# Every shape below writes ONE `$CELLS_DIR/<slug>.json` (via lib.sh's
# emit_* helpers) and ONE `$LOGS_DIR/<slug>.log` combining both sides'
# output. `direction` in the emitted cell always names OUR role
# (`send` = we push/serve, `recv` = we pull/listen) — matches
# crates/tst-interop/src/report.rs's own test fixtures.

# run_send_peer_recv <id> <peer> <tier> <our_url> <out_file> -- <peer_cmd...>
#
# We push (caller/connect) over <our_url>; the peer listens/receives
# and writes <out_file>. Verdict: our send must exit 0, <out_file> must
# be nonempty, and `verify --file <out_file>` must pass; a "transparent"
# tier additionally requires the sent-side stream_sha256 to equal
# <out_file>'s.
run_send_peer_recv() {
  local id=$1 peer=$2 tier=$3 our_url=$4 out_file=$5
  shift 5
  [[ "${1:-}" == "--" ]] && shift
  local -a peer_cmd=("$@")

  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  if ! have "$peer"; then
    emit_skipped "$id" "$peer" send "$tier" "$peer not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")

  {
    echo "=== cell: $id (tier=$tier, profile=$PROFILE) ==="
    echo "--- peer ($peer), listening/receiving -> $out_file ---"
    printf '+ '
    printf '%q ' "${peer_cmd[@]}"
    echo
  } >>"$log"

  timeout --kill-after=5 "${budget}s" "${peer_cmd[@]}" >>"$log" 2>&1 &
  local peer_pid=$!
  sleep "$SETTLE"

  local send_json="$WORK/$(slug "$id")-send.json"
  echo "--- us: send ($our_url) ---" >>"$log"
  local send_rc=0
  timeout --kill-after=3 "${budget}s" \
    "$BIN" send --profile "$PROFILE" --url "$our_url" --seconds "$SECONDS_ARG" --json "$send_json" \
    >>"$log" 2>&1 || send_rc=$?

  local peer_rc=0
  wait "$peer_pid" || peer_rc=$?
  echo "--- exit codes: send=$send_rc peer=$peer_rc ---" >>"$log"

  if [[ $send_rc -ne 0 ]]; then
    emit_fail "$id" "$peer" send "$tier" "$log" - "our send failed (exit $send_rc) — see log"
    return 0
  fi
  if [[ ! -s "$out_file" ]]; then
    emit_fail "$id" "$peer" send "$tier" "$log" - "peer produced no (or empty) output file (peer exit $peer_rc)"
    return 0
  fi

  local vjson="$WORK/$(slug "$id")-verify.json"
  local vrc=0
  timeout --kill-after=5 "${budget}s" \
    "$BIN" verify --file "$out_file" --expect "$PROFILE" --seconds "$SECONDS_ARG" --json "$vjson" \
    >>"$log" 2>&1 || vrc=$?

  # Every jq read below is guarded against a truncated/malformed
  # tool-produced JSON file — see run_peer_send_recv's matching comment
  # for why a bare `x=$(jq ...)` would otherwise abort the whole matrix.
  local -a reasons=()
  if [[ $vrc -ne 0 ]]; then
    local verify_failures
    verify_failures=$(jq -r '.failures | join("; ")' "$vjson" 2>/dev/null) || verify_failures="(unparseable failures list)"
    reasons+=("verify FAILed against the peer's captured file: $verify_failures")
  fi
  if [[ "$tier" == "transparent" ]]; then
    local sent_hash got_hash
    sent_hash=$(jq -r '.stream_sha256' "$send_json" 2>/dev/null) || sent_hash=""
    got_hash=$(jq -r '.metrics.stream_sha256' "$vjson" 2>/dev/null) || got_hash=""
    if [[ -z "$sent_hash" || -z "$got_hash" || "$sent_hash" != "$got_hash" ]]; then
      reasons+=("byte-transparent tier: stream_sha256 mismatch (sent ${sent_hash:-<unparseable>}, peer capture ${got_hash:-<unparseable>})")
    fi
  fi

  local mjson="$WORK/$(slug "$id")-metrics.json"
  metrics_only "$vjson" "$mjson"
  if [[ ${#reasons[@]} -eq 0 ]]; then
    emit_pass "$id" "$peer" send "$tier" "$log" "$mjson"
  else
    emit_fail "$id" "$peer" send "$tier" "$log" "$mjson" "${reasons[@]}"
  fi
}

# run_peer_send_recv <id> <peer> <tier> <our_url> -- <peer_cmd...>
#
# The peer pushes (from $GEN_FILE, already baked into peer_cmd by the
# caller); we listen/receive over <our_url>. Verdict: our recv must
# produce a JSON report and PASS its own profile-invariant check; a
# "transparent" tier additionally requires our received stream_sha256
# to equal $GEN_FILE's own (pre-computed in $GEN_STREAM_SHA).
run_peer_send_recv() {
  local id=$1 peer=$2 tier=$3 our_url=$4
  shift 4
  [[ "${1:-}" == "--" ]] && shift
  local -a peer_cmd=("$@")

  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  if ! have "$peer"; then
    emit_skipped "$id" "$peer" recv "$tier" "$peer not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")

  echo "=== cell: $id (tier=$tier, profile=$PROFILE) ===" >>"$log"
  echo "--- us: recv (listening on $our_url) ---" >>"$log"

  local recv_json="$WORK/$(slug "$id")-recv.json"
  timeout --kill-after=5 "${budget}s" \
    "$BIN" recv --url "$our_url" --expect "$PROFILE" --seconds "$SECONDS_ARG" --json "$recv_json" \
    >>"$log" 2>&1 &
  local recv_pid=$!
  sleep "$SETTLE"

  {
    echo "--- peer ($peer), sending from $GEN_FILE ---"
    printf '+ '
    printf '%q ' "${peer_cmd[@]}"
    echo
  } >>"$log"
  local peer_rc=0
  timeout --kill-after=5 "${budget}s" "${peer_cmd[@]}" >>"$log" 2>&1 || peer_rc=$?

  local recv_rc=0
  wait "$recv_pid" || recv_rc=$?
  echo "--- exit codes: peer=$peer_rc recv=$recv_rc ---" >>"$log"

  if [[ ! -s "$recv_json" ]]; then
    emit_fail "$id" "$peer" recv "$tier" "$log" - \
      "our recv produced no JSON output (recv exit $recv_rc, peer exit $peer_rc)"
    return 0
  fi

  # jq reads a file the peer/our own process can leave truncated (e.g. a
  # `timeout --kill-after` mid-write) — a bare `x=$(jq ...)` propagates
  # jq's failure as the assignment's own exit status and aborts the WHOLE
  # matrix under `set -e`. Every jq call below is guarded the same way:
  # `|| var=""`, then `-z` is treated as its own explicit failure reason
  # rather than silently falling through (an empty `pass` must never be
  # mistaken for `pass != "true"`'s ordinary FAIL path, and two empty
  # hashes must never compare as a false "match").
  local pass
  pass=$(jq -r '.pass' "$recv_json" 2>/dev/null) || pass=""
  local -a reasons=()
  if [[ -z "$pass" ]]; then
    reasons+=("our recv wrote unparseable JSON (possibly truncated by a kill-after) — see $recv_json")
  elif [[ "$pass" != "true" ]]; then
    local recv_failures
    recv_failures=$(jq -r '.failures | join("; ")' "$recv_json" 2>/dev/null) || recv_failures="(unparseable failures list)"
    reasons+=("recv FAILed: $recv_failures")
  fi
  if [[ "$tier" == "transparent" ]]; then
    local got_hash
    got_hash=$(jq -r '.metrics.stream_sha256' "$recv_json" 2>/dev/null) || got_hash=""
    if [[ -z "$got_hash" || "$got_hash" != "$GEN_STREAM_SHA" ]]; then
      reasons+=("byte-transparent tier: stream_sha256 mismatch (source $GEN_STREAM_SHA, received ${got_hash:-<unparseable>})")
    fi
  fi

  local mjson="$WORK/$(slug "$id")-metrics.json"
  metrics_only "$recv_json" "$mjson"
  if [[ ${#reasons[@]} -eq 0 ]]; then
    emit_pass "$id" "$peer" recv "$tier" "$log" "$mjson"
  else
    emit_fail "$id" "$peer" recv "$tier" "$log" "$mjson" "${reasons[@]}"
  fi
}

# run_serve_peer_pull <id> <peer> <tier> <our_serve_url> <out_file> -- <peer_cmd...>
#
# We BIND (HLS HTTP server / RTSP server) and serve $SECONDS_ARG of
# traffic, wall-clock paced, then keep serving for the driver's own
# LINGER (10s — crates/tst-interop/src/serve.rs); the peer pulls and
# writes <out_file>. Same file-based verdict shape as
# run_send_peer_recv, just no sent-side CellMetrics (serve mode's
# `send` doesn't accept --json — see serve.rs's doc comment) and a
# longer timeout budget for our own side.
run_serve_peer_pull() {
  local id=$1 peer=$2 tier=$3 our_serve_url=$4 out_file=$5
  shift 5
  [[ "${1:-}" == "--" ]] && shift
  local -a peer_cmd=("$@")

  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  if ! have "$peer"; then
    emit_skipped "$id" "$peer" send "$tier" "$peer not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local sbudget pbudget
  sbudget=$(serve_timeout "$SECONDS_ARG")
  pbudget=$(cell_timeout "$SECONDS_ARG")

  echo "=== cell: $id (tier=$tier, profile=$PROFILE) ===" >>"$log"
  echo "--- us: serve ($our_serve_url) ---" >>"$log"

  timeout --kill-after=5 "${sbudget}s" \
    "$BIN" send --profile "$PROFILE" --url "$our_serve_url" --seconds "$SECONDS_ARG" \
    >>"$log" 2>&1 &
  local serve_pid=$!
  sleep 2 # bind + mux/HTTP-server setup

  {
    echo "--- peer ($peer), pulling -> $out_file ---"
    printf '+ '
    printf '%q ' "${peer_cmd[@]}"
    echo
  } >>"$log"
  local peer_rc=0
  timeout --kill-after=5 "${pbudget}s" "${peer_cmd[@]}" >>"$log" 2>&1 || peer_rc=$?

  local serve_rc=0
  wait "$serve_pid" || serve_rc=$?
  echo "--- exit codes: peer=$peer_rc serve=$serve_rc ---" >>"$log"

  if [[ ! -s "$out_file" ]]; then
    emit_fail "$id" "$peer" send "$tier" "$log" - \
      "peer produced no (or empty) output file (peer exit $peer_rc, serve exit $serve_rc)"
    return 0
  fi

  local vjson="$WORK/$(slug "$id")-verify.json"
  local vrc=0
  timeout --kill-after=5 "${pbudget}s" \
    "$BIN" verify --file "$out_file" --expect "$PROFILE" --seconds "$SECONDS_ARG" --json "$vjson" \
    >>"$log" 2>&1 || vrc=$?

  local mjson="$WORK/$(slug "$id")-metrics.json"
  metrics_only "$vjson" "$mjson"
  if [[ $vrc -eq 0 ]]; then
    emit_pass "$id" "$peer" send "$tier" "$log" "$mjson"
  else
    # Guarded (see run_send_peer_recv's comment) — $vjson can be
    # truncated/malformed if `verify` itself was the process killed by
    # `timeout --kill-after` above.
    local verify_failures
    verify_failures=$(jq -r '.failures | join("; ")' "$vjson" 2>/dev/null) || verify_failures="(unparseable failures list)"
    emit_fail "$id" "$peer" send "$tier" "$log" "$mjson" \
      "verify FAILed against the peer's captured file: $verify_failures"
  fi
}

# run_serve_peer_probe <id> <peer> <our_serve_url> -- <peer_cmd...>
#
# Same serve setup as run_serve_peer_pull, but the peer is a
# decode-only probe (no capture file to verify) — mirrors this
# project's own pre-release decoder-compatibility spot check: PASS iff
# the peer's combined output has no error/fatal/cannot/failed marker.
# Tier is always "n/a" (there is nothing byte- or structure-level to
# compare — a clean decode is the only signal).
run_serve_peer_probe() {
  local id=$1 peer=$2 our_serve_url=$3
  shift 3
  [[ "${1:-}" == "--" ]] && shift
  local -a peer_cmd=("$@")

  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  if ! have "$peer"; then
    emit_skipped "$id" "$peer" send n/a "$peer not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local sbudget pbudget
  sbudget=$(serve_timeout "$SECONDS_ARG")
  pbudget=$(cell_timeout "$SECONDS_ARG")

  echo "=== cell: $id (decode-probe, no file-verify, profile=$PROFILE) ===" >>"$log"
  echo "--- us: serve ($our_serve_url) ---" >>"$log"

  timeout --kill-after=5 "${sbudget}s" \
    "$BIN" send --profile "$PROFILE" --url "$our_serve_url" --seconds "$SECONDS_ARG" \
    >>"$log" 2>&1 &
  local serve_pid=$!
  sleep 2

  local probe_log="$WORK/$(slug "$id")-probe.log"
  {
    echo "--- peer ($peer), decode-probe ---"
    printf '+ '
    printf '%q ' "${peer_cmd[@]}"
    echo
  } >>"$log"
  local peer_rc=0
  timeout --kill-after=5 "${pbudget}s" "${peer_cmd[@]}" >"$probe_log" 2>&1 || peer_rc=$?
  cat "$probe_log" >>"$log"

  local serve_rc=0
  wait "$serve_pid" || serve_rc=$?
  echo "--- exit codes: probe=$peer_rc serve=$serve_rc ---" >>"$log"

  # Same permissive, case-insensitive, whole-word marker grep this
  # project's own decoder-compatibility checks use — but first
  # strip VLC's known-harmless sandbox-startup noise (no PulseAudio/
  # D-Bus session/$DISPLAY in this headless box: VLC's default
  # `--extraintf` list tries `dbus`+`globalhotkeys` regardless of
  # `--intf dummy`, logs an "error"-labeled line when each fails to
  # init, then falls back to the dummy interface successfully — none of
  # that reflects on whether the RTSP stream itself decoded cleanly).
  # A real stream/decode problem (e.g. VLC's own "buffer deadlock
  # prevented") is untouched by this filter.
  local filtered
  filtered=$(grep -Ev "$VLC_ENV_NOISE" "$probe_log" || true)
  if grep -Eqi '\b(error|fatal|cannot|failed)\b' <<<"$filtered"; then
    local sample
    sample=$(grep -Ei '\b(error|fatal|cannot|failed)\b' <<<"$filtered" | head -3 | tr '\n' ';')
    emit_fail "$id" "$peer" send n/a "$log" - "decoder reported error/fatal markers: $sample"
  else
    emit_pass "$id" "$peer" send n/a "$log" -
  fi
}

# ---------------------------------------------------------------------
# Format-axis cell shapes (local analyzer/decode probes, no transport)
# ---------------------------------------------------------------------
# All three run against $GEN_FILE (the already-generated per-profile
# source, set by the per-profile loop at the bottom of this script) —
# there is no us-side send/recv/serve role at all, so `direction` and
# `tier` are both "n/a" on every cell these shapes emit (mirrors
# rtsp-serve/vlc-probe's existing decode-only convention).

# run_analyze_ffprobe <profile>
#
# Structural probe: `ffprobe -show_streams -of json` on $GEN_FILE,
# asserting the expected total stream count (see lib.sh's
# expected_stream_count doc comment for the formula and its one known,
# deliberate gap: AV1 profiles pass this count-only check even though
# ffmpeg misclassifies the AV1 PID as `codec_type: data`).
run_analyze_ffprobe() {
  local profile=$1
  local id="analyze/ffprobe/$profile"
  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  echo "run-matrix: cell $id" >&2
  if ! have ffprobe; then
    emit_skipped "$id" ffprobe n/a n/a "ffprobe not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")
  local json="$WORK/$(slug "$id").json"
  local rc=0
  timeout --kill-after=5 "${budget}s" ffprobe -v quiet -show_streams -of json "$GEN_FILE" \
    >"$json" 2>"$log" || rc=$?

  local want got
  want=$(expected_stream_count "$profile")
  got=$(jq '.streams | length' "$json" 2>/dev/null) || got=""
  {
    echo "=== cell: $id (analyze-probe, no transport, profile=$profile) ==="
    echo "ffprobe exit=$rc want_streams=$want got_streams=${got:-<unparseable>}"
  } >>"$log"

  if [[ $rc -ne 0 ]]; then
    emit_fail "$id" ffprobe n/a n/a "$log" - "ffprobe exited $rc — see log"
  elif [[ -z "$got" ]]; then
    emit_fail "$id" ffprobe n/a n/a "$log" - "ffprobe produced unparseable JSON — see $json"
  elif [[ "$got" != "$want" ]]; then
    emit_fail "$id" ffprobe n/a n/a "$log" - "stream count mismatch: got $got, want $want"
  else
    emit_pass "$id" ffprobe n/a n/a "$log" -
  fi
}

# run_analyze_counter_probe <tool> <id-suffix> <verdict-fn> -- <cmd...>
#
# Shared body for the counter-based analyze/* cells (tsanalyze --normalized,
# tsp -P analyze): run <cmd...> under the per-cell timeout, capture combined
# stdout+stderr, then call <verdict-fn> with the captured text. <verdict-fn>
# must echo "0" for a clean run or a human-readable non-"0" reason otherwise
# — see lib.sh's tsanalyze_ts_line_counters_zero / tsp_analyze_counters_zero
# doc comments for the two verdict fns this currently drives (the latter
# explains why these cells can't reuse the generic error-marker grep: the
# report's own field LABELS contain the bare word "error").
run_analyze_counter_probe() {
  local tool=$1 id_suffix=$2 verdict_fn=$3
  shift 3
  [[ "${1:-}" == "--" ]] && shift
  local -a cmd=("$@")
  local id="analyze/$id_suffix"
  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  echo "run-matrix: cell $id" >&2
  if ! have "$tool"; then
    emit_skipped "$id" "$tool" n/a n/a "$tool not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")
  local rc=0 out
  out=$(timeout --kill-after=5 "${budget}s" "${cmd[@]}" 2>&1) || rc=$?
  {
    echo "=== cell: $id (analyze-probe, no transport) ==="
    echo "$out"
  } >>"$log"

  local verdict
  verdict=$("$verdict_fn" "$out") || verdict="$verdict_fn itself failed — see log"
  if [[ $rc -ne 0 ]]; then
    emit_fail "$id" "$tool" n/a n/a "$log" - "$tool exited $rc — see log"
  elif [[ "$verdict" == "0" ]]; then
    emit_pass "$id" "$tool" n/a n/a "$log" -
  else
    emit_fail "$id" "$tool" n/a n/a "$log" - "$verdict"
  fi
}

# run_decode_probe <player> <profile> -- <player_cmd...>
#
# Local, no-transport container-acceptance probe (see lib.sh's
# DECODE_PAYLOAD_NOISE doc comment for the pass criterion and the
# verified, per-player exclusion filters). Exit code of <player_cmd> is
# IGNORED on purpose — mirrors this project's own pre-release
# decoder-compatibility check's `|| true` convention verbatim; decode
# acceptance here is signaled purely through log content, exactly as
# established there.
run_decode_probe() {
  local player=$1 profile=$2
  shift 2
  [[ "${1:-}" == "--" ]] && shift
  local -a player_cmd=("$@")
  local id="decode/$player/$profile"

  cell_selected "$id" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "$id"; return 0; fi
  echo "run-matrix: cell $id" >&2
  # gst-play's real binary is gst-play-1.0 (matches the invocation this
  # project's own pre-release decoder-compatibility check uses); the
  # cell id keeps the short "gst-play" form per the task's Interfaces
  # line, mirroring how "srt/us-to-gst" already abbreviates its
  # gst-launch-1.0 peer.
  local bin_name=$player
  [[ "$player" == "gst-play" ]] && bin_name="gst-play-1.0"
  if ! have "$bin_name"; then
    emit_skipped "$id" "$bin_name" n/a n/a "$bin_name not installed on this box"
    return 0
  fi

  local log="$LOGS_DIR/$(slug "$id").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")
  {
    echo "=== cell: $id (decode-probe, no transport, profile=$profile) ==="
    echo "--- pre-release decoder-compatibility invocation, lifted verbatim (exit code ignored, see this function's doc comment) ---"
    printf '+ '
    printf '%q ' "${player_cmd[@]}"
    echo
  } >>"$log"
  # Player output goes to its OWN file, separate from $log's header/
  # command-echo lines above — some invocations' own ARGUMENTS contain
  # marker words (e.g. ffplay's "-loglevel error"), so grepping the
  # combined $log (command line included) would false-positive on our
  # own echoed command rather than the player's actual output. $probe_log
  # is appended into $log afterward purely for human-readable context
  # (mirrors run_serve_peer_probe's identical split).
  local probe_log="$WORK/$(slug "$id")-probe.log"
  timeout --kill-after=5 "${budget}s" "${player_cmd[@]}" >"$probe_log" 2>&1 || true
  cat "$probe_log" >>"$log"

  # Every grep -v below is guarded with `|| true` — a fully-clean probe
  # log legitimately filters down to ZERO remaining lines, which is
  # `grep -v`'s normal "no lines passed the filter" exit status 1, not
  # an error; a bare (unguarded) assignment here would abort the whole
  # matrix under `set -e` on the very first fully-clean cell (see
  # run_send_peer_recv's comment for the general `set -e` +
  # `local x; x=$(...)` gotcha this mirrors).
  local filtered
  filtered=$(grep -Ev "$DECODE_PAYLOAD_NOISE" "$probe_log" || true)
  case "$player" in
    ffplay) filtered=$(grep -Ev "$FFPLAY_ENV_NOISE" <<<"$filtered" || true) ;;
    vlc) filtered=$(grep -Ev "$VLC_ENV_NOISE" <<<"$filtered" || true) ;;
    gst-play) filtered=$(grep -Ev "$GST_PLAY_ENV_NOISE" <<<"$filtered" || true) ;;
  esac

  local -a reasons=()
  if grep -Eqi '\b(error|fatal|cannot|failed)\b' <<<"$filtered"; then
    local sample
    sample=$(grep -Ei '\b(error|fatal|cannot|failed)\b' <<<"$filtered" | head -3 | tr '\n' ';')
    reasons+=("decoder reported error/fatal markers: $sample")
  fi
  if [[ "$player" == "mpv" ]] && grep -q "$MPV_NO_STREAMS_SELECTED" "$probe_log"; then
    reasons+=("mpv selected no stream at all (see lib.sh's MPV_NO_STREAMS_SELECTED doc comment for the verified --no-video/no-audio-track mechanism)")
  fi

  if [[ ${#reasons[@]} -eq 0 ]]; then
    emit_pass "$id" "$bin_name" n/a n/a "$log" -
  else
    emit_fail "$id" "$bin_name" n/a n/a "$log" - "${reasons[@]}"
  fi
}

# ---------------------------------------------------------------------
# Per-transport cell definitions
# ---------------------------------------------------------------------
# Every function below assumes $GEN_FILE / $GEN_STREAM_SHA are set
# (the per-profile loop at the bottom of this script does that before
# calling any of them) and reads $PROFILE / $SECONDS_ARG / $BIN.

srt_cells() {
  local port

  port=$(free_port)
  run_send_peer_recv "srt/us-to-ffmpeg" ffmpeg remux \
    "srt://127.0.0.1:$port?mode=caller" "$WORK/srt_us-to-ffmpeg.ts" -- \
    ffmpeg -y -loglevel warning -i "srt://127.0.0.1:$port?mode=listener" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/srt_us-to-ffmpeg.ts"

  port=$(free_port)
  run_peer_send_recv "srt/ffmpeg-to-us" ffmpeg remux \
    "srt://:$port?mode=listener" -- \
    ffmpeg -y -re -loglevel warning -i "$GEN_FILE" -c copy -copy_unknown -f mpegts \
    "srt://127.0.0.1:$port?mode=caller"

  port=$(free_port)
  run_send_peer_recv "srt/us-to-tsp" tsp transparent \
    "srt://127.0.0.1:$port?mode=caller" "$WORK/srt_us-to-tsp.ts" -- \
    tsp -I srt --listener ":$port" -O file "$WORK/srt_us-to-tsp.ts"

  port=$(free_port)
  run_peer_send_recv "srt/tsp-to-us" tsp transparent \
    "srt://:$port?mode=listener" -- \
    tsp -I file "$GEN_FILE" -P regulate -O srt --caller "127.0.0.1:$port" --linger 5

  port=$(free_port)
  run_send_peer_recv "srt/us-to-gst" gst-launch-1.0 transparent \
    "srt://127.0.0.1:$port?mode=caller" "$WORK/srt_us-to-gst.ts" -- \
    gst-launch-1.0 srtsrc "uri=srt://0.0.0.0:$port?mode=listener" ! \
    filesink "location=$WORK/srt_us-to-gst.ts"

  port=$(free_port)
  run_peer_send_recv "srt/gst-to-us" gst-launch-1.0 transparent \
    "srt://:$port?mode=listener" -- \
    gst-launch-1.0 filesrc "location=$GEN_FILE" ! \
    tsparse set-timestamps=true smoothing-latency=100000 ! \
    srtsink "uri=srt://127.0.0.1:$port?mode=caller"

  # Encrypted (ffmpeg + tsp pairs only, per the plan — gst's srtsrc/
  # srtsink encryption story wasn't part of this arc's scope).
  port=$(free_port)
  run_send_peer_recv "srt/us-to-ffmpeg-encrypted" ffmpeg remux \
    "srt://127.0.0.1:$port?mode=caller&passphrase=$ENCRYPTION_PASSPHRASE" \
    "$WORK/srt_us-to-ffmpeg-encrypted.ts" -- \
    ffmpeg -y -loglevel warning -passphrase "$ENCRYPTION_PASSPHRASE" \
    -i "srt://127.0.0.1:$port?mode=listener" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/srt_us-to-ffmpeg-encrypted.ts"

  # -passphrase is an SRT-protocol AVOption: it must sit between -i and
  # the OUTPUT url here (the encrypted side is the SRT *output*, not
  # the plain-file input) — ffmpeg applies an option to whichever -i/
  # output it immediately precedes, and rejects it as "not found" on
  # the wrong side (confirmed empirically while developing this script).
  port=$(free_port)
  run_peer_send_recv "srt/ffmpeg-to-us-encrypted" ffmpeg remux \
    "srt://:$port?mode=listener&passphrase=$ENCRYPTION_PASSPHRASE" -- \
    ffmpeg -y -re -loglevel warning \
    -i "$GEN_FILE" -c copy -copy_unknown -passphrase "$ENCRYPTION_PASSPHRASE" -f mpegts \
    "srt://127.0.0.1:$port?mode=caller"

  port=$(free_port)
  run_send_peer_recv "srt/us-to-tsp-encrypted" tsp transparent \
    "srt://127.0.0.1:$port?mode=caller&passphrase=$ENCRYPTION_PASSPHRASE" \
    "$WORK/srt_us-to-tsp-encrypted.ts" -- \
    tsp -I srt --listener ":$port" --passphrase "$ENCRYPTION_PASSPHRASE" \
    -O file "$WORK/srt_us-to-tsp-encrypted.ts"

  port=$(free_port)
  run_peer_send_recv "srt/tsp-to-us-encrypted" tsp transparent \
    "srt://:$port?mode=listener&passphrase=$ENCRYPTION_PASSPHRASE" -- \
    tsp -I file "$GEN_FILE" -P regulate -O srt --caller "127.0.0.1:$port" \
    --linger 5 --passphrase "$ENCRYPTION_PASSPHRASE"
}

udp_cells() {
  local port

  port=$(free_port)
  run_send_peer_recv "udp/us-to-ffmpeg" ffmpeg remux \
    "udp://127.0.0.1:$port" "$WORK/udp_us-to-ffmpeg.ts" -- \
    ffmpeg -y -loglevel warning -i "udp://127.0.0.1:$port" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/udp_us-to-ffmpeg.ts"

  port=$(free_port)
  run_peer_send_recv "udp/ffmpeg-to-us" ffmpeg remux \
    "udp://127.0.0.1:$port" -- \
    ffmpeg -y -re -loglevel warning -i "$GEN_FILE" -c copy -copy_unknown -f mpegts \
    "udp://127.0.0.1:$port"

  port=$(free_port)
  run_send_peer_recv "udp/us-to-tsp" tsp transparent \
    "udp://127.0.0.1:$port" "$WORK/udp_us-to-tsp.ts" -- \
    tsp -I ip "$port" -O file "$WORK/udp_us-to-tsp.ts"

  port=$(free_port)
  run_peer_send_recv "udp/tsp-to-us" tsp transparent \
    "udp://127.0.0.1:$port" -- \
    tsp -I file "$GEN_FILE" -P regulate -O ip "127.0.0.1:$port"
}

rist_cells() {
  local port

  port=$(free_port)
  run_send_peer_recv "rist/us-to-ffmpeg" ffmpeg remux \
    "rist://127.0.0.1:$port" "$WORK/rist_us-to-ffmpeg.ts" -- \
    ffmpeg -y -loglevel warning -i "rist://@0.0.0.0:$port" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/rist_us-to-ffmpeg.ts"

  port=$(free_port)
  run_peer_send_recv "rist/ffmpeg-to-us" ffmpeg remux \
    "rist://@0.0.0.0:$port" -- \
    ffmpeg -y -re -loglevel warning -i "$GEN_FILE" -c copy -copy_unknown -f mpegts \
    "rist://127.0.0.1:$port"

  port=$(free_port)
  run_send_peer_recv "rist/us-to-tsp" tsp transparent \
    "rist://127.0.0.1:$port" "$WORK/rist_us-to-tsp.ts" -- \
    tsp -I rist "rist://@0.0.0.0:$port" -O file "$WORK/rist_us-to-tsp.ts"

  port=$(free_port)
  run_peer_send_recv "rist/tsp-to-us" tsp transparent \
    "rist://@0.0.0.0:$port" -- \
    tsp -I file "$GEN_FILE" -P regulate -O rist "rist://127.0.0.1:$port"
}

tcp_cells() {
  local port

  # ffmpeg listens (`?listen=1`); we connect as the caller.
  port=$(free_port)
  run_send_peer_recv "tcp/us-to-ffmpeg" ffmpeg remux \
    "tcp://127.0.0.1:$port" "$WORK/tcp_us-to-ffmpeg.ts" -- \
    ffmpeg -y -loglevel warning -i "tcp://0.0.0.0:$port?listen=1" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/tcp_us-to-ffmpeg.ts"

  # We listen; ffmpeg connects as a plain (non-listen) caller.
  port=$(free_port)
  run_peer_send_recv "tcp/ffmpeg-to-us" ffmpeg remux \
    "tcp://0.0.0.0:$port?listen=1" -- \
    ffmpeg -y -re -loglevel warning -i "$GEN_FILE" -c copy -copy_unknown -f mpegts \
    "tcp://127.0.0.1:$port"
}

hls_cells() {
  local port

  port=$(free_port)
  run_serve_peer_pull "hls/ffmpeg-pull" ffmpeg remux \
    "hls://127.0.0.1:$port" "$WORK/hls_ffmpeg-pull.ts" -- \
    ffmpeg -y -loglevel warning -i "http://127.0.0.1:$port/playlist.m3u8" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/hls_ffmpeg-pull.ts"

  port=$(free_port)
  run_serve_peer_pull "hls/tsp-pull" tsp remux \
    "hls://127.0.0.1:$port" "$WORK/hls_tsp-pull.ts" -- \
    tsp -I hls "http://127.0.0.1:$port/playlist.m3u8" -O file "$WORK/hls_tsp-pull.ts"
}

rtsp_cells() {
  local port

  port=$(free_port)
  run_serve_peer_pull "rtsp-serve/ffmpeg-pull" ffmpeg remux \
    "rtsp://127.0.0.1:$port/mount" "$WORK/rtsp-serve_ffmpeg-pull.ts" -- \
    ffmpeg -y -loglevel warning -rtsp_transport tcp \
    -i "rtsp://127.0.0.1:$port/mount" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/rtsp-serve_ffmpeg-pull.ts"

  port=$(free_port)
  run_serve_peer_probe "rtsp-serve/vlc-probe" cvlc \
    "rtsp://127.0.0.1:$port/mount" -- \
    cvlc --intf dummy --play-and-exit --no-audio \
    "--run-time=$SECONDS_ARG" "rtsp://127.0.0.1:$port/mount"

  # Best-effort: peer-to-peer only (vlc serves, ffmpeg consumes) — our
  # own `recv`/transport::make_recv has no rtsp:// connect-side support
  # (see transport.rs's scheme dispatch: srt/udp/tcp/rist only), so
  # there is no way for tst-interop itself to be the RTSP client here.
  # "us" only brackets this cell (gen the source file / verify the
  # capture); expect flakiness (VLC's --sout RTSP serving is fiddly) —
  # wired as `known_flaky` in expectations.toml starting Task 12.
  cell_selected "rtsp-consume/vlc-serve-ffmpeg-pull" || return 0
  if [[ -n "${DECLARE_ONLY:-}" ]]; then declare_cell "rtsp-consume/vlc-serve-ffmpeg-pull"; return 0; fi
  if ! have cvlc || ! have ffmpeg; then
    local missing="cvlc and/or ffmpeg"
    emit_skipped "rtsp-consume/vlc-serve-ffmpeg-pull" "vlc+ffmpeg" n/a remux \
      "$missing not installed on this box"
    return 0
  fi
  port=$(free_port)
  local log="$LOGS_DIR/$(slug "rtsp-consume/vlc-serve-ffmpeg-pull").log"
  : >"$log"
  local budget
  budget=$(cell_timeout "$SECONDS_ARG")
  {
    echo "=== cell: rtsp-consume/vlc-serve-ffmpeg-pull (tier=remux, profile=$PROFILE) ==="
    echo "--- peer: vlc serves \$GEN_FILE over RTSP ---"
  } >>"$log"
  timeout --kill-after=5 "${budget}s" \
    cvlc "$GEN_FILE" --intf dummy --sout "#rtp{sdp=rtsp://:$port/s}" \
    >>"$log" 2>&1 &
  local vlc_pid=$!
  sleep "$SETTLE"

  local out="$WORK/rtsp-consume_vlc-serve-ffmpeg-pull.ts"
  echo "--- peer: ffmpeg pulls rtsp://127.0.0.1:$port/s -> $out ---" >>"$log"
  local ff_rc=0
  timeout --kill-after=5 "${budget}s" \
    ffmpeg -y -loglevel warning -rtsp_transport tcp -i "rtsp://127.0.0.1:$port/s" \
    -map 0 -c copy -copy_unknown -f mpegts "$out" \
    >>"$log" 2>&1 || ff_rc=$?

  local vlc_rc=0
  wait "$vlc_pid" || vlc_rc=$?
  echo "--- exit codes: ffmpeg=$ff_rc vlc=$vlc_rc ---" >>"$log"

  if [[ ! -s "$out" ]]; then
    emit_fail "rtsp-consume/vlc-serve-ffmpeg-pull" "vlc+ffmpeg" n/a remux "$log" - \
      "ffmpeg produced no (or empty) capture (ffmpeg exit $ff_rc, vlc exit $vlc_rc)"
    return 0
  fi
  local vjson="$WORK/rtsp-consume_vlc-serve-ffmpeg-pull-verify.json"
  local vrc=0
  timeout --kill-after=5 "${budget}s" \
    "$BIN" verify --file "$out" --expect "$PROFILE" --seconds "$SECONDS_ARG" --json "$vjson" \
    >>"$log" 2>&1 || vrc=$?
  local mjson="$WORK/rtsp-consume_vlc-serve-ffmpeg-pull-metrics.json"
  metrics_only "$vjson" "$mjson"
  if [[ $vrc -eq 0 ]]; then
    emit_pass "rtsp-consume/vlc-serve-ffmpeg-pull" "vlc+ffmpeg" n/a remux "$log" "$mjson"
  else
    # Guarded (see run_send_peer_recv's comment) — $vjson can be
    # truncated/malformed if `verify` itself was the process killed by
    # `timeout --kill-after` above.
    local verify_failures
    verify_failures=$(jq -r '.failures | join("; ")' "$vjson" 2>/dev/null) || verify_failures="(unparseable failures list)"
    emit_fail "rtsp-consume/vlc-serve-ffmpeg-pull" "vlc+ffmpeg" n/a remux "$log" "$mjson" \
      "verify FAILed against ffmpeg's captured file: $verify_failures"
  fi
}

# ---------------------------------------------------------------------
# Format-axis per-profile groups
# ---------------------------------------------------------------------
# Unlike the six *_cells functions above (transport axis, intentionally
# pinned to the "baseline" profile only — see the per-profile loop
# below), these three run once per entry in --profiles.

# analyze_cells_for_profile <profile>
analyze_cells_for_profile() {
  local profile=$1
  run_analyze_ffprobe "$profile"
  run_analyze_counter_probe tsanalyze "tsanalyze/$profile" tsanalyze_ts_line_counters_zero -- \
    tsanalyze --normalized "$GEN_FILE"
  run_analyze_counter_probe tsp "tsp-analyze/$profile" tsp_analyze_counters_zero -- \
    tsp -I file "$GEN_FILE" -P analyze -O drop
}

# decode_cells_for_profile <profile>
#
# This project's own four canonical per-player headless decode
# invocations (from its pre-release decoder-compatibility check), onto
# this profile's $GEN_FILE — cited inline in each invocation's comment.
# ffplay/vlc/gst-play-1.0 are lifted verbatim; mpv's is deliberately
# NOT verbatim (`--vo=null`, not `--no-video` — see its own comment
# below for why) but keeps the same `|| true` exit-code-ignored
# convention.
decode_cells_for_profile() {
  local profile=$1

  # ffplay: -autoexit -nodisp -loglevel error -t 1 $BASELINE
  run_decode_probe ffplay "$profile" -- \
    ffplay -autoexit -nodisp -loglevel error -t 1 "$GEN_FILE"

  # vlc: -I dummy --play-and-exit --no-audio --run-time=1 $BASELINE
  run_decode_probe vlc "$profile" -- \
    vlc -I dummy --play-and-exit --no-audio --run-time=1 "$GEN_FILE"

  # mpv: --vo=null --frames=10 --ao=null $BASELINE — NOT --no-video (see
  # lib.sh's MPV_NO_STREAMS_SELECTED doc comment for why: --no-video
  # disables video TRACK SELECTION outright, not just display, so it
  # validated nothing about the video PID at all on any profile without
  # an audio track. --vo=null discards the rendered frames but leaves
  # track selection + decode genuinely exercised — a real
  # container-acceptance probe.
  run_decode_probe mpv "$profile" -- \
    mpv --vo=null --frames=10 --ao=null "$GEN_FILE"

  # gst-play-1.0: --no-interactive --quiet $BASELINE
  run_decode_probe gst-play "$profile" -- \
    gst-play-1.0 --no-interactive --quiet "$GEN_FILE"
}

# srt_live_cells_for_profile <profile>
#
# The subset of srt_cells() the task asked for repeated across every
# profile: plain (non-encrypted, non-gst) us<->ffmpeg and us<->tsp,
# reusing the exact same shapes + peer command patterns srt_cells()
# already established — only the cell id (now 3-segment,
# "srt-live/<direction>/<profile>") and the per-cell work-file paths
# (profile-qualified, since every profile's iteration shares this one
# $WORK dir) are new.
srt_live_cells_for_profile() {
  local profile=$1
  local port

  echo "run-matrix: cell srt-live/us-to-ffmpeg/$profile" >&2
  port=$(free_port)
  run_send_peer_recv "srt-live/us-to-ffmpeg/$profile" ffmpeg remux \
    "srt://127.0.0.1:$port?mode=caller" "$WORK/srtlive_us-to-ffmpeg_$profile.ts" -- \
    ffmpeg -y -loglevel warning -i "srt://127.0.0.1:$port?mode=listener" \
    -map 0 -c copy -copy_unknown -f mpegts "$WORK/srtlive_us-to-ffmpeg_$profile.ts"

  echo "run-matrix: cell srt-live/ffmpeg-to-us/$profile" >&2
  port=$(free_port)
  run_peer_send_recv "srt-live/ffmpeg-to-us/$profile" ffmpeg remux \
    "srt://:$port?mode=listener" -- \
    ffmpeg -y -re -loglevel warning -i "$GEN_FILE" -c copy -copy_unknown -f mpegts \
    "srt://127.0.0.1:$port?mode=caller"

  echo "run-matrix: cell srt-live/us-to-tsp/$profile" >&2
  port=$(free_port)
  run_send_peer_recv "srt-live/us-to-tsp/$profile" tsp transparent \
    "srt://127.0.0.1:$port?mode=caller" "$WORK/srtlive_us-to-tsp_$profile.ts" -- \
    tsp -I srt --listener ":$port" -O file "$WORK/srtlive_us-to-tsp_$profile.ts"

  echo "run-matrix: cell srt-live/tsp-to-us/$profile" >&2
  port=$(free_port)
  run_peer_send_recv "srt-live/tsp-to-us/$profile" tsp transparent \
    "srt://:$port?mode=listener" -- \
    tsp -I file "$GEN_FILE" -P regulate -O srt --caller "127.0.0.1:$port" --linger 5
}

# run_axes_for_profile — every cell shape the current $PROFILE's
# iteration calls, both axes: transport (baseline only) + format.
# Shared by the declare pass below (DECLARE_ONLY=1, every shape records
# its id and returns before doing any real work) and the real execution
# loop, so a cell can never be declared by one path and produced by a
# different one.
run_axes_for_profile() {
  # Transport axis stays pinned to "baseline" regardless of how many
  # profiles --profiles lists — matches the ~25-cell transport-axis
  # inventory task 11 built and verified (8 PASS/17 FAIL/0 SKIPPED);
  # scaling it by profile too would multiply that count by up to 12x
  # for no new signal the format axis below doesn't already cover more
  # precisely (analyze/decode/srt-live are the per-profile probes).
  if [[ "$PROFILE" == "baseline" ]]; then
    srt_cells; udp_cells; rist_cells; tcp_cells; hls_cells; rtsp_cells
  fi

  # Format axis: every listed profile.
  analyze_cells_for_profile "$PROFILE"
  decode_cells_for_profile "$PROFILE"
  srt_live_cells_for_profile "$PROFILE"
}

# ---------------------------------------------------------------------
# Run every axis, once per --profiles entry
# ---------------------------------------------------------------------

IFS=',' read -r -a PROFILE_LIST <<<"$PROFILES_ARG"

# ---------------------------------------------------------------------
# Declare pass: enumerate every cell this run WILL produce, before any
# runs. GEN_FILE/GEN_STREAM_SHA are set empty so call-site argument
# expansion under `set -u` is well-defined; no shape reads them before
# its DECLARE_ONLY early-return.
# ---------------------------------------------------------------------
INVENTORY_TSV="$WORK/inventory.tsv"
: >"$INVENTORY_TSV"
DECLARE_ONLY=1
GEN_FILE=""
GEN_STREAM_SHA=""
for PROFILE in "${PROFILE_LIST[@]}"; do
  export PROFILE
  run_axes_for_profile
done
unset DECLARE_ONLY

SHAPE=subset
[[ "$CELLS_GLOB" == "*" && "$PROFILES_ARG" == "$ALL_PROFILE_NAMES" ]] && SHAPE=full-157
jq -n --arg shape "$SHAPE" --arg seconds "$SECONDS_ARG" --arg cells_glob "$CELLS_GLOB" \
  --arg profiles "$PROFILES_ARG" --rawfile tsv "$INVENTORY_TSV" \
  --argjson tools "$(tool_versions_json)" \
  '{shape: $shape, seconds_per_cell: ($seconds | tonumber), cells_glob: $cells_glob,
    profiles: ($profiles | split(",")),
    cells: ($tsv | split("\n") | map(select(length > 0)) | map(split("\t") | {id: .[0], profile: .[1]})),
    allowed_skips: [], tools: $tools}' >"$OUTDIR/inventory.json"
declared=$(jq '.cells | length' "$OUTDIR/inventory.json")
echo "run-matrix: declared $declared cell(s), shape=$SHAPE" >&2
if [[ "$SHAPE" == "full-157" && "$declared" != "157" ]]; then
  echo "run-matrix: FATAL: full shape declared $declared cells, expected 157 — a cell shape changed without this check being updated" >&2
  exit 2
fi

# Same per-seconds budget the per-cell shapes use — gen/verify here do
# real work proportional to --seconds (this scales correctly even for a
# very long --seconds, e.g. Task 14's eventual soak runs), unlike
# REPORT_TIMEOUT's flat floor below (report merge/render's work scales
# with cell *count*, not stream duration).
bootstrap_budget=$(cell_timeout "$SECONDS_ARG")

for PROFILE in "${PROFILE_LIST[@]}"; do
  export PROFILE
  echo "run-matrix: profile=$PROFILE seconds=$SECONDS_ARG cells=$CELLS_GLOB" >&2

  GEN_FILE="$WORK/gensrc-$PROFILE.ts"
  timeout --kill-after=5 "${bootstrap_budget}s" \
    "$BIN" gen --profile "$PROFILE" --seconds "$SECONDS_ARG" --out "$GEN_FILE"
  GEN_VERIFY_JSON="$WORK/gensrc-$PROFILE-verify.json"
  timeout --kill-after=5 "${bootstrap_budget}s" \
    "$BIN" verify --file "$GEN_FILE" --expect "$PROFILE" --seconds "$SECONDS_ARG" --json "$GEN_VERIFY_JSON"
  # Guarded the same way as every per-cell jq read (see
  # run_send_peer_recv's comment) — but unlike a per-cell read, an
  # unparseable $GEN_VERIFY_JSON here is fatal to the WHOLE run (every
  # transparent-tier cell this profile touches needs a real
  # $GEN_STREAM_SHA to compare against), so this fails loudly and exits
  # immediately rather than limping on with every transparent cell
  # reporting a confusing blanket mismatch.
  GEN_STREAM_SHA=$(jq -r '.metrics.stream_sha256' "$GEN_VERIFY_JSON" 2>/dev/null) || GEN_STREAM_SHA=""
  if [[ -z "$GEN_STREAM_SHA" ]]; then
    echo "run-matrix: FATAL: could not read stream_sha256 from $GEN_VERIFY_JSON (unparseable or missing) — cannot proceed" >&2
    exit 2
  fi
  export GEN_FILE GEN_STREAM_SHA

  run_axes_for_profile
done

# ---------------------------------------------------------------------
# Merge + render
# ---------------------------------------------------------------------

echo "run-matrix: merging cell results..." >&2
merge_rc=0
timeout --kill-after=5 "${REPORT_TIMEOUT}s" \
  "$BIN" report merge \
  --cells-dir "$CELLS_DIR" \
  --expectations "$SCRIPT_DIR/expectations.toml" \
  --meta "$OUTDIR/meta.json" \
  --inventory "$OUTDIR/inventory.json" \
  --out "$OUTDIR/results.json" || merge_rc=$?

timeout --kill-after=5 "${REPORT_TIMEOUT}s" \
  "$BIN" report render --in "$OUTDIR/results.json" --out "$OUTDIR/results.md"

echo "run-matrix: wrote $OUTDIR/results.json + $OUTDIR/results.md (exit $merge_rc)" >&2
exit "$merge_rc"
