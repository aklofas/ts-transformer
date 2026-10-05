#!/usr/bin/env bash
# Fail if any reference to a RELOCATED-AWAY package directory survives in the
# tracked tree. These directories were moved during the 2026-06-02 refactor
# series and no longer exist, so a literal reference to them is a silent dead
# link, a stale doc path, or (worse) a broken build trigger:
#
#   crates/tst-c, crates/tst-c-core, crates/tst-py   -> bindings/ (relocation)
#   bindings/c/tst-c, bindings/c/tst-c-core          -> bindings/c{,/core} (Option-B flatten)
#   crates/baremetal-qemu, crates/baremetal-qemu-c   -> embedded/ (embedded move)
#   vendor/{srt,mbedtls,librist} (workspace root)    -> crates/{srt-sys,mbedtls-src,rist-sys}/vendor/... (crates-io-packaging move)
#   scripts/check-<name>.sh (flat)                   -> scripts/check/<group>/<name>.sh (2026-06-03 ratchet reorg)
#
# This rail exists because the moves above are path-coupled and the literal
# grep used during each move is BLIND to a few forms (slashless `cd crates/tst-c`,
# relative `../tst-c-core` build triggers). This catches the literal-path class
# at CI time so it can't silently rot.
#
# CHANGELOG.md is exempt: it is an append-only history and its entries were
# accurate at the time they were written.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

# Removed path prefixes. The leaf forms `crates/tst-c` and `bindings/c/tst-c`
# carry a trailing-boundary class so they do NOT match the still-present
# `crates/tst-core` (next char `o`) or `bindings/c/core` (no `tst-c`). The
# `*-core` removed dirs are matched explicitly (longest-first), and `baremetal-qemu`
# covers both `baremetal-qemu` and `baremetal-qemu-c`.
PATTERN='crates/tst-c-core|crates/tst-py|crates/baremetal-qemu|bindings/c/tst-c-core|(crates|bindings/c)/tst-c([/")`, ]|$)'

# 2026-07-05 embedded isolation: gate scripts -> embedded/scripts/, embedded-only
# vendor submodules -> embedded/vendor/. The old top-level forms are dead:
#   scripts/check/embedded/*                  -> embedded/scripts/check/
#   scripts/check/c/firmware-qemu.sh          -> embedded/scripts/check/firmware-qemu.sh
#   scripts/lib/run-freertos-srt-example.sh   -> embedded/scripts/lib/
#   vendor/{freertos-kernel,freertos-plus-posix,lwip} -> embedded/vendor/...
# The vendor class and the lib-helper form need carve-outs: their NEW paths
# contain the old ones as suffixes (`embedded/vendor/lwip` contains `vendor/lwip`,
# `embedded/scripts/lib/run-...` contains `scripts/lib/run-...`), so hits on the
# new prefixes are filtered back out. The check-script forms have no such
# collision (`embedded/scripts/check/` does not contain `scripts/check/embedded/`).
# .gitmodules is exempt for the vendor class only: submodule NAMES keep the
# historical `[submodule "vendor/lwip"]` form (renaming a submodule name breaks
# existing checkouts' .git/modules state — deliberately not done).
PATTERN_EMB='scripts/check/embedded/|scripts/check/c/firmware-qemu\.sh|scripts/lib/run-freertos-srt-example\.sh'
PATTERN_VEND='vendor/(freertos-kernel|freertos-plus-posix|lwip)'

# 2026-07-31 crates-io-packaging: the shared workspace-root vendor/{srt,mbedtls}
# submodules (plus vendor/librist, added alongside rist-sys) moved INTO their
# owning crate so each `-sys`/`-src` crate is a self-contained, publishable
# package (crates.io forbids a package referencing files outside its own
# directory):
#   vendor/srt      -> crates/srt-sys/vendor/srt
#   vendor/mbedtls  -> crates/mbedtls-src/vendor/mbedtls
#   vendor/librist  -> crates/rist-sys/vendor/librist
# UNLIKE the other classes above, this one deliberately scans only the DOCS
# PROSE surface (docs/ + the two top-level READMEs), not the whole tracked
# tree: a bare `vendor/srt` (no `crates/srt-sys/` prefix) is CORRECT, not
# stale, wherever it's written relative to its own crate — Cargo resolves
# build.rs/Cargo.toml paths relative to the manifest, so crates/srt-sys's
# (and rist-sys's / mbedtls-src's) own Cargo.toml exclude-lists, build.rs,
# README.md, and lib.rs doc comments legitimately keep the bare form, as do
# incidental source comments elsewhere in the workspace that cite a libsrt/
# librist file path rather than describe where the submodule lives. What
# this guards against is specifically reader-facing docs implying the
# submodule still lives at the shared workspace-root vendor/ (it hasn't
# since this move) — narrowed to the same docs surface as the srt-sys/
# rist-sys package-name sweep from the same arc (docs/ + the two READMEs).
# A trailing-boundary class (mirrors the `tst-c` collision guard in PATTERN
# above) keeps this from matching `vendor/srtcore`-style non-collisions (none
# exist today; boundary-safe by default rather than by luck). Hits on the new
# `crates/<c>/vendor/` prefixes are filtered back out the same way the
# embedded class filters `embedded/vendor/` above.
PATTERN_VEND_NATIVE='vendor/(srt|mbedtls|librist)([/")`, .]|$)'

# 2026-06-03 bash-ratchet reorg: the flat `scripts/check-<name>.sh` scripts
# moved into per-group subdirectories (`scripts/check/<group>/<name>.sh`).
# This class rotted silently for months (18 stale hits found across
# bindings/python alone) because no rail caught the flat form once every
# script moved — this closes that gap. `scripts/check-[a-z-]+\.sh` cannot
# collide with a current path: every live rail script lives under
# `scripts/check/<group>/`, one path segment deeper, so the flat form is
# unambiguously dead once matched.
PATTERN_RATCHET='scripts/check-[a-z-]+\.sh'

# Exempt: CHANGELOG (history) and this script (it names the forbidden paths).
hits=$(git ls-files \
  | grep -vE '^(CHANGELOG\.md|scripts/check/repo/no-stale-relocated-paths\.sh)$' \
  | tr '\n' '\0' \
  | xargs -0 grep -InE "$PATTERN" 2>/dev/null || true)

hits_emb=$(git ls-files \
  | grep -vE '^(CHANGELOG\.md|scripts/check/repo/no-stale-relocated-paths\.sh)$' \
  | tr '\n' '\0' \
  | xargs -0 grep -InE "$PATTERN_EMB" 2>/dev/null \
  | grep -v 'embedded/scripts/lib/' || true)

hits_vend=$(git ls-files \
  | grep -vE '^(CHANGELOG\.md|\.gitmodules|scripts/check/repo/no-stale-relocated-paths\.sh)$' \
  | tr '\n' '\0' \
  | xargs -0 grep -InE "$PATTERN_VEND" 2>/dev/null \
  | grep -v 'embedded/vendor/' || true)

hits_vend_native=$(git ls-files docs/ README.md embedded/README.md \
  | tr '\n' '\0' \
  | xargs -0 grep -InE "$PATTERN_VEND_NATIVE" 2>/dev/null \
  | grep -vE 'crates/(srt-sys|mbedtls-src|rist-sys)/vendor/' || true)

hits_ratchet=$(git ls-files \
  | grep -vE '^(CHANGELOG\.md|scripts/check/repo/no-stale-relocated-paths\.sh)$' \
  | tr '\n' '\0' \
  | xargs -0 grep -InE "$PATTERN_RATCHET" 2>/dev/null || true)

if [ -n "$hits$hits_emb$hits_vend$hits_vend_native$hits_ratchet" ]; then
  echo "FAIL: references to relocated-away package directories found (these dirs no longer exist):" >&2
  [ -n "$hits" ] && echo "$hits" >&2
  [ -n "$hits_emb" ] && echo "$hits_emb" >&2
  [ -n "$hits_vend" ] && echo "$hits_vend" >&2
  [ -n "$hits_vend_native" ] && echo "$hits_vend_native" >&2
  [ -n "$hits_ratchet" ] && echo "$hits_ratchet" >&2
  echo "" >&2
  echo "Update each to its current location:" >&2
  echo "  crates/tst-c, crates/tst-c-core, crates/tst-py  -> bindings/c, bindings/c/core, bindings/python" >&2
  echo "  bindings/c/tst-c, bindings/c/tst-c-core         -> bindings/c, bindings/c/core" >&2
  echo "  crates/baremetal-qemu{,-c}                      -> embedded/baremetal-qemu{,-c}" >&2
  echo "  scripts/check/embedded/*, scripts/check/c/firmware-qemu.sh -> embedded/scripts/check/" >&2
  echo "  scripts/lib/run-freertos-srt-example.sh         -> embedded/scripts/lib/" >&2
  echo "  vendor/{freertos-kernel,freertos-plus-posix,lwip} -> embedded/vendor/ (.gitmodules submodule NAMES exempt)" >&2
  echo "  vendor/srt      -> crates/srt-sys/vendor/srt" >&2
  echo "  vendor/mbedtls  -> crates/mbedtls-src/vendor/mbedtls" >&2
  echo "  vendor/librist  -> crates/rist-sys/vendor/librist  (.gitmodules submodule NAMES exempt)" >&2
  echo "  scripts/check-<name>.sh -> scripts/check/<group>/<name>.sh" >&2
  echo "(CHANGELOG.md is exempt as historical record.)" >&2
  exit 1
fi

echo "OK: no stale references to relocated-away package directories"
