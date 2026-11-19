#!/usr/bin/env bash
# Usage: cargo-deny-all.sh          cargo-deny `check advisories licenses` on every workspace
#        cargo-deny-all.sh --list   print the workspaces' manifests, one per line
# Every workspace of the tree (root, fuzz, embedded) is checked against
# deny.toml at its committed lockfile (`--locked`). With the `rev` of
# tests/coverage/advisory-db-pin.toml set (a release freeze), the advisories
# come from that RustSec advisory-db commit with fetching disabled, so an
# advisory published during the freeze does not change the verdict on the
# frozen commit; with `rev = ""` from the live database. Every workspace runs;
# the script fails when any failed and names each one.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORKSPACES=(
  Cargo.toml
  crates/tst-core/fuzz/Cargo.toml
  crates/tst-rtp/fuzz/Cargo.toml
  crates/tst-srt/fuzz/Cargo.toml
  crates/tst-webrtc/fuzz/Cargo.toml
  embedded/baremetal-qemu/Cargo.toml
  embedded/baremetal-qemu-c/Cargo.toml
  embedded/freertos-srt/example/host/Cargo.toml
)

if [ "${1:-}" = --list ]; then
  printf '%s\n' "${WORKSPACES[@]}"
  exit 0
fi
[ $# -eq 0 ] || { echo "usage: $0 [--list]" >&2; exit 2; }

PIN="${ADVISORY_DB_PIN:-$ROOT/tests/coverage/advisory-db-pin.toml}"
grep -qE '^rev[[:space:]]*=[[:space:]]*"[^"]*"' "$PIN" \
  || { echo "cargo-deny-all: $PIN has no rev = \"...\" line" >&2; exit 2; }
rev="$(sed -nE 's/^rev[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$PIN" | head -1)"
if [ -n "$rev" ] && ! printf '%s' "$rev" | grep -qE '^[0-9a-f]{40}$'; then
  echo "cargo-deny-all: the advisory-db pin '$rev' is not 40 hex" >&2
  exit 2
fi

offline=()
if [ -n "$rev" ]; then
  # cargo-deny names its clone after a hash of the URL, so it makes the clone
  # itself and the clone is then moved to the pin (as its own fetch does).
  grep -qxF 'db-path = "$CARGO_HOME/advisory-db"' "$ROOT/deny.toml" \
    || { echo "cargo-deny-all: deny.toml's db-path is not \$CARGO_HOME/advisory-db" >&2; exit 2; }
  cargo deny --manifest-path "$ROOT/Cargo.toml" --config "$ROOT/deny.toml" fetch db
  dbs=("${CARGO_HOME:-$HOME/.cargo}"/advisory-db/advisory-db-*)
  [ ${#dbs[@]} -eq 1 ] && [ -d "${dbs[0]}/.git" ] \
    || { echo "cargo-deny-all: want one advisory-db clone, found: ${dbs[*]}" >&2; exit 2; }
  db="${dbs[0]}"
  git -C "$db" fetch -q --depth=1 origin "$rev"
  git -C "$db" reset -q --hard FETCH_HEAD
  head="$(git -C "$db" rev-parse HEAD)"
  [ "$head" = "$rev" ] || { echo "cargo-deny-all: the advisory-db clone is at $head, not $rev" >&2; exit 2; }
  echo "cargo-deny-all: advisory-db at $rev ($(git -C "$db" show -s --format=%cI HEAD)), fetching disabled"
  offline=(--offline)
else
  echo "cargo-deny-all: the advisory-db pin is empty; checking against the live database"
fi

failed=()
for m in "${WORKSPACES[@]}"; do
  echo "== cargo-deny: $m"
  # --offline also keeps cargo metadata off the network: fetch the crates first.
  if [ -n "$rev" ] && ! cargo fetch --locked --manifest-path "$ROOT/$m"; then
    failed+=("$m"); continue
  fi
  cargo deny ${offline[@]+"${offline[@]}"} --manifest-path "$ROOT/$m" --config "$ROOT/deny.toml" --locked \
    check advisories licenses || failed+=("$m")
done

for m in ${failed[@]+"${failed[@]}"}; do echo "cargo-deny-all: FAILED: $m"; done
[ ${#failed[@]} -eq 0 ]
