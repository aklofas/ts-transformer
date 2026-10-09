#!/usr/bin/env bash
# Bash ratchet: every native call in the Python transport modules that can
# park must release the GIL through `util::allow_threads_parking`, not a
# bare `py.allow_threads(...)`.
#
# Why: a thread that wakes from a native call after interpreter
# finalisation began and re-takes the GIL is killed by CPython; that forced
# unwind crosses PyO3's panic trampoline and the process aborts (exit 134).
# `allow_threads_parking` parks such a thread for good instead.
#
# Every bare `.allow_threads(` site under
# bindings/python/src/{srt,rtp,udp,tcp,rist,hls}/ is listed as
# `path<TAB>fn` (fn = the nearest preceding `fn name`) and compared to
# scripts/ratchets/py-allow-threads-bare.tsv. Fails on a site not listed
# and on a listed row whose site no longer exists.
#
# bash 3.2 / BSD awk portable.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
ALLOW="scripts/ratchets/py-allow-threads-bare.tsv"

found=$(
  for d in srt rtp udp tcp rist hls; do
    for f in bindings/python/src/"$d"/*.rs; do
      [ -f "$f" ] || continue
      awk -v path="$f" '
        /^[ \t]*\/\// { next }
        {
          if (match($0, /fn [A-Za-z0-9_]+/)) {
            fn = substr($0, RSTART + 3, RLENGTH - 3)
          }
          if ($0 ~ /\.allow_threads\(/) {
            print path "\t" fn
          }
        }
      ' "$f"
    done
  done | sort -u
)

allowed=$(grep -v '^#' "$ALLOW" | grep -v '^[[:space:]]*$' | cut -f1,2 | sort -u)

fail=0
unlisted=$(comm -23 <(printf '%s\n' "$found" | grep -v '^$' || true) <(printf '%s\n' "$allowed" | grep -v '^$' || true))
stale=$(comm -13 <(printf '%s\n' "$found" | grep -v '^$' || true) <(printf '%s\n' "$allowed" | grep -v '^$' || true))

if [ -n "$unlisted" ]; then
  echo "finalize-guard: bare py.allow_threads( site(s) not in $ALLOW:" >&2
  printf '  %s\n' "$unlisted" >&2
  echo "  a new parking native must use util::allow_threads_parking; a non-parking one must be listed" >&2
  fail=1
fi
if [ -n "$stale" ]; then
  echo "finalize-guard: stale row(s) in $ALLOW (no bare site left there):" >&2
  printf '  %s\n' "$stale" >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "finalize-guard: OK ($(printf '%s\n' "$allowed" | grep -c . || true) allowed bare site(s))"
