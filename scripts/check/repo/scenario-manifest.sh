#!/usr/bin/env bash
# Ratchet: validate crates/tst-integration scenarios.toml integrity.
#
# Enforced (fails the build on violation):
#   1. Duplicate scenario id.
#   2. A `kind` value not in {demux, roundtrip, binding_contract, url-corpus}.
#   3. A feature token not in {srt, rtp, udp, tcp, hls, rist}.
#   4. An `input` path that does not resolve on disk relative to the manifest dir.
#   5. A `golden` path that does not resolve on disk relative to the manifest dir.
#
# Run it:    bash scripts/check/repo/scenario-manifest.sh
#
# Overridable (scripts/ratchets/tests/self_test.sh points this at fixtures):
#   SCENARIO_MANIFEST   path to the scenarios.toml file
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

SCENARIO_MANIFEST="${SCENARIO_MANIFEST:-$ROOT/crates/tst-integration/tests/fixtures/scenarios/scenarios.toml}"

# Source-of-truth allow-sets. Extend when a new scenario kind/feature is added to scenarios.toml.
KNOWN_KINDS="demux roundtrip binding_contract url-corpus"
KNOWN_FEATURES="srt rtp udp tcp hls rist"

# ---------------------------------------------------------------------------
# run_check: validate the manifest pointed to by $SCENARIO_MANIFEST
# ---------------------------------------------------------------------------
run_check() {
  [ -f "$SCENARIO_MANIFEST" ] || { echo "FAIL: manifest not found: $SCENARIO_MANIFEST" >&2; return 1; }
  local DIR fail=0
  DIR="$(dirname "$SCENARIO_MANIFEST")"

  # --- Rule 1: duplicate ids ------------------------------------------------
  # Match both `id = "foo"` (spaced, real manifest) and `id="foo"` (the fixtures).
  # `|| true`: under `set -euo pipefail`, a no-match grep (exit 1) inside this
  # command substitution would otherwise abort the whole script mid-check with
  # no "FAIL:" message. Treat "no ids found" as an empty set and report it
  # cleanly below (an empty/malformed manifest is exactly what this ratchet
  # should catch, not silently die on).
  local ids
  ids="$(grep -E '^id[[:space:]]*=[[:space:]]*"' "$SCENARIO_MANIFEST" \
         | sed -E 's/^id[[:space:]]*=[[:space:]]*"(.*)"/\1/' || true)"
  if [ -z "$ids" ]; then
    echo "FAIL: manifest contains no scenario ids: $SCENARIO_MANIFEST" >&2
    fail=1
  fi
  local dups
  dups="$(printf '%s\n' "$ids" | sort | uniq -d)"
  if [ -n "$dups" ]; then
    echo "FAIL: duplicate scenario id(s): $(printf '%s\n' "$dups" | tr '\n' ' ')" >&2
    fail=1
  fi

  # --- Rule 2: unknown kind -------------------------------------------------
  local kind
  while IFS= read -r kind; do
    [ -n "$kind" ] || continue
    if ! printf '%s\n' $KNOWN_KINDS | grep -Fxq "$kind"; then
      echo "FAIL: unknown kind: $kind" >&2
      fail=1
    fi
  done < <(grep -E '^kind[[:space:]]*=[[:space:]]*"' "$SCENARIO_MANIFEST" \
            | sed -E 's/^kind[[:space:]]*=[[:space:]]*"(.*)"/\1/')

  # --- Rule 3: unknown feature tokens ---------------------------------------
  # features line may be `features = []` or `features = ["srt", "rtp"]`
  local feat_line tok tokens
  while IFS= read -r feat_line; do
    # Strip the key prefix, brackets, quotes, and spaces; split on commas.
    feat_line="$(printf '%s' "$feat_line" \
                 | sed -E 's/^features[[:space:]]*=[[:space:]]*//' \
                 | tr -d '[]" ')"
    # Empty array is fine.
    [ -n "$feat_line" ] || continue
    # Split comma-separated tokens (IFS scoped to this read, no save/restore).
    IFS=',' read -ra tokens <<< "$feat_line"
    for tok in "${tokens[@]}"; do
      [ -n "$tok" ] || continue
      if ! printf '%s\n' $KNOWN_FEATURES | grep -Fxq "$tok"; then
        echo "FAIL: unknown feature token: $tok" >&2
        fail=1
      fi
    done
  done < <(grep -E '^features[[:space:]]*=[[:space:]]*\[' "$SCENARIO_MANIFEST")

  # --- Rule 4: input paths must exist ---------------------------------------
  local inp
  while IFS= read -r inp; do
    [ -n "$inp" ] || continue
    if [ ! -f "$DIR/$inp" ]; then
      echo "FAIL: missing input file: $inp" >&2
      fail=1
    fi
  done < <(grep -E '^input[[:space:]]*=[[:space:]]*"' "$SCENARIO_MANIFEST" \
            | sed -E 's/^input[[:space:]]*=[[:space:]]*"(.*)"/\1/')

  # --- Rule 5: golden paths must exist --------------------------------------
  local gld
  while IFS= read -r gld; do
    [ -n "$gld" ] || continue
    if [ ! -f "$DIR/$gld" ]; then
      echo "FAIL: missing golden file: $gld" >&2
      fail=1
    fi
  done < <(grep -E '^golden[[:space:]]*=[[:space:]]*"' "$SCENARIO_MANIFEST" \
            | sed -E 's/^golden[[:space:]]*=[[:space:]]*"(.*)"/\1/')

  return "$fail"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
run_check
echo "scenario-manifest: OK"
