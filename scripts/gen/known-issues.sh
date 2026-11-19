#!/usr/bin/env bash
# Write (default) or --check the "Known issues" block of the CHANGELOG section
# in preparation, from docs/project/release-findings.md. See
# scripts/gen/known_issues.py.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "${1:---write}" in
  --write|--check) exec python3 -I "$ROOT/scripts/gen/known_issues.py" --root "$ROOT" "${1:---write}" ;;
  *) echo "usage: $0 [--write|--check]" >&2; exit 2 ;;
esac
