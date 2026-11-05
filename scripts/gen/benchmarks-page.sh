#!/usr/bin/env bash
# Regenerate the measured tables in docs/project/benchmarks.md from a
# stress-harness archive (stress-results.json + provenance.json). The
# page's prose is hand-written; only the block between
# `<!-- bench:begin -->` and `<!-- bench:end -->` is generated.
# Usage: scripts/gen/benchmarks-page.sh <archive-dir> [--arm <arm-archive-dir>] [--stdout]
# With --arm the block opens with a ceilings-by-architecture table.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ARCHIVE=${1:?archive dir containing stress-results.json + provenance.json}
shift
ARGS=(--results "$ARCHIVE/stress-results.json" --provenance "$ARCHIVE/provenance.json")
STDOUT=0
while [ $# -gt 0 ]; do
  case "$1" in
    --stdout) STDOUT=1; shift ;;
    --arm)
      [ $# -ge 2 ] || { echo "usage: --arm <arm64 archive dir>" >&2; exit 2; }
      ARM=$2
      ARGS+=(--arm-results "$ARM/stress-results.json" --arm-provenance "$ARM/provenance.json")
      shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
if [ "$STDOUT" = 1 ]; then
  exec python3 "$ROOT/scripts/gen/benchmarks_page.py" "${ARGS[@]}"
fi
exec python3 "$ROOT/scripts/gen/benchmarks_page.py" "${ARGS[@]}" --update "$ROOT/docs/project/benchmarks.md"
