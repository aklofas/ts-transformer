#!/usr/bin/env bash
# Write (default) or --check the break-count table of docs/reference/api-stability.md
# from tests/coverage/api-churn.toml. See scripts/gen/api_churn.py.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "${1:---write}" in
  --write|--check) exec python3 -I "$ROOT/scripts/gen/api_churn.py" render --root "$ROOT" "${1:---write}" ;;
  *) echo "usage: $0 [--write|--check]" >&2; exit 2 ;;
esac
