#!/usr/bin/env bash
# Build the C firmware (links libtstrans_firmware.a = tst-c-core's offline C ABI
# built no_std) and run it under QEMU mps2-an386, asserting it byte-matches the
# committed video-roundtrip golden through the C ABI and that demuxing that
# output back yields typed event structs that validate field-by-field
# (32-bit struct-crossing). Skips when the ARM C
# cross-toolchain or QEMU is absent (same optional-tool pattern as
# embedded/scripts/check/qemu-runtime.sh).
set -euo pipefail
cd "$(dirname "$0")/../../.."

# The one toolchain: rust-toolchain.toml's channel.
TOOLCHAIN="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' rust-toolchain.toml)"

# Skip when a tool is absent (local convenience) — or fail closed under
# FIRMWARE_QEMU_REQUIRE_TOOLS=1 (the CI hard-gate sets it so a runner-image
# regression that drops a tool goes red instead of silently skipping to a
# green no-op). Mirrors freertos-srt.sh's FREERTOS_SRT_REQUIRE_TOOLS.
REQUIRE="${FIRMWARE_QEMU_REQUIRE_TOOLS:-0}"
need() { # $1=binary  $2=install hint
  command -v "$1" >/dev/null 2>&1 && return 0
  if [ "$REQUIRE" = "1" ]; then echo "FATAL: required tool '$1' not installed ($2) (FIRMWARE_QEMU_REQUIRE_TOOLS=1)"; exit 1; fi
  echo "SKIP: $1 not installed ($2)"; exit 0
}
need arm-none-eabi-gcc "apt install gcc-arm-none-eabi libnewlib-arm-none-eabi"
need qemu-system-arm   "apt install qemu-system-arm"
need python3           "apt install python3"

CRATE=embedded/baremetal-qemu-c
FW="$CRATE/firmware"
GOLDEN_TS=crates/tst-integration/tests/fixtures/scenarios/video-roundtrip/output.ts

rustup target add thumbv7em-none-eabihf --toolchain "$TOOLCHAIN" >/dev/null 2>&1 || true

echo "==> generating golden.h from $GOLDEN_TS"
bash embedded/scripts/lib/gen-golden-h.sh "$GOLDEN_TS" "$FW/golden.h"

echo "==> building glue staticlib (thumbv7em)"
( cd "$CRATE" && cargo build --release --locked )
# Deterministic path: --release above + target pinned to thumbv7em-none-eabihf
# in the crate's .cargo/config.toml. (A `find | head -1` would walk debug/
# first and could link a stale debug archive from a prior `cargo build`.)
AR="$CRATE/target/thumbv7em-none-eabihf/release/libtstrans_firmware.a"
ARDIR=$(cd "$(dirname "$AR")" && pwd)
INC=$(cd bindings/c/include && pwd)

echo "==> compiling + linking firmware.elf"
# -z noexecstack: newlib's crtn.o ships without a .note.GNU-stack section, and
# binutils >= 2.39 warns "missing .note.GNU-stack section implies executable
# stack" whenever it has to infer the stack flags. Stating them explicitly is
# a no-op for a bare-metal image and silences the (deprecated-behaviour) warning.
# --no-warn-rwx-segments: the same binutils release also warns when a LOAD
# segment is RWX, which mps2_an386.ld's single flash+RAM image is by design
# (toolchains built with the warning enabled — e.g. xPack 14.x — show it;
# Ubuntu's arm-none-eabi build does not).
( cd "$FW" && arm-none-eabi-gcc \
    -mcpu=cortex-m4 -mthumb -mfloat-abi=hard -mfpu=fpv4-sp-d16 \
    -Os -ffunction-sections -fdata-sections --specs=nano.specs --specs=rdimon.specs \
    -T mps2_an386.ld -Wl,--gc-sections -Wl,-z,noexecstack -Wl,--no-warn-rwx-segments -I "$INC" \
    startup.c main.c \
    -Wl,--start-group "-L$ARDIR" -ltstrans_firmware -lc -lm -lrdimon -Wl,--end-group \
    -o firmware.elf )

echo "==> running firmware under QEMU (mps2-an386)"
# Use `if out=$(...)` so a non-zero firmware exit (FAIL path / semihosting
# _exit(1)) is captured rather than killing the script via `set -e`, while
# still preserving rc for the failure message. stderr is folded in (2>&1) so
# that SYS_WRITE0 diagnostic output goes into OUT alongside stdout.
OUT_RC=0
T0=$(date +%s)
if OUT=$(timeout 60 qemu-system-arm -machine mps2-an386 -nographic \
  -semihosting-config enable=on,target=native -kernel "$FW/firmware.elf" 2>&1); then
  OUT_RC=0
else
  OUT_RC=$?
fi
T1=$(date +%s)
echo "$OUT"
if ! echo "$OUT" | grep -q 'PASS: c_firmware ('; then
  echo "GATE FAILED — qemu rc=$OUT_RC, elapsed=$((T1 - T0))s of 60s budget"
  echo "  (rc=124 + full budget = hang/timeout; fast nonzero rc = a labeled FAIL[...] exit — read the transcript)"
  exit 1
fi
echo "OK: C firmware muxer byte-matches the golden + demux struct-crossing checks pass under QEMU"
