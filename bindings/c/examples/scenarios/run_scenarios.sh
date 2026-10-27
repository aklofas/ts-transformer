#!/usr/bin/env bash
# run_scenarios.sh — Build and run the C cross-binding scenario adapter.
#
# Usage:
#   bash bindings/c/examples/scenarios/run_scenarios.sh
#   bash bindings/c/examples/scenarios/run_scenarios.sh [/path/to/scenarios]
#
# WORKSPACE_ROOT is derived from this script's location, so it can be run
# from any directory. To point it at another checkout, set WORKSPACE_ROOT:
#   WORKSPACE_ROOT=/path/to/ts-transformer bash bindings/c/examples/scenarios/run_scenarios.sh
#
# Env vars honoured:
#   SRT_FORCE_VENDORED  — passed to the package build (default "1")
#   RIST_FORCE_VENDORED — passed to the package build (default "1")
#
# Exit code: 0 = all scenarios matched their goldens; non-zero = failure.

set -euo pipefail

# ── Locate workspace root ────────────────────────────────────────────────────
# The script may be invoked from any directory; we derive WORKSPACE_ROOT from
# the script's own location (SCRIPT_DIR/../../../../ for
# bindings/c/examples/scenarios/).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="${WORKSPACE_ROOT:-$(cd "$SCRIPT_DIR/../../../.." && pwd)}"

echo "workspace root: $WORKSPACE_ROOT"

# ── Stage one all-features package ───────────────────────────────────────────
# --all-features, not the default build: the `cancelled-recv-kind` and
# `peer-reset-recv-kind` scenarios call the srt- and tcp-gated receiver
# entry points, and tst-c's transport features are default-OFF. The header, tstrans_config.h and library must come
# from the same build: the committed bindings/c/include/tstrans_config.h
# defines no TST_HAS_*, so compiling against it hides those declarations.
# c-package.sh builds and stages all three, as the ci.yml "C scenario adapter"
# step does. It leaves the output read-only, hence the chmod before removal.
PKG_PARENT="$(mktemp -d)"
trap 'chmod -R u+w "$PKG_PARENT"; rm -rf "$PKG_PARENT"' EXIT
PKG="$PKG_PARENT/pkg"

echo "staging libtstrans package..."
SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}" \
RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}" \
  bash "$WORKSPACE_ROOT/scripts/dev/c-package.sh" --all-features --out "$PKG"

SOURCE="$SCRIPT_DIR/run_scenarios.c"
BINARY="$PKG_PARENT/run_scenarios_c_adapter"

# ── Compile the C adapter ────────────────────────────────────────────────────
# The rpath lets the adapter find the staged library without LD_LIBRARY_PATH.
echo "compiling $SOURCE..."
gcc \
  -I "$PKG/include" \
  -L "$PKG/lib" -Wl,-rpath,"$PKG/lib" \
  -Wall -Werror \
  -o "$BINARY" \
  "$SOURCE" \
  -ltstrans

echo "compiled: $BINARY"

# ── Determine the scenarios directory ────────────────────────────────────────
# Allow override via argv or a well-known relative path from the workspace root.
if [ -n "${1:-}" ]; then
    SCENARIOS_DIR="$1"
else
    SCENARIOS_DIR="$WORKSPACE_ROOT/crates/tst-integration/tests/fixtures/scenarios"
fi

echo "scenarios dir: $SCENARIOS_DIR"

# ── Run the adapter ──────────────────────────────────────────────────────────
echo ""
ADAPTER_EXIT=0
timeout 60 "$BINARY" "$SCENARIOS_DIR" || ADAPTER_EXIT=$?
echo "ADAPTER_EXIT=$ADAPTER_EXIT"
exit "$ADAPTER_EXIT"
