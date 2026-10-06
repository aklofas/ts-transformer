#!/usr/bin/env bash
# Publish (or dry-run) ONE dependency layer of the crates.io release train.
#
#   usage: publish-crates-layer.sh <pkg> [<pkg>...]
#   env:   PUBLISH=1   real publish (CARGO_REGISTRY_TOKEN must be set);
#                      anything else = `cargo publish --dry-run` per crate
#                      (packages + verify-builds, uploads nothing, no token)
#
# Each package's version comes from `cargo metadata` — per crate, NOT a
# single shared workspace version: tstrans-mbedtls-src carries a semver
# build-metadata suffix (e.g. 0.4.0+3.6.7), so a shared version string
# would miss it on the skip check below and a re-run would die trying to
# re-publish a live version.
#
# Skip-if-live: crates.io publishes are immutable, so a re-run after a
# mid-sequence failure must fast-forward through the already-live prefix
# instead of dying on "crate version already exists". 200 from the version
# API = live → skip; 404 = not yet published → publish; anything else is a
# hard error (never guess around a flaky index while holding a publish token).
#
# Dry-run at an UNPUBLISHED version (after a release version sweep, before
# the tag): the packaged crate's `version =` requirements cannot be met by
# the index, so the caller must point them at the tree first — crates-io.yml
# does that with a `[patch.crates-io]` table from dry-run-patch-config.sh;
# locally, `bash scripts/release/dry-run-patch-config.sh >> .cargo/config.toml`
# (delete it afterwards). The real tag publish never needs this: layers land
# in order and cargo (>=1.85) waits for index propagation of just-published
# deps.
set -euo pipefail

[[ $# -ge 1 ]] || { echo "usage: $0 <pkg> [<pkg>...]" >&2; exit 2; }

# crates.io asks API clients for an identifying User-Agent.
UA="ts-transformer-release (https://github.com/aklofas/ts-transformer)"

META=$(cargo metadata --format-version 1 --no-deps)

for pkg in "$@"; do
  ver=$(jq -r --arg p "$pkg" \
    '.packages[] | select(.name == $p) | .version' <<<"$META")
  [[ -n "$ver" ]] || { echo "package ${pkg} not found in workspace" >&2; exit 2; }

  if [[ "${PUBLISH:-0}" == "1" ]]; then
    # '+' is semver build metadata but reserved in URLs — encode it for
    # the API path (0.4.0+3.6.7 → 0.4.0%2B3.6.7).
    # Bounded + retried: an unbounded stall here would wedge the publish
    # train while holding a live token, and a transient blip shouldn't
    # abort a release. 200/404 are terminal (no retry); --retry only
    # covers transient failures (timeouts, 429/5xx).
    status=$(curl -sS -o /dev/null -w '%{http_code}' -A "$UA" \
      --connect-timeout 10 --max-time 30 --retry 3 --retry-connrefused \
      "https://crates.io/api/v1/crates/${pkg}/${ver//+/%2B}")
    case "$status" in
      200) echo "== ${pkg}@${ver} already live on crates.io — skipping"; continue ;;
      404) ;; # not yet published — proceed
      *)   echo "unexpected crates.io API status ${status} for ${pkg}@${ver}" >&2; exit 1 ;;
    esac
    echo "== publishing ${pkg}@${ver}"
    cargo publish -p "$pkg"
  else
    echo "== dry-run: ${pkg}@${ver}"
    cargo publish --dry-run -p "$pkg"
  fi
done
