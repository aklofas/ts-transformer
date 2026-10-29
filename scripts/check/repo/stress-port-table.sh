#!/usr/bin/env bash
# stress-port-table: reserve_port (scripts/interop/lib.sh) must never hand
# out a port an earlier reservation of the same step holds. A stub
# free_port returns the same port twice, the way the kernel can before
# either socket has bound; the second reservation must re-probe past it.
# This is the property a `$(reserve_port …)` call silently loses, since
# the subshell's write to USED_PORTS dies with the subshell.
#
# Then stress.sh's own --dry-run on a small ladder: its per-step port table
# comes from the real stream_port calls (on reserve_port's counter, so
# nothing binds) and must list every leg of every step with no port twice
# within a step. Ports repeat ACROSS steps by design (each step resets its
# table), so only a within-step repeat is a fault.
#
# Run it:   bash scripts/check/repo/stress-port-table.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
(( BASH_VERSINFO[0] >= 4 )) || { echo "SKIP: stress-port-table needs bash >= 4"; exit 0; }
# shellcheck source=../../interop/lib.sh
source "$ROOT/scripts/interop/lib.sh"
declare -A USED_PORTS=() STREAM_PORT=() PROXY_PORT=()
seq_ports=(40000 40000 40001 40001 40002)
# The stub runs inside reserve_port's own `$(free_port …)`, so it cannot
# consume its list in place; a file holds the cursor instead.
cursor=$(mktemp)
dry_outdir=$(mktemp -d)
trap 'rm -rf "$cursor" "$dry_outdir"' EXIT
echo 0 >"$cursor"
free_port() {
  local i
  i=$(<"$cursor")
  echo $((i + 1)) >"$cursor"
  printf '%s\n' "${seq_ports[$i]}"
}
reserve_port udp 'STREAM_PORT[a]'
reserve_port udp 'STREAM_PORT[b]'
reserve_port udp 'PROXY_PORT[b]'
[[ "${STREAM_PORT[a]}" == 40000 && "${STREAM_PORT[b]}" == 40001 && "${PROXY_PORT[b]}" == 40002 ]] \
  || { echo "FAIL: reserve_port handed out ${STREAM_PORT[a]}, ${STREAM_PORT[b]} and ${PROXY_PORT[b]}"; exit 1; }
[[ ${#USED_PORTS[@]} -eq 3 ]] \
  || { echo "FAIL: USED_PORTS holds ${#USED_PORTS[@]} port(s), not the 3 reserved"; exit 1; }

out=$(SRT_RECONNECT_MODE=background RSS_SLOPE_THRESHOLD_KB_PER_HOUR=8000 \
  bash "$ROOT/scripts/interop/stress.sh" --outdir "$dry_outdir" --dry-run --seed 11 \
  --transports srt,tcp --stream-ladder 1,3 --scale-ladder 2 --skip-hold 2>&1) \
  || { echo "FAIL: stress.sh --dry-run exited non-zero:"; printf '%s\n' "$out"; exit 1; }
steps=0
while read -r t axis load _; do
  steps=$((steps + 1))
  if [[ "$axis" == streams ]]; then n=$load; else n=1; fi
  rows=$(grep -E "^ports: $t $axis $load " <<<"$out" || true)
  for ((i = 0; i < n; i++)); do
    grep -q " $t-$i recv=[0-9]" <<<"$rows" \
      || { echo "FAIL: dry-run port table for $t/$axis/$load lists no $t-$i"; exit 1; }
  done
  dup=$(sed -E 's/.* recv=([0-9]+) proxy=([0-9-]+)$/\1\n\2/' <<<"$rows" | grep -v '^-$' | sort | uniq -d)
  [[ -z "$dup" ]] || { echo "FAIL: dry-run step $t/$axis/$load reserves port(s) $dup twice"; exit 1; }
done < <(grep -E '^(srt|tcp) (streams|bitrate) [0-9]+ ' <<<"$out")
[[ $steps -eq 6 ]] || { echo "FAIL: expected 6 dry-run steps, saw $steps"; exit 1; }
echo "PASS: stress-port-table"
