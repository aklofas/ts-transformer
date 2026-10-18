#!/usr/bin/env bash
# The qualification record and its evidence-page block. See scripts/gen/qualification.py.
#   qualification.sh [--write|--check]   render docs/project/qualification.json into the page
#                                        (no JSON committed = the page says none is recorded)
#   qualification.sh collect|render|checksums|archives ...   pass straight through
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PY=(python3 -I "$ROOT/scripts/gen/qualification.py")
JSON="$ROOT/docs/project/qualification.json"
case "${1:---write}" in
  --write|--check) exec "${PY[@]}" render --json "$JSON" --root "$ROOT" "${1:---write}" ;;
  collect|checksums) exec "${PY[@]}" "$@" ;;
  render|archives) cmd="$1"; shift; exec "${PY[@]}" "$cmd" --root "$ROOT" "$@" ;;
  *) echo "usage: $0 [--write|--check|collect|render|checksums|archives] ..." >&2; exit 2 ;;
esac
