#!/usr/bin/env bash
# The generated tables in the docs match ci.yml, sanitizers.yml,
# tests/coverage/fuzz-targets.toml, the crates' Cargo features and
# tests/coverage/*.toml. Regenerate with: bash scripts/gen/doc-tables.sh
# Needs cargo and python3 (>= 3.11); gated to linux-x86_64 in CI.
set -euo pipefail
exec bash "$(cd "$(dirname "$0")/../../.." && pwd)/scripts/gen/doc-tables.sh" --check
