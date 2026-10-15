#!/usr/bin/env bash
# Fail if a tracked file still names a package directory that has moved
# away: a literal reference to one is a dead link, a stale doc path or a
# broken build trigger. Each pattern below names its old and new locations;
# the failure message repeats the mapping. CHANGELOG.md is exempt (history).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

# crates/{tst-c,tst-c-core,tst-py} -> bindings/, bindings/c/{tst-c,tst-c-core}
# -> bindings/c{,/core}, crates/baremetal-qemu{,-c} -> embedded/. The `tst-c`
# leaf forms carry a trailing-boundary class so the live `crates/tst-core`
# does not match; the `-core` forms are listed first.
PATTERN='crates/tst-c-core|crates/tst-py|crates/baremetal-qemu|bindings/c/tst-c-core|(crates|bindings/c)/tst-c([/")`, ]|$)'

# Embedded gate scripts and embedded-only vendor submodules moved under
# embedded/. The new paths contain the old ones as suffixes
# (`embedded/vendor/lwip`, `embedded/scripts/lib/run-...`), so hits on the
# new prefixes are filtered back out. .gitmodules is exempt for the vendor
# class: submodule NAMES keep the old `vendor/lwip` form so existing
# checkouts' .git/modules state stays valid.
PATTERN_EMB='scripts/check/embedded/|scripts/check/c/firmware-qemu\.sh|scripts/lib/run-freertos-srt-example\.sh'
PATTERN_VEND='vendor/(freertos-kernel|freertos-plus-posix|lwip)'

# vendor/{srt,mbedtls,librist} moved into crates/{srt-sys,mbedtls-src,
# rist-sys}/vendor/ so each crate is a self-contained package. Only the
# docs surface (docs/ + the two READMEs) is scanned: inside its own crate a
# bare `vendor/srt` is correct (Cargo resolves paths from the manifest), and
# source comments cite libsrt/librist files that way. The trailing-boundary
# class keeps `vendor/srtcore`-style names out; hits on the new
# `crates/<c>/vendor/` prefixes are filtered back out.
PATTERN_VEND_NATIVE='vendor/(srt|mbedtls|librist)([/")`, .]|$)'

# Flat `scripts/check-<name>.sh` rails moved to `scripts/check/<group>/`;
# every live rail is one segment deeper, so the flat form cannot match one.
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
