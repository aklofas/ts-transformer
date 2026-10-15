#!/usr/bin/env bash
# Skip-ledger sync ratchet.
#
# Proves tests/coverage/skip-ledger.toml stays in lockstep with the actual
# Rust `#[ignore]` annotations in the tree, so a skip can neither be added and
# forgotten nor linger in the ledger after the test is un-ignored.
#
# Enforced (fails the build on violation):
#   1. Every active `#[ignore]` test function has a NON-resolved ledger entry
#      whose `test` is the exact function name.            (unledgered skip)
#   2. Every NON-resolved ledger entry's `test` is a real active `#[ignore]`.
#      (stale ledger — un-ignored tests must be marked `resolved`, not left.)
#   3. Every NON-resolved placeholder/blocked_bug entry has an `expires_after`
#      strictly in the future.                              (expired deferral)
#
# Out of scope (by design, documented in the ledger header): module-level
# Python `pytest.mark.skipif` capability guards.
#
# Run it:   bash scripts/check/repo/skip-ledger-sync.sh
#
# Overridable (scripts/ratchets/tests/self_test.sh points these at fixtures):
#   SKIP_LEDGER_FILE   path to the ledger TOML
#   SKIP_SCAN_DIRS     space-separated dirs scanned for `#[ignore]`
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

SKIP_LEDGER_FILE="${SKIP_LEDGER_FILE:-$ROOT/tests/coverage/skip-ledger.toml}"
SKIP_SCAN_DIRS="${SKIP_SCAN_DIRS:-$ROOT/crates}"
TODAY="$(date +%F)"

# Emit one active-`#[ignore]` function name per line. An `#[ignore]` ATTRIBUTE
# starts the line (after optional indent); comment lines that merely mention
# `#[ignore]` start with `//`, `///`, `//!` or `*`, so the `^[[:space:]]*#\[`
# anchor excludes them. The function name is the next `fn NAME` seen.
enumerate_active_ignores() {
  local dir
  for dir in $SKIP_SCAN_DIRS; do
    [ -d "$dir" ] || continue
    find "$dir" -name '*.rs' -type f -print0 | while IFS= read -r -d '' f; do
      awk '
        /^[[:space:]]*#\[ignore/ { pending = 1; next }
        pending && /[[:space:]]*fn[[:space:]]/ {
          if (match($0, /fn[[:space:]]+[A-Za-z0-9_]+/)) {
            s = substr($0, RSTART, RLENGTH); sub(/fn[[:space:]]+/, "", s);
            print s
          }
          pending = 0
        }
      ' "$f"
    done
  done | sort -u
}

# Emit `test<TAB>resolved<TAB>class<TAB>expires_after` per [[skip]] block.
# Triple-quoted `note = """..."""` bodies are skipped so prose can't be misread
# as field rows.
parse_ledger() {
  awk '
    function val(s,  v) { v = s; sub(/^[^=]*= *"/, "", v); sub(/".*$/, "", v); return v }
    function flush() { if (have) print t "\t" r "\t" c "\t" e; t=""; r=""; c=""; e=""; have=0 }
    BEGIN { in_note = 0; have = 0 }
    {
      n = gsub(/"""/, "&", $0)            # count triple-quotes on this line
      if (in_note) { if (n % 2 == 1) in_note = 0; next }
      if (n % 2 == 1) { in_note = 1; next }
      if ($0 ~ /^\[\[skip\]\]/)          { flush(); have = 1; next }
      if ($0 ~ /^test = /)               { t = val($0) }
      else if ($0 ~ /^resolved = /)      { r = val($0) }
      else if ($0 ~ /^class = /)         { c = val($0) }
      else if ($0 ~ /^expires_after = /) { e = val($0) }
    }
    END { flush() }
  ' "$SKIP_LEDGER_FILE"
}

run_check() {
  [ -f "$SKIP_LEDGER_FILE" ] || { echo "FAIL: ledger not found: $SKIP_LEDGER_FILE" >&2; return 1; }

  local active ledger fail=0
  active="$(enumerate_active_ignores)"
  ledger="$(parse_ledger)"

  # Active (non-resolved) ledger test names.
  local active_entries
  active_entries="$(printf '%s\n' "$ledger" | awk -F'\t' 'NF && $2 == "" { print $1 }')"

  # Rule 1: every active #[ignore] has a non-resolved ledger entry.
  local fn
  while IFS= read -r fn; do
    [ -n "$fn" ] || continue
    if ! printf '%s\n' "$active_entries" | grep -Fxq "$fn"; then
      echo "FAIL: active #[ignore] '$fn' has no (non-resolved) skip-ledger entry" >&2
      fail=1
    fi
  done <<EOF
$active
EOF

  # Rule 2: every non-resolved ledger entry maps to a real active #[ignore].
  while IFS= read -r fn; do
    [ -n "$fn" ] || continue
    if ! printf '%s\n' "$active" | grep -Fxq "$fn"; then
      echo "FAIL: skip-ledger entry '$fn' is not an active #[ignore]; mark it resolved or remove it" >&2
      fail=1
    fi
  done <<EOF
$active_entries
EOF

  # Rule 3: non-resolved placeholder/blocked_bug entries need a future expiry.
  local line t r c e
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    t="$(printf '%s' "$line" | cut -f1)"
    r="$(printf '%s' "$line" | cut -f2)"
    c="$(printf '%s' "$line" | cut -f3)"
    e="$(printf '%s' "$line" | cut -f4)"
    [ -z "$r" ] || continue                       # resolved entries are history
    case "$c" in
      placeholder|blocked_bug) ;;
      *) continue ;;
    esac
    if [ -z "$e" ]; then
      echo "FAIL: $c entry '$t' has no expires_after" >&2
      fail=1
    elif [ "$e" \< "$TODAY" ]; then
      echo "FAIL: $c entry '$t' expired on $e (today is $TODAY)" >&2
      fail=1
    fi
  done <<EOF
$ledger
EOF

  return "$fail"
}

run_check
echo "skip-ledger-sync: OK"
