# `embedded/` — bare-metal & RTOS support

Self-contained embedded sub-project: firmware-side proofs and reference ports
that exercise the workspace's `no_std` surface on microcontroller targets. All
of it runs under QEMU — no hardware required.

> **Most consumers do not need this directory.** If you run on an OS, use the
> Rust crates or the C/Python/JVM bindings. This tree is for the narrow case of
> running the MPEG-TS/KLV core — and optionally full SRT — directly in
> bare-metal or FreeRTOS firmware.

## Sub-projects

| Directory | What it proves | Stack |
|---|---|---|
| `baremetal-qemu/` | the `tst-core` muxer + `tst-pipeline` `MuxSender`/`DemuxReceiver` run `no_std` on **both ARM (Cortex-M4) and RISC-V** and byte-match the committed golden, including smoltcp UDP loopback egress and ingress | Rust `no_std`, QEMU `mps2-an386` (ARM) + `virt` (RISC-V) |
| `baremetal-qemu-c/` | the offline C ABI (`tst-c-core`) works from C firmware via a `no_std` staticlib (`libtstrans_firmware.a`) | Rust staticlib + arm-none-eabi C firmware, QEMU |
| `freertos-srt/` | **reference product**: libsrt + the muxer/demuxer on FreeRTOS + FreeRTOS-Plus-POSIX + lwIP — SRT video egress AND ingress from/to a microcontroller (plain and AES-128 on egress; on-device demux verification on ingress) | C/C++ substrate, arm-none-eabi GCC, QEMU |

## Layout

- `vendor/` — embedded-only submodules: `freertos-kernel`, `freertos-plus-posix`,
  `lwip`. (The shared `srt` and `mbedtls` submodules live at `crates/srt-sys/vendor/srt`
  and `crates/mbedtls-src/vendor/mbedtls` — `srt-sys` builds them for host targets too.)
- `scripts/check/` — the embedded CI gate scripts (below).
- `scripts/lib/` — helpers shared by the gates.

The two `baremetal-qemu*` projects are excluded from the cargo workspace (they
pin bare-metal targets and profiles) and consume the workspace crates by path
(`crates/tst-core`, `crates/tst-pipeline`, `bindings/c/core`).

## Prerequisites

```bash
git submodule update --init --recursive   # embedded/vendor/* + crates/{srt-sys,mbedtls-src}/vendor/*
sudo apt install qemu-system-arm qemu-system-misc gcc-arm-none-eabi libnewlib-arm-none-eabi \
                 libstdc++-arm-none-eabi-newlib cmake python3
rustup target add thumbv7em-none-eabihf riscv32imac-unknown-none-elf
```

(`qemu-system-arm` provides the `mps2-an386` ARM runner; `qemu-system-misc`
provides `qemu-system-riscv32` for the `virt` RISC-V runner — both used by
`baremetal-qemu/`'s two-architecture QEMU runtime gate below.)

## Running the gates

All gates run from the workspace root. Missing tools skip cleanly by default;
CI hard-gates fail closed instead via per-script env knobs
(`FREERTOS_SRT_REQUIRE_TOOLS=1`, `FIRMWARE_QEMU_REQUIRE_TOOLS=1`,
`QEMU_RUNTIME_REQUIRE_TOOLS=1`):

```bash
bash embedded/scripts/check/no-std-baremetal.sh   # no_std compile proof (3 crates x 2 targets)
bash embedded/scripts/check/qemu-runtime.sh       # baremetal-qemu golden byte-match under QEMU (ARM + RISC-V)
bash embedded/scripts/check/firmware-qemu.sh      # C firmware via libtstrans_firmware.a
bash embedded/scripts/check/freertos-srt.sh exceptions     # C++ exceptions on FreeRTOS
bash embedded/scripts/check/freertos-srt.sh lwip-loopback  # lwIP UDP loopback round-trip
bash embedded/scripts/check/freertos-srt.sh libsrt-smoke   # cross-built libsrt boots
bash embedded/scripts/check/freertos-srt.sh loopback-arq      # SRT ARQ + AES-128 over a lossy netif
bash embedded/scripts/check/freertos-srt.sh arq-connfail      # caller at dead port fails fast with labeled verdict
bash embedded/scripts/check/freertos-srt.sh example           # NIC egress to a host listener
bash embedded/scripts/check/freertos-srt.sh srt-recv          # NIC ingress from a host caller, demuxed + verified on-device
bash embedded/scripts/check/freertos-srt.sh fault-smoke       # deliberate fault produces labeled FAIL token + fast exit (gate asserts the failure)
bash embedded/scripts/check/freertos-srt.sh malloc-stress     # 4 tasks × 20000 malloc/free + EH + errno isolation
```

Each sub-project's own README covers internals and design rationale.

## Fatal-path diagnostics

Every fatal path in `freertos-srt/` prints a labeled token and exits non-zero
via semihosting, rather than hanging silently. Fault handlers (HardFault /
UsageFault / BusFault / MemManage) emit `FAIL[hardfault] pc=0x…` including the
stacked PC and LR; `configASSERT` fires `FAIL[assert]` with file and line;
task/thread creation failures print `FAIL[task-…]` and exit immediately.
All output uses direct ARM semihosting `SYS_WRITE0` — no newlib stdio, no heap,
no locks — so the path is safe from fault context and from pre-scheduler
initialisation. Because QEMU routes semihosting writes to stderr, the gate
scripts fold stderr into the captured transcript (`2>&1`), and the `fault-smoke`
gate asserts the expected `FAIL[hardfault]` token rather than a `PASS` token.

## Newlib locking and per-task reentrancy

`freertos-srt/` enables `configUSE_NEWLIB_REENTRANT = 1` so FreeRTOS allocates a
separate `struct _reent` for each task. This makes `errno` and the C library's
internal file-pointer state task-local, eliminating cross-task bleed under
preemption. The xpack `arm-none-eabi` newlib is built with `_RETARGETABLE_LOCKING`:
the library references `__retarget_lock_*` symbols and ships no-op archive
fallbacks. `substrate/newlib_lock.c` provides strong definitions that back each
lock with a FreeRTOS recursive mutex, so `malloc`/`free`, `stdio`, and `env`
become fully preemption-safe. Before `vTaskStartScheduler()`, the scheduler is
not running and all acquire/release operations are no-ops — the same pattern as
`pthread_key_shim.c`.

## Third-party components

This sub-project uses the following third-party libraries, vendored as git
submodules. Licenses are in each submodule tree.

| Component | Submodule path | License |
|---|---|---|
| FreeRTOS Kernel | `embedded/vendor/freertos-kernel` | MIT |
| FreeRTOS-Plus-POSIX | `embedded/vendor/freertos-plus-posix` | MIT |
| lwIP | `embedded/vendor/lwip` | BSD-3-Clause |
| libsrt | `crates/srt-sys/vendor/srt` | MPL-2.0 |
| Mbed TLS | `crates/mbedtls-src/vendor/mbedtls` | Apache-2.0 OR GPL-2.0-or-later |

## Generated artifacts

All generated output (firmware ELF, object files, intermediate libraries) lives
under each sub-project's gitignored `build/` or `target/` directory; nothing
generated is committed. The two golden header files (`golden.h`) used by
`baremetal-qemu-c/firmware/` and `freertos-srt/tests/` derive from the
committed fixture at
`crates/tst-integration/tests/fixtures/scenarios/video-roundtrip/output.ts`
via `embedded/scripts/lib/gen-golden-h.sh`. The three Rust sub-projects
(`baremetal-qemu`, `baremetal-qemu-c`, `freertos-srt/example/host`) commit
their `Cargo.lock` files, and the CI gates build with `--locked` to reproduce
the exact dependency tree.
