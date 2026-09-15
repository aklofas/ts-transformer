#!/usr/bin/env bash
# MSRV-consistency ratchet (deep-review-4 DEBT-19).
#
# The workspace pins ONE Rust toolchain in rust-toolchain.toml, but the same
# version string is repeated at ~30 literal sites: [workspace.package]
# rust-version, two crate-local rust-version literals, every workflow's
# RUSTUP_TOOLCHAIN / dtolnay toolchain: input, the embedded gate scripts'
# `rustup target add … --toolchain X`, and four user-facing doc claims. A
# toolchain bump that misses one site either silently tests on the wrong
# compiler (CI) or publishes a wrong MSRV claim (docs). This rail makes the
# first forced bump a mechanical sweep: every site must equal the channel.
#
# Two assertions per site:
#   (a) no OTHER version appears where the pinned one is expected
#       (drift) — the site regex is matched with a generic X.Y[.Z] and with
#       the pinned version; the counts must be equal;
#   (b) the site still exists (min occurrences) — a rewrite that removes a
#       site must update the table, so the rail cannot go blind quietly.
#
# Run it:    bash scripts/check/repo/msrv-consistency.sh
# Self-test: bash scripts/check/repo/msrv-consistency.sh --self-test
#            (copies the real sites into a temp tree, plants a 9.99, expects
#            FAIL; removes a site, expects FAIL)
#
# Overridable: MSRV_ROOT — tree root the table paths resolve against.
#
# Bash 3.2-portable (macOS CI is a gating platform): no mapfile/readarray/
# declare -A, mktemp always given a template, no `sed -i`. Does not read
# stdin (the pre-push runner feeds </dev/null).
set -euo pipefail

ROOT="${MSRV_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
TOOLCHAIN_FILE="$ROOT/rust-toolchain.toml"

# ---------------------------------------------------------------------------
# Site table: <path relative to ROOT> TAB <ERE with @V@ for the version> TAB <min>
# Regexes are matched with `grep -oE` (occurrences, not lines). Keep each
# regex anchored on the surrounding syntax so a prose mention of an older
# version in a comment never counts as a site.
# ---------------------------------------------------------------------------
sites() {
  printf '%s\t%s\t%s\n' \
    'Cargo.toml'                                          '^rust-version[[:space:]]*=[[:space:]]*"@V@"'                    1 \
    'crates/tst-test-helpers/Cargo.toml'                  '^rust-version[[:space:]]*=[[:space:]]*"@V@"'                    1 \
    'crates/tst-integration/Cargo.toml'                   '^rust-version[[:space:]]*=[[:space:]]*"@V@"'                    1 \
    '.github/workflows/ci.yml'                            '(RUSTUP_TOOLCHAIN|toolchain|rust-version):[[:space:]]*"@V@"'    15 \
    '.github/workflows/ci.yml'                            'rustup toolchain install @V@ '                                  1 \
    '.github/workflows/ci.yml'                            'cargo \+@V@ '                                                   1 \
    '.github/workflows/crates-io.yml'                     '(RUSTUP_TOOLCHAIN|toolchain):[[:space:]]*"@V@"'                 2 \
    '.github/workflows/python-wheels.yml'                 '(RUSTUP_TOOLCHAIN|toolchain):[[:space:]]*"@V@"'                 3 \
    '.github/workflows/interop.yml'                       '(RUSTUP_TOOLCHAIN|toolchain):[[:space:]]*"@V@"'                 2 \
    '.github/workflows/jvm-jar.yml'                       '(toolchain:[[:space:]]*"@V@"|Install Rust @V@$)'                 2 \
    '.github/workflows/apple-ios.yml'                     'toolchain:[[:space:]]*"@V@"'                                     1 \
    'embedded/scripts/check/no-std-baremetal.sh'          '[-][-]toolchain @V@ '                                             3 \
    'embedded/scripts/check/firmware-qemu.sh'             '[-][-]toolchain @V@ '                                             1 \
    'embedded/scripts/check/qemu-runtime.sh'              '[-][-]toolchain @V@ '                                             2 \
    'embedded/freertos-srt/substrate/build-common.sh'     '[-][-]toolchain @V@ '                                             1 \
    'scripts/check/repo/release-version-consistency.sh'   'rust-version = "@V@"'                                           1 \
    'README.md'                                           'MSRV(: |-)@V@'                                                  2 \
    'docs/languages/rust.md'                              '(Rust \*\*@V@\*\*|auto-uses @V@ via rustup)'                    2 \
    'docs/reference/compatibility.md'                     'MSRV \*\*@V@\*\*'                                               1 \
    'docs/start/quickstart.md'                            '(Rust @V@\+|pins to @V@ )'                                      2
}

# ---------------------------------------------------------------------------
# run_check
# ---------------------------------------------------------------------------
run_check() {
  local pinned
  pinned="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+(\.[0-9]+)?)".*/\1/p' "$TOOLCHAIN_FILE" | head -1)"
  if [ -z "$pinned" ]; then
    echo "FAIL: could not read a numeric channel from $TOOLCHAIN_FILE (a 'stable'/'nightly' channel is not an MSRV pin)" >&2
    return 1
  fi
  # Escape the dots for ERE use; the generic form matches any X.Y[.Z].
  local pin_re gen_re
  pin_re="$(printf '%s' "$pinned" | sed 's/\./\\./g')"
  gen_re='[0-9]+\.[0-9]+(\.[0-9]+)?'

  local rc=0 checked=0 path re min file want any got
  while IFS="$(printf '\t')" read -r path re min; do
    [ -n "$path" ] || continue
    file="$ROOT/$path"
    if [ ! -f "$file" ]; then
      echo "FAIL: site file missing: $path (update the table in $0 if it moved)" >&2
      rc=1; continue
    fi
    want="${re//@V@/$pin_re}"
    any="${re//@V@/$gen_re}"
    # `|| true`: grep exits 1 on no match, which pipefail+set -e would turn
    # into a silent abort instead of a counted zero.
    got="$( (grep -oE "$want" "$file" || true) | wc -l | tr -d ' ')"
    local total
    total="$( (grep -oE "$any" "$file" || true) | wc -l | tr -d ' ')"
    if [ "$total" -ne "$got" ]; then
      echo "FAIL: $path carries a toolchain version other than $pinned:" >&2
      grep -nE "$any" "$file" | grep -vE "$want" | sed 's/^/      /' >&2 || true
      rc=1
    fi
    if [ "$got" -lt "$min" ]; then
      echo "FAIL: $path: expected >= $min pinned site(s) matching /$want/, found $got (site removed or rewritten — update the table)" >&2
      rc=1
    fi
    checked=$((checked + got))
  done < <(sites)

  [ "$rc" -eq 0 ] || return 1
  echo "msrv-consistency: OK — $checked sites agree with rust-toolchain.toml channel $pinned"
}

# ---------------------------------------------------------------------------
# self_test — copy the REAL sites into a temp tree, run the real script
# against it via MSRV_ROOT, plant drift, expect the right verdicts.
# ---------------------------------------------------------------------------
self_test() {
  local tmp; tmp="$(mktemp -d "${TMPDIR:-/tmp}/msrv-selftest.XXXXXX")"
  trap 'rm -rf "$tmp"' RETURN

  copy_tree() {
    rm -rf "$tmp/tree"; mkdir -p "$tmp/tree"
    mkdir -p "$(dirname "$tmp/tree/rust-toolchain.toml")"
    cp "$ROOT/rust-toolchain.toml" "$tmp/tree/rust-toolchain.toml"
    local path re min
    while IFS="$(printf '\t')" read -r path re min; do
      [ -n "$path" ] || continue
      mkdir -p "$(dirname "$tmp/tree/$path")"
      cp "$ROOT/$path" "$tmp/tree/$path"
    done < <(sites)
  }

  # plant <path> : rewrite EVERY pinned occurrence in the copied file to 9.99
  # (a drift at that site; the pinned count drops below min AND the generic
  # count exceeds the pinned count — both assertions must fire). Portable
  # sed only (no GNU `0,/re/`, no `-i`).
  plant() {
    local f="$tmp/tree/$1" pinned esc
    pinned="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([0-9.]+)".*/\1/p' "$tmp/tree/rust-toolchain.toml")"
    esc="$(printf '%s' "$pinned" | sed 's/\./\\./g')"
    sed "s/$esc/9.99/g" "$f" > "$f.new"
    mv "$f.new" "$f"
  }

  expect() { # <pass|fail> <label>
    local want="$1" label="$2" rc
    if MSRV_ROOT="$tmp/tree" bash "$0" >/dev/null 2>&1; then rc=pass; else rc=fail; fi
    if [ "$rc" = "$want" ]; then
      echo "  ok: $label (expected $want)"
    else
      echo "  SELF-TEST FAIL: $label expected $want got $rc" >&2
      return 1
    fi
  }

  copy_tree
  expect pass "real sites copied verbatim agree" || return 1

  copy_tree; plant 'crates/tst-test-helpers/Cargo.toml'
  expect fail "planted 9.99 in a crate-local rust-version" || return 1

  copy_tree; plant '.github/workflows/python-wheels.yml'
  expect fail "planted 9.99 in a workflow toolchain pin" || return 1

  copy_tree; plant 'embedded/scripts/check/qemu-runtime.sh'
  expect fail "planted 9.99 in an embedded --toolchain pin" || return 1

  copy_tree; plant 'README.md'
  expect fail "planted 9.99 in the README MSRV badge" || return 1

  copy_tree; printf '[toolchain]\nchannel = "9.99"\n' > "$tmp/tree/rust-toolchain.toml"
  expect fail "toolchain bumped, sites not swept" || return 1

  copy_tree; rm "$tmp/tree/docs/reference/compatibility.md"
  expect fail "site file removed → rail refuses to go blind" || return 1

  copy_tree; printf '[toolchain]\nchannel = "stable"\n' > "$tmp/tree/rust-toolchain.toml"
  expect fail "non-numeric channel is not an MSRV pin" || return 1

  echo "msrv-consistency self-test: OK"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
else
  run_check
fi
