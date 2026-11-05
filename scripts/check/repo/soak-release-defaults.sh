#!/usr/bin/env bash
# soak-release-defaults: the soak launcher's defaults are the release soak.
# The run length in soak.sh and release-vm-soak.sh must equal
# `[soak] required_duration_s` in tests/coverage/evidence-archives.toml, and
# the publisher-generation and outage periods must tile that duration so the
# run sees whole cycles (and at least two publisher generations).
#
# Run it:   bash scripts/check/repo/soak-release-defaults.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

SOAK=scripts/interop/soak.sh
VM=scripts/interop/release-vm-soak.sh
TOML=tests/coverage/evidence-archives.toml

fails=0
fail() {
  echo "FAIL: $*"
  fails=$((fails + 1))
}

required=$(python3 -c 'import tomllib,sys; print(tomllib.load(open(sys.argv[1],"rb"))["soak"]["required_duration_s"])' "$TOML")

soak_hours=$(sed -n 's/^HOURS=\([0-9][0-9.]*\)$/\1/p' "$SOAK" | head -n1)
drop=$(sed -n 's/^PUBLISHER_DROP_PERIOD_S="\${PUBLISHER_DROP_PERIOD_S:-\([0-9]*\)}".*/\1/p' "$SOAK" | head -n1)
outage=$(sed -n 's/^OUTAGE_PERIOD_S="\${OUTAGE_PERIOD_S:-\([0-9]*\)}".*/\1/p' "$SOAK" | head -n1)
outage_dur=$(sed -n 's/^OUTAGE_DUR_S="\${OUTAGE_DUR_S:-\([0-9]*\)}".*/\1/p' "$SOAK" | head -n1)
vm_hours=$(sed -n 's/.*local outdir="" hours=\([0-9][0-9.]*\) .*/\1/p' "$VM" | head -n1)

for pair in "soak.sh HOURS:$soak_hours" "release-vm-soak.sh hours:$vm_hours" \
  "PUBLISHER_DROP_PERIOD_S:$drop" "OUTAGE_PERIOD_S:$outage" "OUTAGE_DUR_S:$outage_dur"; do
  [[ -n "${pair#*:}" ]] || fail "could not read ${pair%%:*} default"
done
((fails == 0)) || exit 1

for pair in "soak.sh HOURS=$soak_hours" "release-vm-soak.sh hours=$vm_hours"; do
  h=${pair##*=}
  secs=$(awk -v h="$h" 'BEGIN { printf "%d", h * 3600 }')
  ((secs == required)) || fail "$pair -> $secs, table requires $required"
done

((required % drop == 0)) || fail "required $required not a multiple of PUBLISHER_DROP_PERIOD_S $drop"
((required / drop >= 2)) || fail "PUBLISHER_DROP_PERIOD_S $drop gives fewer than two generations in $required"
((required % outage == 0)) || fail "required $required not a multiple of OUTAGE_PERIOD_S $outage"
((outage_dur < outage)) || fail "OUTAGE_DUR_S $outage_dur not below OUTAGE_PERIOD_S $outage"

((fails == 0)) || exit 1
echo "soak-release-defaults: ok"
