#!/usr/bin/env bash
# Run the fail-closed ratchets' negative-case self-test as a gate (the cases
# in scripts/ratchets/tests/self_test.sh).
# Lives under scripts/check/ so the local pre-push loop picks it up; also wired
# as an explicit CI step (CI does not glob).
set -euo pipefail
exec "$(dirname "$0")/../../ratchets/tests/self_test.sh"
