#!/usr/bin/env bash
#
# Cross-implementation byte-diff harness.
#
# Compares our `Muxer` output against ffmpeg's MPEG-TS muxer and tsduck's
# `tsp` muxer across a small content matrix that exercises representative
# pipeline shapes (video-only, video+KLV, video+audio, video+audio+KLV,
# multi-program, H.265+KLV). Helps gauge how "normal-looking" our output
# is to consumer libraries.
#
# How this differs from release-validation.sh's ffmpeg/tsduck steps:
#   - `release-validation.sh` golden-diffs our OWN output against committed
#     reference files (regression check: did our muxer change?).
#   - This harness compares ACROSS implementations (drift check: do we
#     emit similar structure to ffmpeg / tsduck for the same logical
#     content shape?).
#
# Reference input strategy
# ------------------------
# ffmpeg can't synthesize KLV from null input, and our synthetic muxer
# baselines use minimal parameter-set bytes that ffmpeg's H.264/H.265
# parsers reject for re-extraction (a documented limitation, see
# release-validation.sh step 6).
#
# Workaround: for ffmpeg references, we generate synthetic video via
# lavfi (`testsrc2` source) and encode through ffmpeg's real codec. This
# guarantees a valid bitstream while preserving the structural comparison
# we care about (PSI shape, descriptor inventory, PCR cadence, adaptation
# usage). The elementary stream PAYLOAD differs (lavfi vs our synthetic
# bytes) but the container-level wire format is what this harness checks.
#
# For tsduck reference: tsduck doesn't have a native muxer in the same
# shape as ffmpeg; instead we re-process our output through tsp's filter
# plugin (a packet-passthrough that re-emits PSI). Any divergence between
# ours and the tsp re-emit indicates places where our encoding is
# non-canonical even if spec-conformant.
#
# Skip behavior
# -------------
# Any missing tool (ffmpeg / tsanalyze / tspsi / tsp) emits a per-case
# "tool-missing" cell and continues; the script never aborts on missing
# external tools.
#
# Usage:
#   scripts/dev/cross-impl-byte-diff.sh [--artifacts-dir PATH] [--keep-tmp]
#     --artifacts-dir PATH  destination for per-case diff artifacts
#                           (default: target/i5-byte-diff-artifacts/)
#     --keep-tmp            leave the per-run output dir in place for inspection
#                           (default: clean up via trap on exit)
#
# Outputs
# -------
# Per-case raw artifact files under <artifacts-dir>/case-<id>/:
#   tsanalyze.{ours,ffmpeg,tsduck}.txt
#   tspsi.{ours,ffmpeg,tsduck}.txt
#   ffprobe.{ours,ffmpeg,tsduck}.json
#   <dim>.ours-vs-{ffmpeg,tsduck}.diff
# Summary table is printed to stdout for paste-into-results-doc.

set -euo pipefail

# Resolve to the workspace root: this script lives at <workspace>/scripts/.
cd "$(dirname "$0")/../.."

ARTIFACTS_DIR="target/i5-byte-diff-artifacts"
TMP_DIR="$(mktemp -d)"
KEEP_TMP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --artifacts-dir) ARTIFACTS_DIR="$2"; shift 2 ;;
    --keep-tmp) KEEP_TMP=1; shift ;;
    -h|--help)
      sed -n '3,57p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [ "$KEEP_TMP" = "0" ]; then
  trap 'rm -rf "$TMP_DIR"' EXIT
else
  echo "  (--keep-tmp: temp dir is $TMP_DIR)"
fi

mkdir -p "$ARTIFACTS_DIR"

have() { command -v "$1" >/dev/null 2>&1; }
header() { echo; echo "=== [$1] $2 ==="; }

# Filter noise out of tsanalyze diffs: timestamp lines that vary per run.
TSANALYZE_FILTER='/^time:(utc|local):system:/d'

# Strip control characters from tspsi output so PSI dumps remain
# human-readable when piped into diff (binary payload bleed-through
# turns the file into mojibake otherwise).
TSPSI_FILTER='s/[[:cntrl:]]//g'

# Discover tool availability up front.
HAVE_FFMPEG=0; have ffmpeg && HAVE_FFMPEG=1
HAVE_FFPROBE=0; have ffprobe && HAVE_FFPROBE=1
HAVE_TSANALYZE=0; have tsanalyze && HAVE_TSANALYZE=1
HAVE_TSPSI=0; have tspsi && HAVE_TSPSI=1
HAVE_TSP=0; have tsp && HAVE_TSP=1

header "info" "Tool availability"
echo "  ffmpeg:    $([ $HAVE_FFMPEG = 1 ] && echo OK || echo missing)"
echo "  ffprobe:   $([ $HAVE_FFPROBE = 1 ] && echo OK || echo missing)"
echo "  tsanalyze: $([ $HAVE_TSANALYZE = 1 ] && echo OK || echo missing)"
echo "  tspsi:     $([ $HAVE_TSPSI = 1 ] && echo OK || echo missing)"
echo "  tsp:       $([ $HAVE_TSP = 1 ] && echo OK || echo missing)"
[ $HAVE_FFMPEG = 1 ] && echo "  $(ffmpeg -version 2>&1 | head -1)"
[ $HAVE_TSP = 1 ] && echo "  $(tsp --version 2>&1 | head -1)"

# Tool version capture for the results doc.
TOOL_VERSIONS_FILE="$TMP_DIR/tool-versions.txt"
{
  echo "ffmpeg:    $(ffmpeg -version 2>&1 | head -1 || echo missing)"
  echo "ffprobe:   $(ffprobe -version 2>&1 | head -1 || echo missing)"
  echo "tsanalyze: $(tsanalyze --version 2>&1 | head -1 || echo missing)"
  echo "tspsi:     $(tspsi --version 2>&1 | head -1 || echo missing)"
  echo "tsp:       $(tsp --version 2>&1 | head -1 || echo missing)"
  echo "rust:      $(cargo --version 2>&1)"
  echo "git SHA:   $(git rev-parse HEAD 2>&1)"
} > "$TOOL_VERSIONS_FILE"

# ------------------------------------------------------------------
# Helpers — capture each analysis dimension for one input file.

# Capture all three analysis dimensions for one file into a directory.
# $1 input file, $2 output dir, $3 label tag for filenames.
capture_dimensions() {
  local input="$1"
  local outdir="$2"
  local tag="$3"
  mkdir -p "$outdir"

  if [ $HAVE_TSANALYZE = 1 ]; then
    tsanalyze --normalized "$input" 2>/dev/null \
      | sed -E "$TSANALYZE_FILTER" > "$outdir/tsanalyze.$tag.txt" || true
  else
    echo "tool-missing" > "$outdir/tsanalyze.$tag.txt"
  fi

  if [ $HAVE_TSPSI = 1 ]; then
    tspsi "$input" 2>/dev/null \
      | sed -E "$TSPSI_FILTER" > "$outdir/tspsi.$tag.txt" || true
  else
    echo "tool-missing" > "$outdir/tspsi.$tag.txt"
  fi

  if [ $HAVE_FFPROBE = 1 ]; then
    # -show_streams + -show_format only; -show_packets adds 10k+ lines
    # of per-packet timing data that swamps cross-impl signal.
    ffprobe -v error -show_streams -show_format -of json "$input" 2>/dev/null \
      > "$outdir/ffprobe.$tag.json" || echo '{}' > "$outdir/ffprobe.$tag.json"
  else
    echo '{}' > "$outdir/ffprobe.$tag.json"
  fi
}

# Diff two captures and write a per-dimension summary line. Echoes a
# pipe-delimited row suitable for pasting into the results table.
# $1 case_id, $2 outdir, $3 dim (tsanalyze|tspsi|ffprobe),
# $4 ours_tag, $5 ref_tag, $6 ref_label (ffmpeg|tsduck)
diff_dimension() {
  local case_id="$1"
  local outdir="$2"
  local dim="$3"
  local ours_tag="$4"
  local ref_tag="$5"
  local ref_label="$6"
  local ext="txt"
  [ "$dim" = "ffprobe" ] && ext="json"
  local ours_file="$outdir/$dim.$ours_tag.$ext"
  local ref_file="$outdir/$dim.$ref_tag.$ext"
  local diff_file="$outdir/$dim.$ours_tag-vs-$ref_label.diff"

  if [ ! -s "$ours_file" ] || [ ! -s "$ref_file" ]; then
    printf "  %s  %-10s (%-7s): SKIP (missing capture)\n" "$case_id" "$dim" "$ref_label"
    return
  fi

  if grep -q '^tool-missing$' "$ours_file" 2>/dev/null \
     || grep -q '^tool-missing$' "$ref_file" 2>/dev/null; then
    printf "  %s  %-10s (%-7s): SKIP (tool missing)\n" "$case_id" "$dim" "$ref_label"
    return
  fi

  # Treat 2-byte empty-JSON ({}) as no-reference too.
  if [ "$dim" = "ffprobe" ] && [ "$(stat -c '%s' "$ref_file")" -le 3 ]; then
    printf "  %s  %-10s (%-7s): SKIP (no reference)\n" "$case_id" "$dim" "$ref_label"
    return
  fi

  if diff -q "$ours_file" "$ref_file" >/dev/null 2>&1; then
    printf "  %s  %-10s (%-7s): IDENTICAL\n" "$case_id" "$dim" "$ref_label"
    : > "$diff_file"
  else
    local lines
    lines=$(diff -u "$ours_file" "$ref_file" 2>/dev/null | wc -l || true)
    diff -u "$ours_file" "$ref_file" > "$diff_file" 2>/dev/null || true
    printf "  %s  %-10s (%-7s): DIFFERS (%s diff lines)\n" "$case_id" "$dim" "$ref_label" "$lines"
  fi
}

# ------------------------------------------------------------------
# Generate "ours" outputs via existing examples. Each example accepts a
# single positional output-path argument; mux_to_file additionally
# accepts a duration (we keep it short for snappy captures).

header "1/6" "Generate 'ours' baselines"
SRT_FORCE_VENDORED=1 cargo build -q -p tst-examples

OURS_A="$TMP_DIR/ours-A.ts"   # mux_to_file: H.264 + private-data KLV (async)
OURS_B="$TMP_DIR/ours-B.ts"   # mux_h265_with_klv: H.265 + sync-KLV (AU cell)
OURS_C="$TMP_DIR/ours-C.ts"   # mux_audio_video_klv: H.264 + MP2 + KLV
OURS_D="$TMP_DIR/ours-D.ts"   # mux_h266_with_klv: H.266 + sync-KLV
OURS_E_IN1="$TMP_DIR/ours-E-prog1.ts"
OURS_E_IN2="$TMP_DIR/ours-E-prog2.ts"
OURS_E="$TMP_DIR/ours-E.ts"   # repack_two_programs: H.264 + H.265 in 2 programs

SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_to_file -- "$OURS_A" 3
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_h265_with_klv -- "$OURS_B"
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_audio_video_klv -- "$OURS_C"
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_h266_with_klv -- "$OURS_D"
# Two-program needs two single-program inputs.
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_to_file -- "$OURS_E_IN1" 3
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example mux_h265_with_klv -- "$OURS_E_IN2"
SRT_FORCE_VENDORED=1 cargo run -q -p tst-examples --example repack_two_programs -- \
    "$OURS_E_IN1" "$OURS_E_IN2" "$OURS_E" >/dev/null

for f in "$OURS_A" "$OURS_B" "$OURS_C" "$OURS_D" "$OURS_E"; do
  [ -s "$f" ] || { echo "FAIL: $(basename "$f") was not produced"; exit 1; }
  echo "  $(basename "$f"): $(stat -c '%s' "$f") bytes"
done

# ------------------------------------------------------------------
# Generate ffmpeg references. ffmpeg has no synthetic-KLV input so KLV
# cases compare only structural dimensions that are content-agnostic
# (PSI cadence, PMT descriptor inventory, PCR placement). lavfi sources
# give us well-formed bitstreams that downstream tools can parse.

header "2/6" "Generate ffmpeg references"
FFMPEG_REF_A="$TMP_DIR/ref-A-ffmpeg.ts"
FFMPEG_REF_B="$TMP_DIR/ref-B-ffmpeg.ts"
FFMPEG_REF_C="$TMP_DIR/ref-C-ffmpeg.ts"
FFMPEG_REF_D="$TMP_DIR/ref-D-ffmpeg.ts"
FFMPEG_REF_E="$TMP_DIR/ref-E-ffmpeg.ts"

if [ $HAVE_FFMPEG = 1 ]; then
  # Case A: H.264 video-only reference.
  ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
    -c:v libx264 -preset ultrafast -tune zerolatency -g 30 \
    -f mpegts "$FFMPEG_REF_A" || true

  # Case B: H.265 video reference (KLV omitted, ffmpeg has no source).
  ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=5 \
    -c:v libx265 -preset ultrafast -tag:v hvc1 -g 30 \
    -f mpegts "$FFMPEG_REF_B" || true

  # Case C: H.264 video + MP2 audio reference (KLV omitted).
  ffmpeg -v error -y \
    -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
    -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
    -c:v libx264 -preset ultrafast -g 30 \
    -c:a mp2 -b:a 128k -t 3 \
    -f mpegts "$FFMPEG_REF_C" || true

  # Case D: H.266 / VVC reference. libvvenc support is sparse; fall
  # back to libx265 (PSI structure is still meaningful as a comparison
  # point, the stream_type byte differs).
  if ffmpeg -hide_banner -encoders 2>&1 | grep -q libvvenc; then
    ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
      -c:v libvvenc -g 30 -f mpegts "$FFMPEG_REF_D" || true
  else
    echo "  Case D: libvvenc unavailable, substituting libx265 (note in results)"
    ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
      -c:v libx265 -preset ultrafast -g 30 -f mpegts "$FFMPEG_REF_D" || true
  fi

  # Case E: ffmpeg's mpegts muxer is single-program by API. We emit a
  # one-program reference and note the asymmetry in the results doc;
  # there's no realistic two-program reference we can produce inline.
  ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
    -c:v libx264 -preset ultrafast -g 30 \
    -f mpegts -mpegts_service_id 1 \
    "$FFMPEG_REF_E" || true
fi

for f in "$FFMPEG_REF_A" "$FFMPEG_REF_B" "$FFMPEG_REF_C" "$FFMPEG_REF_D" "$FFMPEG_REF_E"; do
  if [ -s "$f" ]; then
    echo "  $(basename "$f"): $(stat -c '%s' "$f") bytes"
  else
    echo "  $(basename "$f"): NOT PRODUCED"
  fi
done

# ------------------------------------------------------------------
# tsduck reference: re-emit our own output through tsp's filter plugin.
# This is a passthrough that exercises tsp's packet-deserialization +
# packet-reserialization paths. Any divergence is a hint at our encoding
# being non-canonical (descriptor packing order, padding byte selection,
# adaptation-field byte choices) even if both end states are valid.

header "3/6" "Generate tsduck references (tsp passthrough)"
TSDUCK_REF_A="$TMP_DIR/ref-A-tsduck.ts"
TSDUCK_REF_B="$TMP_DIR/ref-B-tsduck.ts"
TSDUCK_REF_C="$TMP_DIR/ref-C-tsduck.ts"
TSDUCK_REF_D="$TMP_DIR/ref-D-tsduck.ts"
TSDUCK_REF_E="$TMP_DIR/ref-E-tsduck.ts"

if [ $HAVE_TSP = 1 ]; then
  for pair in "A:$OURS_A:$TSDUCK_REF_A" "B:$OURS_B:$TSDUCK_REF_B" \
              "C:$OURS_C:$TSDUCK_REF_C" "D:$OURS_D:$TSDUCK_REF_D" \
              "E:$OURS_E:$TSDUCK_REF_E"; do
    IFS=':' read -r label src dst <<< "$pair"
    # `-P filter --negate --pid 8191` drops only PID 0x1FFF (null
    # padding) — a near-noop that still forces tsp's PSI / packet
    # serializer to round-trip every other packet, exposing
    # canonicalization gaps in our encoder.
    if tsp -I file "$src" -P filter --negate --pid 8191 -O file "$dst" >/dev/null 2>&1; then
      echo "  $(basename "$dst"): $(stat -c '%s' "$dst") bytes"
    else
      echo "  $(basename "$dst"): tsp passthrough failed"
      : > "$dst"
    fi
  done
fi

# ------------------------------------------------------------------
# Capture analysis dimensions for every (case, source) tuple.

header "4/6" "Capture analysis dimensions"
for case_id in A B C D E; do
  case_dir="$ARTIFACTS_DIR/case-$case_id"
  rm -rf "$case_dir"; mkdir -p "$case_dir"

  # Indirect lookup case_id -> file var.
  ours_var="OURS_$case_id"
  ffmpeg_var="FFMPEG_REF_$case_id"
  tsduck_var="TSDUCK_REF_$case_id"
  ours_file="${!ours_var}"
  ffmpeg_file="${!ffmpeg_var}"
  tsduck_file="${!tsduck_var}"

  echo "  case $case_id"
  capture_dimensions "$ours_file" "$case_dir" "ours"
  if [ -s "$ffmpeg_file" ]; then
    capture_dimensions "$ffmpeg_file" "$case_dir" "ffmpeg"
  else
    for d in tsanalyze tspsi; do echo "tool-missing" > "$case_dir/$d.ffmpeg.txt"; done
    echo '{}' > "$case_dir/ffprobe.ffmpeg.json"
  fi
  if [ -s "$tsduck_file" ]; then
    capture_dimensions "$tsduck_file" "$case_dir" "tsduck"
  else
    for d in tsanalyze tspsi; do echo "tool-missing" > "$case_dir/$d.tsduck.txt"; done
    echo '{}' > "$case_dir/ffprobe.tsduck.json"
  fi
done

# ------------------------------------------------------------------
# Per-dimension diffs.

header "5/6" "Diff dimensions per case (ours vs reference)"
for case_id in A B C D E; do
  case_dir="$ARTIFACTS_DIR/case-$case_id"
  for dim in tsanalyze tspsi ffprobe; do
    diff_dimension "$case_id" "$case_dir" "$dim" "ours" "ffmpeg" "ffmpeg"
    diff_dimension "$case_id" "$case_dir" "$dim" "ours" "tsduck" "tsduck"
  done
done

# ------------------------------------------------------------------
# Tool-version copy alongside artifacts for traceability.

header "6/6" "Persist tool versions"
cp "$TOOL_VERSIONS_FILE" "$ARTIFACTS_DIR/tool-versions.txt"
echo "  wrote $ARTIFACTS_DIR/tool-versions.txt"

echo
echo "Cross-impl byte-diff complete."
echo "Per-case artifacts: $ARTIFACTS_DIR/case-{A,B,C,D,E}/"
echo "Tool versions:      $ARTIFACTS_DIR/tool-versions.txt"
