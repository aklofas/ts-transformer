#!/usr/bin/env bash
# The disk cleanup of the free-disk and rust-setup actions (Linux runners), one
# mode per step of theirs:
#   free-disk.sh before          prints `df -h /`, writes avail_before=<bytes>
#                                to $GITHUB_OUTPUT
#   free-disk.sh clean <bytes>   removes the unused preinstalled toolsets when
#                                fewer than 30 GiB were free
#   free-disk.sh after <bytes>   prints `df -h /` and the before/after line, and
#                                appends that line to $GITHUB_STEP_SUMMARY
# A reading that is empty or not a number counts as short of space, so the
# removal runs. FREE_DISK_PATHS and FREE_DISK_THRESHOLD (bytes) replace the
# toolset list and the 30 GiB threshold for the self-test only.
set -euo pipefail

threshold="${FREE_DISK_THRESHOLD:-32212254720}"
paths="${FREE_DISK_PATHS:-/usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL /usr/local/share/boost}"

avail() { df -B1 --output=avail / | tail -1 | tr -d ' '; }
roomy() { [ "$1" -ge "$threshold" ]; }
skip_line() { awk -v b="$1" 'BEGIN{printf "free-disk: %.1f GiB free, removal skipped (runs below 30 GiB)\n", b/1073741824}'; }

case "${1:-}" in
  before)
    df -h /
    echo "avail_before=$(avail)" >> "$GITHUB_OUTPUT"
    ;;
  clean)
    before="${2:-}"
    # Runners with room to spare keep their toolsets: the removal costs
    # minutes and only pays when the disk is short (30 GiB).
    if roomy "$before"; then
      skip_line "$before"
      exit 0
    fi
    # Every removal is bounded and timed: this step prints nothing on its
    # own, so a wedged runner here looked like a slow rm (45 min to the job
    # cap, once). A bounded removal that times out costs the space it would
    # have freed, nothing else.
    for p in $paths; do
      [ -e "$p" ] || continue
      t0=$SECONDS
      sudo timeout 300 rm -rf "$p" || echo "free-disk: rm $p exited $? (bounded at 300 s)"
      echo "free-disk: $p removed in $((SECONDS - t0)) s"
    done
    # No docker image prune: the runner builds a Docker action's container
    # before the first step, and `prune -a` deleted it (cargo-deny then
    # failed at `docker run` with "Unable to find image").
    ;;
  after)
    before="${2:-}"
    df -h /
    if roomy "$before"; then
      line="$(skip_line "$before")"
    else
      line="$(awk -v b="$before" -v a="$(avail)" 'BEGIN{g=1073741824; printf "free-disk: %.1f GiB → %.1f GiB (+%.1f GiB)", b/g, a/g, (a-b)/g}')"
    fi
    echo "$line"
    echo "$line" >> "$GITHUB_STEP_SUMMARY"
    ;;
  *)
    echo "usage: free-disk.sh before | clean <bytes> | after <bytes>" >&2
    exit 2
    ;;
esac
