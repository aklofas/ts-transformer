#!/usr/bin/env bash
# url-keys: the URL key table in docs/reference/conventions.md is written from
# the key tables that drive the seven URL parsers (tst_udp, tst_tcp, tst_rist,
# tst_hls, tst_srt, tst_rtp). This rail fails, with a unified diff, when the
# block between the print-url-keys markers differs from a fresh render.
#
# Regenerate: cargo run -p tst-binding-gen --bin print-url-keys -- --write
# Overridable: URL_KEYS_ROOT (a tree to check instead of this one).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 \
    cargo run -q -p tst-binding-gen --bin print-url-keys -- --check --root "${URL_KEYS_ROOT:-$ROOT}"
