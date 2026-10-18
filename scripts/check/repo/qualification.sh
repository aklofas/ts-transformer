#!/usr/bin/env bash
# The evidence page's qualification block matches docs/project/qualification.json
# (or says none is recorded while no JSON is committed), and
# tests/coverage/evidence-archives.toml is well formed. Regenerate with:
# bash scripts/gen/qualification.sh --write
set -euo pipefail
G="$(cd "$(dirname "$0")/../../.." && pwd)/scripts/gen/qualification.sh"
bash "$G" archives
bash "$G" --check
