#!/usr/bin/env bash
# stress-port-table: reserve_port (scripts/interop/lib.sh) must never hand
# out a port an earlier reservation of the same step holds. A stub
# free_port returns the same port twice, the way the kernel can before
# either socket has bound; the second reservation must re-probe past it.
# This is the property a `$(reserve_port …)` call silently loses, since
# the subshell's write to USED_PORTS dies with the subshell.
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
trap 'rm -f "$cursor"' EXIT
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
echo "PASS: stress-port-table"
