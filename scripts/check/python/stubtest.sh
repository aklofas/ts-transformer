#!/usr/bin/env bash
# Verifies EVERY tst-py .pyi stub (the four core modules io/codec/klv/mpegts
# plus the six transport modules srt/rtp/udp/tcp/hls/rist) matches the live
# runtime surface via `mypy stubtest`. Build-dependent (needs a maturin-built
# tstrans + mypy in bindings/python/.venv), so it is EXCLUDED from the bare
# `find scripts/check` pre-push sweep's hard-fail contract: when no venv with
# mypy exists it prints SKIP and exits 0. CI runs it for real in the
# `python-core` job right after `maturin develop --release` (tst-py's
# transport features are default-on, so all ten modules import).
#
# History: core-only from v0.2.0 (#11) until 2026-09-08, when the transport
# stubs were brought under the rail — they had drifted to 294 findings
# (mostly missing @final / __init__-vs-__new__, plus real signature drift).
#
# 2026-09-10: worktree-aware. A linked `git worktree` has no
# bindings/python/.venv, so this rail used to print SKIP there and the
# pre-push sweep passed vacuously. Now:
#   - the interpreter (mypy) falls back to the MAIN checkout's venv when this
#     tree has none;
#   - `tstrans` is always imported from THIS tree (PYTHONPATH), so the stubs
#     under test are the ones being pushed, never the main checkout's;
#   - the native module must be built in (or copied into) this tree — a venv
#     without a `_native*.so` here FAILS with the recipe instead of skipping;
#   - a native module older than the Rust sources gets a WARNING (the venv's
#     build is whatever `maturin develop` last produced; a stale one turns
#     real drift into a false failure, or masks it).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
PKG="$ROOT/bindings/python/python"
ALLOWLIST="$ROOT/scripts/ratchets/stubtest-allowlist.txt"
MODULES=(
  tstrans.io tstrans.codec tstrans.klv tstrans.mpegts
  tstrans.srt tstrans.rtp tstrans.udp tstrans.tcp tstrans.hls tstrans.rist
)

# Interpreter: this tree's venv, else the main checkout's (linked worktree).
PY="$ROOT/bindings/python/.venv/bin/python"
if [ ! -x "$PY" ]; then
  common="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  main_root="${common%/.git}"
  if [ -n "$common" ] && [ "$main_root" != "$ROOT" ] \
     && [ -x "$main_root/bindings/python/.venv/bin/python" ]; then
    PY="$main_root/bindings/python/.venv/bin/python"
    echo "stubtest: linked worktree, using the main checkout's venv: $PY"
  fi
fi
if [ ! -x "$PY" ] || ! "$PY" -c 'import mypy' >/dev/null 2>&1; then
  echo "SKIP: no venv with mypy at bindings/python/.venv (this tree or the main checkout)"
  echo "      (run: cd bindings/python && python -m venv .venv && .venv/bin/pip install maturin mypy"
  echo "            && .venv/bin/maturin develop --release)"
  exit 0
fi

# Native module: must live in THIS tree. PYTHONPATH below makes `tstrans`
# resolve here, so a build from another checkout is never silently substituted.
native=""
for f in "$PKG"/tstrans/_native*.so "$PKG"/tstrans/_native*.pyd; do
  [ -f "$f" ] && { native="$f"; break; }
done
if [ -z "$native" ]; then
  echo "FAIL: no built tstrans native module under $PKG/tstrans/"
  echo "      build it in this tree:  cd bindings/python && maturin develop --release"
  echo "      (stub-only change in a worktree: copy _native.abi3.so from the main"
  echo "       checkout's bindings/python/python/tstrans/ — the rail then checks"
  echo "       THIS tree's stubs against that runtime)"
  exit 1
fi

# Staleness: the Rust inputs feeding the module (tst-py's own crate incl.
# its Cargo.toml/build.rs, every library crate, the lockfile) are newer than
# the build.
newer=$(find "$ROOT/bindings/python" "$ROOT/crates" "$ROOT/Cargo.lock" \
          \( -path '*/vendor' -o -path '*/.venv' -o -path '*/target' \) -prune -o \
          \( -name '*.rs' -o -name Cargo.toml -o -name Cargo.lock \) -newer "$native" -print 2>/dev/null | wc -l)
if [ "$newer" -gt 0 ]; then
  echo "WARNING: $native is older than $newer Rust source file(s);"
  echo "         if the findings below look like drift, rebuild first:"
  echo "         cd bindings/python && maturin develop --release"
fi

resolved=$(PYTHONPATH="$PKG" "$PY" -c 'import os, tstrans; print(os.path.dirname(tstrans.__file__))')
if [ "$resolved" != "$PKG/tstrans" ]; then
  echo "FAIL: tstrans resolved to $resolved, expected $PKG/tstrans"
  exit 1
fi

PYTHONPATH="$PKG" "$PY" -m mypy.stubtest "${MODULES[@]}" --allowlist "$ALLOWLIST"
echo "stubtest: all ${#MODULES[@]} tstrans modules OK ($native)"
