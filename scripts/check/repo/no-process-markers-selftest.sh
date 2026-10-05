#!/usr/bin/env bash
# Adversarial self-test of no-process-markers.sh: plants every pattern and a
# set of domain-sense phrases, asserts detection and non-detection. Lives
# under scripts/check/ so the local pre-push sweep runs it; ci.yml runs it as
# its own step next to the rail.
set -euo pipefail
exec bash "$(dirname "$0")/no-process-markers.sh" --selftest
