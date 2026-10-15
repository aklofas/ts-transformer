#!/usr/bin/env bash
# Platform-capability sync ratchet.
#
# Proves tests/coverage/platform-capabilities.toml stays in lockstep with the
# actual test-level platform cfg-gates in the tree, so a gated file cannot be
# added without a manifest entry and a soft-promotion window cannot silently
# lapse.
#
# Enforced (fails the build on violation):
#   1. Every test-level platform cfg-gated file has a NON-resolved
#      [[platform_skip]] whose `file` matches.          (unlisted gate)
#   2. Every NON-resolved [[platform_skip]] `file` is a real gated file.
#      (stale entry — removed gate must be marked `resolved`, not left)
#   3. Every NON-resolved blocked_bug [[platform_skip]] has a future
#      expires_after.                                    (expired deferral)
#      Every A-soft / gating-pending [[capability]] has a future soft_until.
#                                                        (promotion alarm)
#
# Run it:    bash scripts/check/repo/platform-capabilities.sh
#
# Overridable env vars (scripts/ratchets/tests/self_test.sh points them at a temp tree):
#   PLATCAP_FILE      path to the manifest TOML
#   PLATCAP_SCAN_DIRS space-separated dirs scanned for platform cfg-gates
#   PLATCAP_ROOT      root used for $ROOT-relative path stripping in the
#                     enumerator (defaults to $ROOT = the workspace root)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

MANIFEST="${PLATCAP_FILE:-$ROOT/tests/coverage/platform-capabilities.toml}"
SCAN_DIRS="${PLATCAP_SCAN_DIRS:-$ROOT/crates}"
TODAY="$(date +%F)"

# Emit one manifest-relative path per test file carrying a top-level platform
# cfg-gate. Strips the PLATCAP_ROOT (default: $ROOT) prefix so paths match the
# manifest `file` field values. Evaluated at call time so the fixtures can
# override PLATCAP_ROOT after the script loads.
enumerate_gated_files() {
  local strip_root d
  strip_root="${PLATCAP_ROOT:-$ROOT}"
  for d in ${PLATCAP_SCAN_DIRS:-$SCAN_DIRS}; do
    [ -d "$d" ] || continue
    # Only tests/ subtrees; match file-level (#![cfg) or item-level (#[cfg)
    # platform guards for windows/macos/linux/unix.
    grep -rlE \
      '#!?\[cfg\((not\()?target_os *= *"(windows|macos|linux)"|#!?\[cfg\(unix\)|#!?\[cfg\(windows\)' \
      "$d"/*/tests 2>/dev/null || true
  done | sed "s#^$strip_root/##" | sort -u
}

# Emit `file<TAB>resolved<TAB>class<TAB>expires_after` per [[platform_skip]].
# Triple-quoted `note = """..."""` bodies are skipped so prose can't be misread
# as field rows.
parse_platform_skips() {
  local mf="${PLATCAP_FILE:-$MANIFEST}"
  awk '
    function val(s,  v) { v = s; sub(/^[^=]*= *"/, "", v); sub(/".*$/, "", v); return v }
    function flush() { if (have) print f "\t" r "\t" c "\t" e; f=""; r=""; c=""; e=""; have=0 }
    BEGIN { in_note = 0; have = 0 }
    {
      n = gsub(/"""/, "&", $0)
      if (in_note) { if (n % 2 == 1) in_note = 0; next }
      if (n % 2 == 1) { in_note = 1; next }
      if ($0 ~ /^\[\[platform_skip\]\]/) { flush(); have = 1; next }
      if ($0 ~ /^file = /)              { f = val($0) }
      else if ($0 ~ /^resolved = /)     { r = val($0) }
      else if ($0 ~ /^class = /)        { c = val($0) }
      else if ($0 ~ /^expires_after = /) { e = val($0) }
    }
    END { flush() }
  ' "$mf"
}

# Emit `status<TAB>soft_until` per [[capability]].
parse_capabilities() {
  local mf="${PLATCAP_FILE:-$MANIFEST}"
  awk '
    function val(s,  v) { v = s; sub(/^[^=]*= *"/, "", v); sub(/".*$/, "", v); return v }
    function flush() { if (have) print s "\t" u; s=""; u=""; have=0 }
    BEGIN { in_note = 0; have = 0 }
    {
      n = gsub(/"""/, "&", $0)
      if (in_note) { if (n % 2 == 1) in_note = 0; next }
      if (n % 2 == 1) { in_note = 1; next }
      if ($0 ~ /^\[\[capability\]\]/) { flush(); have = 1; next }
      if ($0 ~ /^status = /)          { s = val($0) }
      else if ($0 ~ /^soft_until = /) { u = val($0) }
    }
    END { flush() }
  ' "$mf"
}

run_check() {
  local mf="${PLATCAP_FILE:-$MANIFEST}"
  [ -f "$mf" ] || { echo "FAIL: manifest not found: $mf" >&2; return 1; }

  local fail=0 gated skips files_active
  gated="$(enumerate_gated_files)"
  skips="$(parse_platform_skips)"
  files_active="$(printf '%s\n' "$skips" | awk -F'\t' 'NF && $2 == "" { print $1 }')"

  # Rule 1: every gated file is listed (non-resolved).
  local gf
  while IFS= read -r gf; do
    [ -n "$gf" ] || continue
    if ! printf '%s\n' "$files_active" | grep -Fxq "$gf"; then
      echo "FAIL: gated test file '$gf' has no platform_skip entry" >&2
      fail=1
    fi
  done <<EOF
$gated
EOF

  # Rule 2: every non-resolved entry maps to a real gated file.
  local ef
  while IFS= read -r ef; do
    [ -n "$ef" ] || continue
    if ! printf '%s\n' "$gated" | grep -Fxq "$ef"; then
      echo "FAIL: platform_skip '$ef' is not a gated file; mark resolved or remove" >&2
      fail=1
    fi
  done <<EOF
$files_active
EOF

  # Rule 3a: non-resolved blocked_bug skips need a future expires_after.
  local line f r c e
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    f="$(printf '%s' "$line" | cut -f1)"
    r="$(printf '%s' "$line" | cut -f2)"
    c="$(printf '%s' "$line" | cut -f3)"
    e="$(printf '%s' "$line" | cut -f4)"
    [ -z "$r" ] || continue                        # resolved entries are history
    [ "$c" = "blocked_bug" ] || continue
    if [ -z "$e" ]; then
      echo "FAIL: blocked_bug '$f' has no expires_after" >&2
      fail=1
    elif [ "$e" \< "$TODAY" ]; then
      echo "FAIL: blocked_bug '$f' expired on $e (today $TODAY)" >&2
      fail=1
    fi
  done <<EOF
$skips
EOF

  # Rule 3b: A-soft / gating-pending capabilities need a future soft_until.
  local s u
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    s="$(printf '%s' "$line" | cut -f1)"
    u="$(printf '%s' "$line" | cut -f2)"
    case "$s" in A-soft|gating-pending) ;; *) continue ;; esac
    if [ -z "$u" ]; then
      echo "FAIL: $s capability has no soft_until" >&2
      fail=1
    elif [ "$u" \< "$TODAY" ]; then
      echo "FAIL: $s capability soft_until lapsed on $u (today $TODAY) — promote or extend" >&2
      fail=1
    fi
  done <<EOF
$(parse_capabilities)
EOF

  return "$fail"
}

run_check
echo "platform-capabilities: OK"
