# shellcheck shell=bash
# Sourced by soak.sh and scripts/check/repo/soak-rss-default.sh.

# rss_slope_default_kb_per_hour HOURS: print the default RSS-slope threshold
# (KiB/h) for a run of HOURS (decimals allowed), or nothing when the run is
# shorter than 24 h and its gate stays provisional.
rss_slope_default_kb_per_hour() {
  if awk -v h="$1" 'BEGIN{exit !(h>=24)}'; then
    echo 200
  fi
}
