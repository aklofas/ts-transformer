#!/usr/bin/env bash
# Write (default) or --check the generated tables in the docs: the build
# targets, sanitizer jobs and fuzz count in docs/reference/compatibility.md,
# the feature table in docs/languages/rust.md and the manifest list in
# tests/coverage/README.md. See scripts/gen/doc_tables.py.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# A cold box must not try pkg-config for libsrt/librist while loading metadata.
export SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1
case "${1:---write}" in
  --write|--check) exec python3 -I "$ROOT/scripts/gen/doc_tables.py" --root "$ROOT" "${1:---write}" ;;
  *) echo "usage: $0 [--write|--check]" >&2; exit 2 ;;
esac
