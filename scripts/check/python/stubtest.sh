#!/usr/bin/env bash
# Verifies EVERY tst-py .pyi stub (the four core modules io/codec/klv/mpegts
# plus the six transport modules srt/rtp/udp/tcp/hls/rist) matches the live
# runtime surface via `mypy stubtest`. Build-dependent (needs a maturin-built
# tstrans + mypy in bindings/python/.venv), so it is EXCLUDED from the bare
# `find scripts/check` pre-push sweep's hard-fail contract: when the venv /
# mypy / built module are absent it prints SKIP and exits 0. CI runs it for
# real in the `python-core` job right after `maturin develop --release`
# (tst-py's transport features are default-on, so all ten modules import).
#
# History: core-only from v0.2.0 (#11) until 2026-09-08, when the transport
# stubs were brought under the rail — they had drifted to 294 findings
# (mostly missing @final / __init__-vs-__new__, plus real signature drift).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
VENV="$ROOT/bindings/python/.venv"
ALLOWLIST="$ROOT/scripts/ratchets/stubtest-allowlist.txt"
MODULES=(
  tstrans.io tstrans.codec tstrans.klv tstrans.mpegts
  tstrans.srt tstrans.rtp tstrans.udp tstrans.tcp tstrans.hls tstrans.rist
)

PY="$VENV/bin/python"
if [ ! -x "$PY" ] || ! "$PY" -c 'import mypy, tstrans' >/dev/null 2>&1; then
  echo "SKIP: bindings/python/.venv missing mypy or a built tstrans"
  echo "      (run: cd bindings/python && maturin develop --release && pip install mypy)"
  exit 0
fi

"$PY" -m mypy.stubtest "${MODULES[@]}" --allowlist "$ALLOWLIST"
echo "stubtest: all ${#MODULES[@]} tstrans modules OK"
