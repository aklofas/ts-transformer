#!/usr/bin/env bash
# Regenerate the lines-of-code tables in docs/project/code-size.md.
#
# Thin wrapper over scripts/gen/loc_report.py (stdlib Python 3): counts every
# git-tracked source file, splits Rust inline `#[cfg(test)]` modules out of
# the production code they sit next to, and rewrites the block between
# `<!-- loc:begin -->` and `<!-- loc:end -->` in the page. The page's prose
# is hand-written; only that block is generated.
#
# This is a report, not a gate: LOC drifts every PR, so nothing in CI checks
# the page for staleness. Regenerate when the numbers matter (a release, a
# refactor arc) and commit the result.
#
# Usage:
#   scripts/gen/loc-report.sh            # update docs/project/code-size.md
#   scripts/gen/loc-report.sh --stdout   # print the Markdown tables instead
#   scripts/gen/loc-report.sh --json     # per-file JSON on stdout
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "${1:-}" in
  --stdout) exec python3 "$ROOT/scripts/gen/loc_report.py" ;;
  --json)   exec python3 "$ROOT/scripts/gen/loc_report.py" --json ;;
  "")       exec python3 "$ROOT/scripts/gen/loc_report.py" --update "$ROOT/docs/project/code-size.md" ;;
  -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
  *) echo "unknown arg: $1" >&2; exit 2 ;;
esac
