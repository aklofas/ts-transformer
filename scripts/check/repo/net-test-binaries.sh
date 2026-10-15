#!/usr/bin/env bash
# Every integration-test binary whose sources open a socket is named `net_*`
# (and only those are),
# so .config/nextest.toml's network group (one test at a time, tight
# timeouts) is `binary(/^net_/)` instead of a name list. A binary's sources
# are its root file, every `#[path = "…"]` file the root includes, and
# tests/<name>/ (or the directory of a tests/<x>/mod.rs root). Unit tests
# inside lib binaries are matched by the filter's test(...) half, not by this
# rail.
# Overridable: ROSTER_METADATA (see scripts/lib/roster.sh), NTB_NEXTEST.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NEXTEST="${NTB_NEXTEST:-$ROOT/.config/nextest.toml}"
SOCKET='TcpListener|TcpStream|UdpSocket|127\.0\.0\.1|Ipv[46]Addr::(LOCALHOST|UNSPECIFIED)'
fail=0; n=0
while IFS="$(printf '\t')" read -r pkg name src; do
  n=$((n + 1))
  stem="$(basename "$src" .rs)"
  if [ "$stem" = mod ]; then dir="$(dirname "$src")"; else dir="$(dirname "$src")/$stem"; fi
  files="$src"; [ -d "$dir" ] && files="$files $(find "$dir" -name '*.rs' | sort | tr '\n' ' ')"
  for p in $(sed -nE 's/^[[:space:]]*#\[path[[:space:]]*=[[:space:]]*"([^"]+)"\].*/\1/p' "$src"); do
    [ -f "$(dirname "$src")/$p" ] && files="$files $(dirname "$src")/$p"
  done
  # shellcheck disable=SC2086  # $files is a deliberate word-split list
  if grep -qE "$SOCKET" $files; then
    case "$name" in net_*) ;; *) echo "FAIL: $pkg test binary '$name' opens a socket; rename it net_$name"; fail=1 ;; esac
  else
    case "$name" in net_*) echo "FAIL: $pkg test binary '$name' is named net_* but opens no socket; drop the prefix"; fail=1 ;; esac
  fi
done < <(bash "$ROOT/scripts/lib/roster.sh" test-targets)
[ "$n" -gt 0 ] || { echo "FAIL: no test targets found"; exit 1; }
[ "$(grep -c 'binary(/^net_/)' "$NEXTEST")" -eq 3 ] || { echo "FAIL: $NEXTEST must carry binary(/^net_/) in all three network overrides"; fail=1; }
[ "$fail" -eq 0 ] && echo "net-test-binaries: OK ($n test binaries)"
exit "$fail"
