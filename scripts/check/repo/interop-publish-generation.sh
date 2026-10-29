#!/usr/bin/env bash
# interop-publish-generation: publish_generation (scripts/interop/lib.sh) runs
# one publisher generation of the soak's rtsp-publish legs and decides whether
# it ended cleanly. A fake gst-launch-1.0 stands in for the real one: a
# nonzero exit after `Got EOS` is a send failure while closing a stream whose
# media was delivered, a clean end; a nonzero exit before it, or a timeout
# kill after it, is still a failed generation.
#
# Run it:   bash scripts/check/repo/interop-publish-generation.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=../../interop/lib.sh
source "$ROOT/scripts/interop/lib.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# fake <name> <exit code> <line...>: a gst-launch-1.0 stand-in printing the lines.
fake() {
  local f="$work/$1" rc=$2
  shift 2
  {
    echo '#!/usr/bin/env bash'
    local l
    for l in "$@"; do printf 'echo %q\n' "$l"; done
    echo "exit $rc"
  } >"$f"
  chmod +x "$f"
}
EOS='Got EOS from element "pipeline0".'
SEND='ERROR: from element /GstPipeline:pipeline0/GstRTSPClientSink:rtspclientsink0: Could not write to resource.'
fake clean 0 "Setting pipeline to PLAYING ..." "$EOS" "Freeing pipeline ..."
fake post_eos 1 "Setting pipeline to PLAYING ..." "$EOS" "$SEND"
fake pre_eos 1 "Setting pipeline to PLAYING ..." "$SEND"
fake timed_out 124 "Setting pipeline to PLAYING ..." "$EOS"

fails=0
check() { # <name> <want rc> <want substring of the output, or ""> <fake>
  local rc=0 out
  out=$(publish_generation 7 "$work/gen.log" -- "$work/$4") || rc=$?
  if [[ "$rc" -ne "$2" ]]; then
    echo "FAIL: $1: rc $rc, want $2"
    fails=$((fails + 1))
  elif [[ -n "$3" && "$out" != *"$3"* ]]; then
    echo "FAIL: $1: output lacks '$3':"
    echo "$out"
    fails=$((fails + 1))
  elif [[ "$out" != *"$EOS"* && "$4" != pre_eos ]]; then
    echo "FAIL: $1: the generation's own output was not copied to the log"
    fails=$((fails + 1))
  else
    echo "ok: $1"
  fi
}

check "a clean end" 0 "publisher: generation 7 ended rc=0" clean
check "a send failure after EOS is a clean end" 0 \
  "generation 7: post-EOS send failure ($SEND), counted as a clean end" post_eos
check "a failure before EOS fails the generation" 1 "publisher: generation 7 ended rc=1" pre_eos
check "a timeout kill after EOS fails the generation" 124 "publisher: generation 7 ended rc=124" timed_out

if [[ $fails -ne 0 ]]; then
  echo "interop-publish-generation: $fails case(s) failed"
  exit 1
fi
echo "interop-publish-generation: OK"
