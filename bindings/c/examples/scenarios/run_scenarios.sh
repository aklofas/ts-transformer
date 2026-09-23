#!/usr/bin/env bash
# run_scenarios.sh — Build and run the C cross-binding scenario adapter.
#
# Usage:
#   bash bindings/c/examples/scenarios/run_scenarios.sh
#   bash bindings/c/examples/scenarios/run_scenarios.sh [/path/to/scenarios]
#
# Must be run from the workspace root (the directory containing Cargo.toml).
# If run from elsewhere, set WORKSPACE_ROOT before calling:
#   WORKSPACE_ROOT=/path/to/ts-transformer bash bindings/c/examples/scenarios/run_scenarios.sh
#
# Env vars honoured:
#   SRT_FORCE_VENDORED  — passed to cargo build (default "1")
#   RIST_FORCE_VENDORED — passed to cargo build (default "1")
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

# ── Build libtstrans ─────────────────────────────────────────────────────────
# --all-features, not the default build: the `cancelled-recv-kind` scenario
# calls the srt-gated receiver/raw-sender entry points, and tst-c's transport
# features are default-OFF, so a bare build would leave those symbols out of
# the cdylib and the link below would fail. CI links the all-features cdylib
# its earlier build step produced (ci.yml "C scenario adapter"), so this keeps
# the local run and CI on the same surface.
echo "building libtstrans..."
SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}" \
RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}" \
  cargo build -p tst-c --all-features --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

LIB_DIR="$WORKSPACE_ROOT/target/debug"
INCLUDE_DIR="$WORKSPACE_ROOT/bindings/c/include"
SOURCE="$SCRIPT_DIR/run_scenarios.c"
BINARY="/tmp/run_scenarios_c_adapter"

# ── Compile the C adapter ────────────────────────────────────────────────────
echo "compiling $SOURCE..."
gcc \
  -I "$INCLUDE_DIR" \
  -L "$LIB_DIR" \
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
LD_LIBRARY_PATH="$LIB_DIR" "$BINARY" "$SCENARIOS_DIR"
ADAPTER_EXIT=$?
echo "ADAPTER_EXIT=$ADAPTER_EXIT"
exit $ADAPTER_EXIT
