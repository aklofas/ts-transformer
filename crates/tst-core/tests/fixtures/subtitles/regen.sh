#!/usr/bin/env bash
# Regenerate synthetic subtitle / caption MPEG-TS fixtures for
# tst-core subtitle carriage tests.
#
# Unlike the audio fixtures, these are NOT produced by ffmpeg —
# they're emitted by our own Muxer through tst-core's
# `gen-subtitle-fixtures` maintainer bin (pure tst-core, no transport).
# Bootstrap cycle: muxer must work to emit them, then they guard
# against regression in either side.
#
# Run: ./regen.sh
#
# Tools required (locally; not on CI): a cargo build environment. The
# bin links no libsrt or mbedTLS.

set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"

# Climb four levels (subtitles -> fixtures -> tests -> tst-core ->
# crates/); cargo finds the workspace from there.
cd "$DIR/../../../.."

SRT_FORCE_VENDORED=1 cargo run -p tst-core --bin gen-subtitle-fixtures --release -- "$DIR"

echo
echo "Regenerated fixtures:"
ls -la "$DIR"/*.ts
