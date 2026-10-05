#!/usr/bin/env bash
# Network-test stress rail.
#
# Test-binary consolidation raised intra-binary concurrency on the loopback /
# RTSP / multicast tests (cargo runs binaries sequentially; libtest parallelises
# within one). The result is a flake class that a single green run hides,
# including the known aarch64 `tst-srt builder::drop_closes_cleanly`
# accept/close race.
#
# This rail re-runs the network binaries N times under two topologies:
#   * serialized   (`--test-threads=1`) — exercises teardown/Drop ordering;
#   * parallel     (libtest default)    — exercises port/CPU contention.
# It stops on the FIRST failure and prints every command before running it, so a
# flake reproduces with a copy-pasteable line.
#
# This deliberately lives under scripts/dev/, NOT scripts/check/: the pre-push
# loop runs every *.sh under scripts/check/, and stress is far too slow
# for that gate. Run it manually before a release or wire it into a scheduled /
# manual CI workflow — never PR-gating until its runtime and flake rate are
# known (Workstream 7 policy in docs/test-1/2026-05-30-testing-infra-gap-closure-plan.md).
#
# Usage:
#   scripts/dev/stress-network-tests.sh                 # quick scope, 3 loops
#   STRESS_LOOPS=5 scripts/dev/stress-network-tests.sh
#   STRESS_SCOPE=full scripts/dev/stress-network-tests.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# libsrt / librist build from the vendored submodules (mirrors CI + pre-push).
export SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}"
export RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}"

STRESS_LOOPS="${STRESS_LOOPS:-3}"
STRESS_SCOPE="${STRESS_SCOPE:-quick}"

# crate:binary pairs. "quick" is the always-on gating set most prone to flakes
# (SRT loopback + RTSP); "full" adds the secondary transports.
QUICK=(
  "tst-srt:loopback"
  "tst-srt:pipeline"
  "tst-srt:stats"
  "tst-srt:builder"
  "tst-rtp:rtp"
  "tst-rtp:rtcp"
  "tst-rtp:rtsp_client"
  "tst-rtp:rtsp_server"
  "tst-c:receiving"
  "tst-c:transports"
)
FULL_EXTRA=(
  "tst-udp:loopback_unicast"
  "tst-udp:loopback_multicast"
  "tst-udp:pipeline_round_trip"
  "tst-tcp:loopback"
  "tst-tcp:hls_e2e"
  "tst-tcp:pipeline_round_trip"
  "tst-rist:loopback"
  "tst-rist:pipeline_round_trip"
)

case "$STRESS_SCOPE" in
  quick) TARGETS=("${QUICK[@]}") ;;
  full)  TARGETS=("${QUICK[@]}" "${FULL_EXTRA[@]}") ;;
  *)     echo "FAIL: STRESS_SCOPE must be 'quick' or 'full', got '$STRESS_SCOPE'" >&2; exit 1 ;;
esac

echo "stress: scope=$STRESS_SCOPE loops=$STRESS_LOOPS targets=${#TARGETS[@]}"

run() {
  echo "+ $*"
  "$@"
}

for ((loop = 1; loop <= STRESS_LOOPS; loop++)); do
  echo "── loop $loop/$STRESS_LOOPS ──────────────────────────────────────────"
  for pair in "${TARGETS[@]}"; do
    crate="${pair%%:*}"
    bin="${pair##*:}"
    # Serialized: teardown/Drop ordering determinism.
    run cargo test -p "$crate" --test "$bin" -- --test-threads=1
    # Parallel: port/CPU contention (libtest default thread count).
    run cargo test -p "$crate" --test "$bin"
  done
done

echo "stress: PASS — ${STRESS_LOOPS} loop(s) over ${#TARGETS[@]} binaries, no failures"
