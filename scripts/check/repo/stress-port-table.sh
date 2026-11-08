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
# Last, a step's launch loop: stress.sh's own run_step, over stubs, must
# probe every port of the step before it starts the first process. A probe
# costs a process start, and a receiver waits a fixed time for a sender
# that only starts after the whole loop.
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
# An exhausted list fails the rail instead of feeding reserve_port's
# re-probe loop an empty candidate forever.
free_port() {
  local i
  i=$(<"$cursor")
  if ((i >= ${#seq_ports[@]})); then
    echo "FAIL: the stub free_port ran out after $i ports: reserve_port kept re-probing" >&2
    exit 1
  fi
  echo $((i + 1)) >"$cursor"
  printf '%s\n' "${seq_ports[$i]}"
}
# A failing `$(free_port)` inside reserve_port must end the rail, not
# return an empty candidate, so errexit has to reach it.
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
  # A UDP-transport leg has a proxy port; a TCP leg has none.
  if [[ "$t" == tcp ]]; then proxy='-'; else proxy='[0-9]+'; fi
  for ((i = 0; i < n; i++)); do
    grep -qE " $t-$i recv=[0-9]+ proxy=$proxy\$" <<<"$rows" \
      || { echo "FAIL: dry-run port table for $t/$axis/$load lists no $t-$i with recv= and proxy=$proxy"; exit 1; }
  done
  dup=$(awk '{ for (f = 1; f <= NF; f++) if ($f ~ /^(recv|proxy)=[0-9]+$/) { sub(/^[a-z]+=/, "", $f); print $f } }' <<<"$rows" | sort | uniq -d)
  [[ -z "$dup" ]] || { echo "FAIL: dry-run step $t/$axis/$load reserves port(s) $dup twice"; exit 1; }
done < <(grep -E '^(srt|tcp) (streams|bitrate) [0-9]+ ' <<<"$out")
[[ $steps -eq 6 ]] || { echo "FAIL: expected 6 dry-run steps, saw $steps"; exit 1; }

# A real run refuses an inherited dry-run counter. The outdir already holds
# a stress-config.json, so if the refusal ever goes missing the run stops at
# the reused-outdir check (a different message) and launches nothing.
real_outdir=$(mktemp -d)
trap 'rm -rf "$cursor" "$dry_outdir" "$real_outdir"' EXIT
echo '{}' >"$real_outdir/stress-config.json"
rc=0
out=$(DRY_RUN_PORT_NEXT=1 SRT_RECONNECT_MODE=background RSS_SLOPE_THRESHOLD_KB_PER_HOUR=8000 \
  bash "$ROOT/scripts/interop/stress.sh" --outdir "$real_outdir" --seed 11 \
  --transports srt,tcp --stream-ladder 1 --scale-ladder 2 --skip-hold 2>&1) || rc=$?
[[ $rc -eq 2 && "$out" == *"DRY_RUN_PORT_NEXT is a --dry-run counter; refusing it on a real run"* ]] \
  || { echo "FAIL: a real run with DRY_RUN_PORT_NEXT set exited $rc: $out"; exit 1; }
# run_step and stream_port are stress.sh's own text; everything they call
# is a stub. A port probe and a process start each log one line, and the
# step ends at its LAUNCH-SPAN event, before anything is supervised.
launch_log=$(mktemp)
trap 'rm -rf "$cursor" "$dry_outdir" "$real_outdir" "$launch_log"' EXIT
(
  OUTDIR=$dry_outdir STEP_WARMUP_S=1 STEP_HOLD_S=1 SETTLE=0 STRESS_MAX_AGG_MBPS=1
  declare -A PIDS=() STREAM_PORT=() PROXY_PORT=() USED_PORTS=()
  eval "$(sed -n -e '/^stream_port() {$/,/^}$/p' -e '/^run_step() {$/,/^}$/p' "$ROOT/scripts/interop/stress.sh")"
  step_streams() { echo 3; }
  step_scale() { echo 1; }
  per_stream_mbps() { echo 1; }
  agg_mbps() { echo 1; }
  over_cap() { return 1; }
  write_step_declaration() { :; }
  sleep() { :; }
  event() { [[ "$1" != LAUNCH-SPAN* ]] || exit 0; }
  reserve_port() { echo probe >>"$launch_log"; printf -v "$2" '%s' 1; }
  # A listening stream asks for its ports, as the real launch does.
  launch_stream() {
    echo "launch $1 $4" >>"$launch_log"
    [[ "$1" != listen ]] || stream_port "$2" "$4"
  }
  mkdir -p "$OUTDIR/sweep/srt/streams/3"
  run_step srt streams 3 2>/dev/null
  echo "FAIL: run_step ran past its LAUNCH-SPAN event"
  exit 1
) || exit 1
[[ $(grep -c '^probe$' "$launch_log") -eq 6 && $(grep -c '^launch ' "$launch_log") -eq 6 ]] \
  || { echo "FAIL: a 3-stream srt step logged other than 6 probes and 6 launches:"; cat "$launch_log"; exit 1; }
late=$(awk '/^launch /{launched = 1} /^probe$/ && launched {n++} END {print n + 0}' "$launch_log")
[[ $late -eq 0 ]] \
  || { echo "FAIL: run_step probed $late port(s) after starting its first process:"; cat "$launch_log"; exit 1; }
echo "PASS: stress-port-table"
