#!/usr/bin/env bash
# Regenerate the measured tables in docs/project/benchmarks.md from a
# stress-harness archive (stress-results.json + provenance.json). The
# page's prose is hand-written; only the block between
# `<!-- bench:begin -->` and `<!-- bench:end -->` is generated.
# Usage: scripts/gen/benchmarks-page.sh <archive-dir> [--stdout]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ARCHIVE=${1:?archive dir containing stress-results.json + provenance.json}
shift
case "${1:-}" in
  --stdout) exec python3 "$ROOT/scripts/gen/benchmarks_page.py" --results "$ARCHIVE/stress-results.json" --provenance "$ARCHIVE/provenance.json" ;;
  "")       exec python3 "$ROOT/scripts/gen/benchmarks_page.py" --results "$ARCHIVE/stress-results.json" --provenance "$ARCHIVE/provenance.json" --update "$ROOT/docs/project/benchmarks.md" ;;
  *) echo "unknown arg: $1" >&2; exit 2 ;;
esac
