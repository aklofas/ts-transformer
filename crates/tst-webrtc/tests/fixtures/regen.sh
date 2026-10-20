#!/usr/bin/env bash
# Regenerate the Annex B fixtures:
# - h264/idr_with_sps_pps.h264, h265/idr_with_vps_sps_pps.h265: one
#   ffmpeg-synthesized 320x240 test-pattern IDR with its parameter sets
#   (H.264: SPS, PPS, IDR; H.265: VPS, SPS, PPS, IDR_W_RADL), used by the
#   packetizer round-trip tests.
# - h264/gop.h264: a 30-frame Constrained Baseline GOP (IDR first, then 29
#   P frames, no B frames), each access unit opened by an access unit
#   delimiter, used by the loopback tests that play a mount to a viewer.
# Encoder SEI messages are removed, and every NAL unit is re-framed with a
# 4-byte start code, the framing the tst-rtp depacketizers emit, so a round
# trip compares byte for byte.
#
# Tools required (locally; not on CI): ffmpeg with libx264 and libx265,
# python3.
#
# Run: ./regen.sh

set -euo pipefail
cd "$(dirname "$0")"
mkdir -p h264 h265

INPUT=(-f lavfi -i 'testsrc=size=320x240:rate=1' -frames:v 1 -pix_fmt yuv420p)

reframe() {
  python3 - "$1" "$2" <<'PY'
import re, sys
data = open(sys.argv[1], "rb").read()
starts = [m.end() for m in re.finditer(b"\x00\x00\x01", data)]
out = bytearray()
for i, s in enumerate(starts):
    end = starts[i + 1] - 3 if i + 1 < len(starts) else len(data)
    nal = data[s:end].rstrip(b"\x00")
    out += b"\x00\x00\x00\x01" + nal
open(sys.argv[2], "wb").write(out)
PY
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

ffmpeg -y -hide_banner -loglevel error "${INPUT[@]}" \
  -c:v libx264 -profile:v high -crf 18 \
  -bsf:v filter_units=remove_types=6 -f h264 "$tmp/h264.raw"
reframe "$tmp/h264.raw" h264/idr_with_sps_pps.h264

ffmpeg -y -hide_banner -loglevel error "${INPUT[@]}" \
  -c:v libx265 -crf 18 -x265-params log-level=none \
  -bsf:v filter_units=remove_types=39 -f hevc "$tmp/h265.raw"
reframe "$tmp/h265.raw" h265/idr_with_vps_sps_pps.h265

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i 'testsrc=size=320x240:rate=30' -frames:v 30 -pix_fmt yuv420p \
  -c:v libx264 -profile:v baseline -bf 0 -g 30 -crf 28 \
  -x264-params aud=1:scenecut=0 \
  -bsf:v filter_units=remove_types=6 -f h264 "$tmp/gop.raw"
reframe "$tmp/gop.raw" h264/gop.h264
