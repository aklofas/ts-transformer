#!/usr/bin/env bash
# interop-analyze-counters: tsanalyze_ts_line_counters_zero (scripts/interop/lib.sh)
# is the verdict of the analyze/tsanalyze/* cells. Each case below feeds it a
# `tsanalyze --normalized` capture shaped like the real output and checks the
# line it echoes: "0" for a clean stream, a named counter otherwise. The
# per-PID cases are the leap gate: a PCR or PTS that jumps backward (or by
# more than a second) or a continuity break on any PID fails the cell, except
# the one leap TSDuck counts when a clock wraps at 2^33 inside the capture.
#
# Run it:   bash scripts/check/repo/interop-analyze-counters.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=../../interop/lib.sh
source "$ROOT/scripts/interop/lib.sh"

TS='ts:id=1:services=1:pids=4:packets=12994:invalidsyncs=0:transporterrors=0:suspectignored=0:bytes=2442872:'
PAT='pid:pid=0:access=clear:packets=100:pcr=0:pts=0:dts=0:pcrleap=0:ptsleap=0:dtsleap=0:discontinuities=0:duplicated=0:description=PAT'
video() { # <pcrleap> <ptsleap> <discontinuities> <firstpcr> <lastpcr> <firstpts> <lastpts>
  echo "pid:pid=4113:access=clear:video:packets=12694:pcr=150:pts=300:dts=0:pcrleap=$1:ptsleap=$2:dtsleap=0:discontinuities=$3:duplicated=0:pes=300:firstpcr=$4:lastpcr=$5:firstpts=$6:lastpts=$7:description=AVC video"
}
CLEAN_VIDEO=$(video 0 0 0 0 268200000 0 897000)

fails=0
check() { # <name> <expected echo> <capture>
  local got
  got=$(tsanalyze_ts_line_counters_zero "$3")
  if [[ "$got" == "$2" ]]; then
    echo "ok: $1"
  else
    echo "FAIL: $1: expected '$2', got '$got'"
    fails=$((fails + 1))
  fi
}

check "a clean capture" "0" "$TS
$PAT
$CLEAN_VIDEO"
check "a global counter" \
  "nonzero counters: invalidsyncs=0 transporterrors=2 suspectignored=0" \
  "${TS/transporterrors=0/transporterrors=2}
$PAT
$CLEAN_VIDEO"
check "a PCR that steps back" \
  "nonzero pid counters: pid=4113 pcrleap=1 ptsleap=0 discontinuities=0" "$TS
$PAT
$(video 1 0 0 0 268200000 0 897000)"
check "a PTS leap" \
  "nonzero pid counters: pid=4113 pcrleap=0 ptsleap=3 discontinuities=0" "$TS
$PAT
$(video 0 3 0 0 268200000 0 897000)"
check "a continuity break on the PAT, the first offending PID" \
  "nonzero pid counters: pid=0 pcrleap=0 ptsleap=0 discontinuities=2" "$TS
${PAT/discontinuities=0/discontinuities=2}
$(video 1 0 0 0 268200000 0 897000)"
check "one leap per clock that wraps at 2^33 in the capture" "0" "$TS
$PAT
$(video 1 1 0 2576845377600 133200000 8589484592 447000)"
check "a backward reset that does not wrap is not excused" \
  "nonzero pid counters: pid=4113 pcrleap=1 ptsleap=1 discontinuities=0" "$TS
$PAT
$(video 1 1 0 900000000 27000000 3000000 90000)"
check "a reset from the top half of the range to near zero is not a wrap" \
  "nonzero pid counters: pid=4113 pcrleap=1 ptsleap=1 discontinuities=0" "$TS
$PAT
$(video 1 1 0 1500000000000 133200000 5000000000 447000)"
check "a step-back on top of a wrap" \
  "nonzero pid counters: pid=4113 pcrleap=2 ptsleap=1 discontinuities=0" "$TS
$PAT
$(video 2 1 0 2576845377600 133200000 8589484592 447000)"
check "a leap on a clock that did not wrap, beside one that did" \
  "nonzero pid counters: pid=4113 pcrleap=0 ptsleap=1 discontinuities=0" "$TS
$PAT
$(video 0 1 0 2576845377600 133200000 0 447000)"
check "a pid: line without ptsleap" "could not parse pid counters" "$TS
${PAT/ptsleap=0:/}
$CLEAN_VIDEO"
check "no pid: line at all" "could not parse pid counters" "$TS"

if [[ $fails -ne 0 ]]; then
  echo "interop-analyze-counters: $fails case(s) failed"
  exit 1
fi
echo "interop-analyze-counters: OK"
