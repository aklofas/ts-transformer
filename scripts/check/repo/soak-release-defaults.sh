#!/usr/bin/env bash
# soak-release-defaults: the soak launcher's defaults are the release soak.
# The run length in soak.sh and release-vm-soak.sh must equal
# `[soak] required_duration_s` in tests/coverage/evidence-archives.toml, and
# the publisher-generation and outage periods must tile that duration so the
# run sees whole cycles (and at least two publisher generations).
# The documented `soak.sh --hours` recipes in the interop README and the
# release runbook must give at least two publisher generations and two browser
# connections, the launcher's own launch checks.
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
# Both `preflight` and `launch` carry their own `hours=N` local default.
vm_hours=$(sed -n 's/^  local .*[ "]hours=\([0-9][0-9.]*\) .*/\1/p' "$VM")
vm_lines=$(grep -n -E '^  local .*[ "]hours=[0-9]' "$VM" | cut -d: -f1 | tr '\n' ' ')
vm_count=$(printf '%s\n' "$vm_hours" | grep -c . || true)
((vm_count == 2)) || fail "release-vm-soak.sh: expected 2 local hours= defaults (preflight, launch), found $vm_count"

for pair in "soak.sh HOURS:$soak_hours" "release-vm-soak.sh hours:${vm_hours%%$'\n'*}" \
  "PUBLISHER_DROP_PERIOD_S:$drop" "OUTAGE_PERIOD_S:$outage" "OUTAGE_DUR_S:$outage_dur"; do
  [[ -n "${pair#*:}" ]] || fail "could not read ${pair%%:*} default"
done
((fails == 0)) || exit 1

hours_check() {
  local label=$1 h=$2 secs
  secs=$(awk -v h="$h" 'BEGIN { printf "%d", h * 3600 }')
  ((secs == required)) || fail "$label -> $secs, table requires $required"
}
hours_check "soak.sh HOURS=$soak_hours" "$soak_hours"
read -r -a vm_line_nos <<<"$vm_lines"
i=0
while read -r h; do
  [[ -n "$h" ]] && hours_check "release-vm-soak.sh:${vm_line_nos[$i]:-?} hours=$h" "$h"
  i=$((i + 1))
done <<<"$vm_hours"

((required % drop == 0)) || fail "required $required not a multiple of PUBLISHER_DROP_PERIOD_S $drop"
((required / drop >= 2)) || fail "PUBLISHER_DROP_PERIOD_S $drop gives fewer than two generations in $required"
((required % outage == 0)) || fail "required $required not a multiple of OUTAGE_PERIOD_S $outage"
((outage_dur < outage)) || fail "OUTAGE_DUR_S $outage_dur not below OUTAGE_PERIOD_S $outage"

# The documented soak.sh recipes must clear the launcher's own checks: at
# least two publisher generations while a publish leg is on and at least two
# browser connections while the whep leg is on. A recipe that does not exits
# 2 before any browser starts.
whep=$(sed -n 's/^WHEP_RECONNECT_S="\${WHEP_RECONNECT_S:-\([0-9]*\)}".*/\1/p' "$SOAK" | head -n1)
[[ -n "$whep" ]] || fail "could not read WHEP_RECONNECT_S default"
recipes=$(python3 - "$drop" "$whep" scripts/interop/README.md docs/project/releasing.md <<'PY'
import re, sys
drop_default, whep_default = int(sys.argv[1]), int(sys.argv[2])
found = 0
for path in sys.argv[3:]:
    text = open(path).read().replace("\\\n", " ")
    for line in text.splitlines():
        m = re.search(r"--hours\s+([0-9.]+)", line)
        if "soak.sh" not in line or not m:
            continue
        found += 1
        total = int(float(m.group(1)) * 3600 + 0.5)
        env = dict(re.findall(r"\b(PUBLISHER_DROP_PERIOD_S|WHEP_RECONNECT_S)=(\d+)", line))
        drop = int(env.get("PUBLISHER_DROP_PERIOD_S", drop_default))
        whep = int(env.get("WHEP_RECONNECT_S", whep_default))
        both_off = re.search(r"--no-rtsp-publish(\s|$)", line) and "--no-rtsp-publish-es" in line
        if not both_off and total // drop < 2:
            print("FAIL: %s: --hours %s with PUBLISHER_DROP_PERIOD_S=%d gives %d publisher "
                  "generation(s), below two" % (path, m.group(1), drop, total // drop))
        if "--no-whep" not in line and total // whep < 2:
            print("FAIL: %s: --hours %s with WHEP_RECONNECT_S=%d gives %d browser connection(s), "
                  "below two" % (path, m.group(1), whep, total // whep))
if found == 0:
    print("FAIL: no soak.sh --hours recipe in " + ", ".join(sys.argv[3:]))
PY
)
if [[ -n "$recipes" ]]; then
  echo "$recipes"
  fails=$((fails + 1))
fi

((fails == 0)) || exit 1
echo "soak-release-defaults: ok"
