#!/usr/bin/env bash
# soak-rss-default: the soak launcher's RSS-slope gate defaults on (200 KiB/h)
# for every run of 24 hours or more and stays off (provisional) below that.
# rss_slope_default_kb_per_hour lives in scripts/interop/lib/rss-slope.sh so
# this rail can test the rule without launching a soak.
#
# Run it:   bash scripts/check/repo/soak-rss-default.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
# shellcheck source=../../interop/lib/rss-slope.sh
source "$ROOT/scripts/interop/lib/rss-slope.sh"

fails=0
check() {
  local hours=$1 want=$2 got
  got=$(rss_slope_default_kb_per_hour "$hours")
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: rss_slope_default_kb_per_hour $hours -> '$got' (want '$want')"
    fails=$((fails + 1))
  fi
}

check 72 200
check 48 200
check 24 200
check 23.9 ""
check 1 ""

grep -q -- '--no-rss-slope-gate' scripts/interop/soak.sh || {
  echo "FAIL: soak.sh does not offer --no-rss-slope-gate"
  fails=$((fails + 1))
}

((fails == 0)) || exit 1
echo "soak-rss-default: ok"
