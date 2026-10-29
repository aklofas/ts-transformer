#!/usr/bin/env bash
# interop-subset: `run-matrix.sh --only-subset <name>` runs the cells listed in
# expectations.toml's `[<name>_subset]` table. subset_cells (scripts/interop/
# lib.sh) reads that list; it must agree with a TOML parser on the live file
# and refuse a malformed table, and cell_selected must then select exactly the
# listed patterns.
#
# Run it:   bash scripts/check/repo/interop-subset.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=../../interop/lib.sh
source "$ROOT/scripts/interop/lib.sh"
EXPECTATIONS="$ROOT/scripts/interop/expectations.toml"

fails=0
fail() { echo "FAIL: $*"; fails=$((fails + 1)); }

got=$(subset_cells "$EXPECTATIONS" arm64) || fail "subset_cells could not read the live arm64 table"
want=$(python3 -I -c 'import sys, tomllib
print("\n".join(tomllib.load(open(sys.argv[1], "rb"))["arm64_subset"]["cells"]))' "$EXPECTATIONS")
if [[ "$got" == "$want" ]]; then echo "ok: the live arm64 list matches a TOML parser"; else
  fail "subset_cells read '$got', a TOML parser '$want'"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
refuse() { # <name> <file text>
  printf '%s' "$2" >"$work/e.toml"
  if subset_cells "$work/e.toml" arm64 >/dev/null 2>&1; then fail "accepted $1"; else echo "ok: refuses $1"; fi
}
refuse "a missing table" $'[census]\nfull_cells = 3\n'
refuse "an unterminated list" $'[arm64_subset]\ncells = [\n  "srt/*",\n'
refuse "an empty list" $'[arm64_subset]\ncells = [\n]\n'
refuse "an unquoted pattern" $'[arm64_subset]\ncells = [\n  srt/*\n]\n'
refuse "another key" $'[arm64_subset]\nlist = [\n  "srt/*"\n]\n'

dead=$(subset_dead_patterns $'srt/*\nmux/*\nrist/us-to-tsp\nklv/*' $'srt/us-to-tsp\nrist/us-to-tsp\nwhep/x')
if [[ "$dead" == $'mux/*\nklv/*' ]]; then echo "ok: names the patterns that match no declared cell"; else
  fail "subset_dead_patterns gave '$dead'"
fi
[[ -z $(subset_dead_patterns "$got" $'srt/a\nsrt-live/a/b\nrist/a\nrtsp-serve/ffmpeg-pull\nrtsp-publish/a\nanalyze/tsanalyze/a\nanalyze/tsp-analyze/a') ]] \
  && echo "ok: every live pattern matches its family" || fail "a live pattern matched nothing"

CELLS_GLOB="*"
SUBSET_CELLS="$got"
for id in srt/us-to-ffmpeg srt/tsp-to-us-encrypted srt-live/us-to-tsp/h265-klv rist/ffmpeg-to-us \
  rtsp-serve/ffmpeg-pull rtsp-publish/gst-push-es-klv analyze/tsanalyze/baseline \
  analyze/tsp-analyze/two-program; do
  cell_selected "$id" && echo "ok: selects $id" || fail "did not select $id"
done
for id in whep/chromium-sei-dc-klv decode/vlc/baseline decode/mpv/audio hls/ffmpeg-pull \
  analyze/ffprobe/baseline rtsp-serve/vlc-probe rtsp-consume/vlc-serve-ffmpeg-pull udp/us-to-tsp; do
  cell_selected "$id" && fail "selected $id" || echo "ok: leaves out $id"
done
# --cells still narrows inside the subset.
CELLS_GLOB="rist/*"
cell_selected srt/us-to-ffmpeg && fail "--cells did not narrow the subset" || echo "ok: --cells narrows the subset"
SUBSET_CELLS=""
cell_selected rist/us-to-tsp && echo "ok: no subset selects by --cells alone" || fail "no subset broke --cells"

if [[ $fails -ne 0 ]]; then
  echo "interop-subset: $fails case(s) failed"
  exit 1
fi
echo "interop-subset: OK"
