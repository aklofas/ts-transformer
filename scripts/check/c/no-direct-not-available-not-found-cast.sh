#!/usr/bin/env bash
# Verify that no C ABI source file in bindings/c/core/src/ contains a direct
# `TstError::NotAvailable as i32` or `TstError::NotFound as i32` cast.
# These bypass set_last_error() and leave stale message visible to
# tst_get_last_error(). The canonical pattern is to call
# record_not_available(msg) / record_not_found(msg) from crate::error.
#
# A regression here means an old pattern slipped back in — either a
# revert, a copy-paste from old code, or a new accessor that used the
# wrong shape. The TST_E_* code stays correct, but the last-error
# message is stale.
#
# Exclusions:
# - `assert_eq` in test modules (documented enum-value assertions).
# - bindings/c/core/src/error.rs (the helpers themselves contain the cast
#   on their happy path — they pair it with set_last_error in the same
#   call, which is the whole point).

set -euo pipefail

cd "$(dirname "$0")/../../.."

# Fail closed if ripgrep is absent. The previous `rg ... | grep ... || true`
# shape masked rg's own failure: with pipefail the pipeline status is the
# rightmost grep (exit 1, "no match"), and `|| true` swallowed even that — so
# on a runner without `rg` the guard printed OK without scanning anything.
if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) is required by $(basename "$0") but is not on PATH." >&2
    echo "  Install it (apt install ripgrep / brew install ripgrep / choco install ripgrep)." >&2
    exit 1
fi

# Drive pass/fail off rg's own exit code: 0 = matches, 1 = no matches (healthy),
# >=2 = a real rg error that must red the build rather than be filtered away.
set +e
matches=$(rg -n "TstError::(NotAvailable|NotFound) as i32" bindings/c/core/src/)
rg_rc=$?
set -e
if [ "$rg_rc" -ge 2 ]; then
    echo "FAIL: ripgrep errored (exit $rg_rc) scanning bindings/c/core/src/." >&2
    exit 1
fi

BYPASS=$(printf '%s' "$matches" \
    | grep -v "assert_eq" \
    | grep -v "^bindings/c/core/src/error.rs:" \
    || true)

if [ -n "$BYPASS" ]; then
    echo "FAIL: direct TstError::(NotAvailable|NotFound) as i32 cast(s) found in bindings/c/core/src/:"
    echo "$BYPASS" | sed 's/^/  /'
    echo
    echo "Use record_not_available(msg) or record_not_found(msg) from"
    echo "crate::error instead — those set last-error state before returning."
    exit 1
fi

echo "OK: no direct NotAvailable/NotFound casts in bindings/c/core/src/"
