#!/usr/bin/env bash
# Local mirror of the CI QEMU runtime gate.
#
# Runs the tst-core muxer + tst-pipeline shells on both a Cortex-M4 (ARM,
# QEMU `mps2-an386`) and a RISC-V core (QEMU `virt`) and asserts the output
# byte-matches the committed video-roundtrip golden on each (see
# embedded/baremetal-qemu/). Each arch's QEMU binary is checked
# independently and skips gracefully when absent, so pre-push runs on
# machines missing one or both do not fail — same optional-tool pattern as
# the ffmpeg-dependent tests. `QEMU_RUNTIME_REQUIRE_TOOLS=1` (CI) makes a
# missing tool fail closed instead, per arch.
set -euo pipefail
cd "$(dirname "$0")/../../.."

# The one toolchain: rust-toolchain.toml's channel.
TOOLCHAIN="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' rust-toolchain.toml)"

REQUIRE="${QEMU_RUNTIME_REQUIRE_TOOLS:-0}"

# Add targets to the workspace toolchain (rust-toolchain.toml) — a bare
# `rustup target add` installs into the DEFAULT toolchain, which may differ.
rustup target add thumbv7em-none-eabihf --toolchain "$TOOLCHAIN" >/dev/null 2>&1 || true
rustup target add riscv32imac-unknown-none-elf --toolchain "$TOOLCHAIN" >/dev/null 2>&1 || true

run_target() {
  local tool="$1" target="$2" label="$3"
  if ! command -v "$tool" >/dev/null 2>&1; then
    if [ "$REQUIRE" = "1" ]; then echo "FATAL: required tool '$tool' not installed (QEMU_RUNTIME_REQUIRE_TOOLS=1)"; exit 1; fi
    echo "SKIP: $tool not installed ($label)"; return 0
  fi
  echo "==> QEMU runtime smoke: baremetal-qemu ($label)"
  # Build OUTSIDE the QEMU timeout (a cold build alone can eat the whole 60 s
  # budget), with the committed lockfile enforced (--locked) and the tuned
  # [profile.release] actually exercised (the debug profile left it dead config).
  ( cd embedded/baremetal-qemu && cargo build --release --locked --target "$target" )
  ( cd embedded/baremetal-qemu && timeout 60 cargo run --release --locked --target "$target" )
  echo "OK: tst-core muxer + tst-pipeline shells byte-match the video-roundtrip golden under QEMU ($label)"
}

run_target qemu-system-arm thumbv7em-none-eabihf "ARM Cortex-M4, mps2-an386"
run_target qemu-system-riscv32 riscv32imac-unknown-none-elf "RISC-V, virt"
