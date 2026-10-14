#!/usr/bin/env bash
# Verify that libtstrans.so's dynamic export table contains zero
# srt_*/SRT_* symbols. Export control is done by `bindings/c/build.rs`:
# on Linux rustc's anonymous version script localises the libsrt/mbedTLS
# symbols (the build deliberately does NOT use `--exclude-libs=ALL`); on
# macOS the `exports.txt` list (`-exported_symbols_list`) whitelists
# tst_*/TST_*. This ratchet enforces the result at CI time.
#
# A regression here means either:
#   - the build.rs link-arg was dropped from emission
#   - libsrt has a new C-visible symbol the version-script / exports list
#     didn't anticipate
#   - someone added a libsrt re-export to tst-c without going through
#     the Rust srt_sys::* binding path
#
# All three are real bugs.

set -euo pipefail

cd "$(dirname "$0")/../../.."

# Linux GNU-only (matches symbol_audit.rs test gating).
if [ "$(uname -s)" != "Linux" ]; then
    echo "  SKIP: non-Linux host (this ratchet uses GNU nm)"
    exit 0
fi

LIB="target/debug/libtstrans.so"
# Always (re)build with `--features srt` — this ratchet is only meaningful when
# libsrt is actually statically linked in. `srt` is opt-in (default-off) since
# the all-transports-optional change, so a stray default build of libtstrans.so
# would link no libsrt and the check below would pass vacuously. `--features srt`
# (without --no-default-features) keeps mbedtls on, matching a real encrypted
# srt consumer build.
echo "Building tst-c with --features srt to produce $LIB..."
SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt

LEAKED=$(nm -D -g --defined-only "$LIB" 2>&1 \
    | awk '$2 ~ /^[TDR]$/ { print $3 }' \
    | { grep -E '^(srt_|SRT_)' || true; })

if [ -n "$LEAKED" ]; then
    echo "FAIL: libtstrans.so dynamic export table contains srt_*/SRT_* symbols:"
    echo "$LEAKED" | sed 's/^/  /'
    echo
    echo "Plan B's symbol-hygiene wiring (bindings/c/build.rs) should hide these."
    echo "Diagnose:"
    echo "  cargo build -p tst-c -v 2>&1 | grep exclude-libs"
    exit 1
fi

echo "OK: libtstrans.so exports zero srt_*/SRT_* symbols"
