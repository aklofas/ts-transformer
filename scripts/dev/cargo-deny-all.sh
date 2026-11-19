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
#
# Pinned mode: cargo-deny 0.20.2's only switch that stops the database fetch is
# the global `--offline`, and it also runs `cargo metadata` offline, so each
# workspace's crates are fetched first (`cargo fetch --locked`). Every network
# step (in live mode the first fetch of the database) is bounded at 120 s (GNU
# timeout, when present) and tried three times, 20 s and 60 s apart: at most
# 3 x 130 + 80 = 470 s per step. The offline checks then run behind a proxy on a
# closed port: a check that tried to fetch anything would fail instead of
# reading the live database.
# There is no fixture-database test: cargo-deny 0.20.2 refuses a database URL
# without a domain name, file:// included.
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

bound=()
if timeout --version 2>/dev/null | grep -q GNU; then bound=(timeout -k 10 120); fi
# retry <command...>: three bounded attempts; before_retry, when set, runs
# between them.
before_retry=()
retry() {
  local attempt=1 w
  until ${bound[@]+"${bound[@]}"} "$@"; do
    echo "::notice::attempt $attempt of 3 failed: $*"
    [ "$attempt" -lt 3 ] || return 1
    ${before_retry[@]+"${before_retry[@]}"}
    w="$(echo "20 60" | awk -v i="$attempt" '{print $i}')"
    sleep "$w"
    attempt=$((attempt + 1))
  done
}

# deny.toml's db-path, where cargo-deny keeps its clones.
grep -qxF 'db-path = "$CARGO_HOME/advisory-db"' "$ROOT/deny.toml" \
  || { echo "cargo-deny-all: deny.toml's db-path is not \$CARGO_HOME/advisory-db" >&2; exit 2; }
DB_ROOT="${CARGO_HOME:-$HOME/.cargo}/advisory-db"
drop_new_clone() { rm -rf "$DB_ROOT"/advisory-db-*; }

fetch_db() {
  # An attempt cut off by the bound leaves a half-written clone that cargo-deny
  # would fetch into on the retry. A clone this run started is removed before
  # each retry; one that was under db-path before the run is never touched.
  before_retry=()
  compgen -G "$DB_ROOT/advisory-db-*" >/dev/null || before_retry=(drop_new_clone)
  retry cargo deny --manifest-path "$ROOT/Cargo.toml" --config "$ROOT/deny.toml" fetch db \
    || { echo "cargo-deny-all: could not fetch the advisory-db" >&2; exit 1; }
  before_retry=()
}

deny=()
if [ -n "$rev" ]; then
  # cargo-deny names its clone after a hash of the URL, so it makes the clone
  # itself and the clone is then moved to the pin (as its own fetch does).
  fetch_db
  dbs=("$DB_ROOT"/advisory-db-*)
  [ ${#dbs[@]} -eq 1 ] && [ -d "${dbs[0]}/.git" ] \
    || { echo "cargo-deny-all: want one advisory-db clone, found: ${dbs[*]}" >&2; exit 2; }
  db="${dbs[0]}"
  retry git -C "$db" fetch -q --depth=1 origin "$rev"
  git -C "$db" reset -q --hard FETCH_HEAD
  head="$(git -C "$db" rev-parse HEAD)"
  [ "$head" = "$rev" ] || { echo "cargo-deny-all: the advisory-db clone is at $head, not $rev" >&2; exit 2; }
  echo "cargo-deny-all: advisory-db at $rev ($(git -C "$db" show -s --format=%cI HEAD)), fetching disabled"
  dead=http://127.0.0.1:9
  deny=(env "https_proxy=$dead" "HTTPS_PROXY=$dead" "http_proxy=$dead" "HTTP_PROXY=$dead"
           "ALL_PROXY=$dead" "all_proxy=$dead" no_proxy= NO_PROXY= cargo deny --offline)
else
  deny=(cargo deny)
  echo "cargo-deny-all: the advisory-db pin is empty; checking against the live database"
  # The clone, retried here; each check then only updates it.
  fetch_db
fi

failed=()
for m in "${WORKSPACES[@]}"; do
  echo "== cargo-deny: $m"
  if [ -n "$rev" ] && ! retry cargo fetch --locked --manifest-path "$ROOT/$m"; then
    failed+=("$m"); continue
  fi
  "${deny[@]}" --manifest-path "$ROOT/$m" --config "$ROOT/deny.toml" --locked \
    check advisories licenses || failed+=("$m")
done

for m in ${failed[@]+"${failed[@]}"}; do echo "cargo-deny-all: FAILED: $m"; done
[ ${#failed[@]} -eq 0 ]
