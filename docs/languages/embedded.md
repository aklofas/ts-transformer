# Embedded (no_std / bare-metal)

> **Who this is for:** You are building firmware that runs without an OS, or on a real-time OS like FreeRTOS, and need the MPEG-TS/KLV core — or full SRT video egress and ingress — inside the microcontroller, not in a host process.

> **You will learn:**
> - The three consumption paths (no_std Rust, C staticlib, FreeRTOS reference port)
> - Cargo snippet for pulling in the core without the standard library
> - What is and is not QEMU-gated in CI
> - The non-obvious gotchas before you start

## Overview

ts-transformer has three embedded consumption paths:

| Path | What you get | Reference |
|---|---|---|
| **no_std Rust** | `tst-core` + `tst-pipeline` sender and receiver shells, `#![no_std]` + `alloc`, runtime-gated under QEMU for two bare-metal targets | [`embedded/baremetal-qemu/`](/embedded/baremetal-qemu/) |
| **C firmware staticlib** | Offline mux/demux C ABI (the `tst-c-core` crate) bundled as `libtstrans_firmware.a` | [`embedded/baremetal-qemu-c/`](/embedded/baremetal-qemu-c/) |
| **FreeRTOS reference port** | Full libsrt + the MPEG-TS muxer/demuxer on FreeRTOS + FreeRTOS-Plus-POSIX + lwIP — SRT video egress AND ingress from/to a microcontroller, plain and AES-128 on egress | [`embedded/freertos-srt/`](/embedded/freertos-srt/) |

All three run under QEMU — no vendor hardware or BSPs are required or provided. Path 1 runs on both Cortex-M4 (`mps2-an386`) and RISC-V (`virt`); Paths 2 and 3 are Cortex-M4-only. See [`/docs/reference/compatibility.md`](/docs/reference/compatibility.md) for the platform-support tiers.

## Path 1 — no_std Rust

Pull in `tst-core` and the pipeline sender and receiver shells with the standard library disabled:

```toml
[dependencies]
tst-core     = { version = "0.7.0", default-features = false }
tst-pipeline = { version = "0.7.0", default-features = false }
```

`default-features = false` drops the `std` feature on both crates. What you get:

- `tst-core` — the full MPEG-TS mux/demux engine, KLV substrate and MISB typed sets, in-crate codec parameter-set parsers, and the `Transport`/`RecvTransport` traits. The `std` feature (default-on) gates the `net` helpers, file I/O, and `serde_json`/`toml` serialization — none of that is present in a `no_std` build.
- `tst-pipeline` sender and receiver shells — `MuxSender`, `Sender`, `RawSender`, `Receiver`, `DemuxReceiver`, and `RawReceiver`. The `Managed*` reconnect wrappers, `MuxPublisher`, and `ext::pairing` still require `std` and are not available in a `no_std` build. The receiver shells eagerly allocate a `transport.max_payload()`-sized scratch buffer at construction, and allocation failure aborts (no fallible-alloc path) — so on a device with a fixed heap, `max_payload()` is effectively a heap-sizing decision, not just a wire-protocol ceiling. The crate's internal 256 MiB clamp on that allocation is a sanity bound against a hostile/misbehaving `Transport`, not a substitute for budgeting the allocation against your actual device heap.

### QEMU reference

[`embedded/baremetal-qemu/`](/embedded/baremetal-qemu/) is the runnable reference. It exercises four checks under QEMU, on both a Cortex-M4 and a RISC-V core (the same source, unmodified, on two unrelated instruction sets):

1. Bare `Muxer` — mux the `video-roundtrip` fixture and byte-compare to the committed golden.
2. `MuxSender` over an in-memory `Vec`-backed transport — same golden, proves the pipeline shell runs on bare metal.
3. `MuxSender` through a real `no_std` smoltcp IPv4/UDP stack over a loopback device.
4. `DemuxReceiver` over its own dedicated smoltcp loopback `RecvTransport` — proves the receiver-path `no_std` shells run correctly on target.

```bash
# Run locally (needs qemu-system-arm + qemu-system-misc + thumbv7em-none-eabihf + riscv32imac-unknown-none-elf targets):
cd embedded/baremetal-qemu
cargo run --release --locked                                       # ARM
cargo run --release --locked --target riscv32imac-unknown-none-elf # RISC-V

# Or via the CI gate script (skips cleanly, per-target, if the matching QEMU binary is absent):
bash embedded/scripts/check/qemu-runtime.sh
```

### Gotchas

- **`std` is on by default.** A bare `tst-core = { version = "0.7.0" }` pulls in the standard library. Always add `default-features = false` for bare-metal targets — the compiler will reject the build if you forget (the `*-none-*` target has no `std`).
- **Final binaries need a `#[global_allocator]`.** Both crates are `#![no_std]` + `alloc`, which requires a heap. Library crates that consume `tst-core` do not need to provide one; the final binary (your firmware) does. `embedded/baremetal-qemu/` uses `embedded-alloc` + `cortex-m-rt` as one approach.
- **The `embedded/` tree is workspace-excluded on purpose.** The sub-projects pin bare-metal targets and profiles that are incompatible with a host-target workspace build. They consume the workspace crates by path and carry their own committed `Cargo.lock` files; CI builds them with `--locked`.
- **One sender per task.** Under `no_std` the pipeline shells' internal lock is a spin lock with no priority inheritance and no interrupt masking. Drive each `MuxSender`/`Sender`/`RawSender` — and, in the C firmware path, each `tst_*` handle — from a single task. Sharing one across preemptive tasks can livelock a higher-priority task against a preempted lock-holder.
- **The C ABI's last-error slot is process-global under `no_std`.** The `std` build keeps a per-thread slot; the `no_std` build keeps one global slot behind a critical section. In multi-task firmware, read `tst_get_last_error()`/`tst_get_last_error_str()` immediately after the failing call — any task's next tst-c call may overwrite them.

## Path 2 — C firmware staticlib

[`embedded/baremetal-qemu-c/`](/embedded/baremetal-qemu-c/) shows how to link the offline C ABI surface into C firmware. It builds `tst-c-core` as a `no_std` staticlib (`libtstrans_firmware.a`) and provides the three embedder-supplied pieces that `tst-c-core` deliberately omits:

- `#[global_allocator]` forwarding to the C firmware's newlib heap (`memalign`/`free`)
- `#[panic_handler]` calling `abort()`
- The `critical-section` single-core implementation

The resulting archive exposes the offline mux/demux C ABI — config, push, pull, event, error — usable from plain C firmware with no Rust toolchain in the firmware build system itself (only in the glue-crate build step).

Network transports (SRT, RTP, UDP, TCP, RIST) and the HLS publisher are host-only; they require `std` and are not available through `tst-c-core`.

```bash
bash embedded/scripts/check/firmware-qemu.sh
```

## Path 3 — FreeRTOS reference port

[`embedded/freertos-srt/`](/embedded/freertos-srt/) is the flagship: a complete reference port of **libsrt** + the MPEG-TS muxer/demuxer onto FreeRTOS + FreeRTOS-Plus-POSIX + lwIP. It demonstrates SRT video egress AND ingress from/to a microcontroller (`example` streams out; `srt-recv` receives in and demuxes on-device), including byte-exact ARQ recovery under ~20% packet loss and AES-128 encryption on egress.

It is **not built by default** — it is not a Cargo workspace member, and its gates skip locally when the cross-toolchain or QEMU is absent. In CI, every `freertos-srt` target runs as a fail-closed hard gate under QEMU.

```bash
# From the workspace root:
bash embedded/scripts/check/freertos-srt.sh loopback-arq        # SRT byte-exact + AES-128
bash embedded/scripts/check/freertos-srt.sh malloc-stress       # 4 tasks × 20000 malloc/free + per-task errno isolation
bash embedded/scripts/check/freertos-srt.sh fault-smoke         # deliberate fault → labeled FAIL + fast exit
```

Gate targets:

| Target | Proves |
|---|---|
| `exceptions` | Per-task C++ exceptions isolated on FreeRTOS |
| `lwip-loopback` | FreeRTOS + lwIP substrate round-trips a UDP golden |
| `libsrt-smoke` | Cross-compiled libsrt boots on the substrate |
| `loopback-arq` | SRT recovers byte-exact under ~20% loss, plain and AES-128 |
| `arq-connfail` | Caller at a dead port fails fast with a labeled verdict |
| `fault-smoke` | HardFault/assert prints labeled `FAIL[…]` and exits, not hangs |
| `malloc-stress` | 4 tasks × 20000 malloc/free + concurrent exception handling + per-task errno isolation |
| `example` | Real-NIC SRT caller streams the golden to a host listener, plain and AES-128 |
| `srt-recv` | Real-NIC SRT listener receives from a host caller and demuxes the golden on-device, byte-exact (the reverse of `example`) |

### Production crypto warning

> **The AES-128 path in this reference port uses _deterministic_ entropy.** The `ENCRYPT=1` builds wire mbedTLS, but the entropy hooks are a fixed-seed LCG chosen for QEMU/CI reproducibility, compiled in only under `-DTST_QEMU_TEST_ENTROPY=1`. **This is not cryptographically secure.** Before enabling SRT encryption in production firmware, replace both hooks with a hardware RNG or your board's approved entropy source — a build that omits the define fails at link on the missing hooks rather than silently linking predictable key material. See the warning in [`/embedded/freertos-srt/README.md`](/embedded/freertos-srt/README.md) for exactly which hooks to replace.

## Prerequisites

```bash
git submodule update --init --recursive   # embedded/vendor/* + crates/srt-sys/vendor/srt + crates/mbedtls-src/vendor/mbedtls
sudo apt install qemu-system-arm qemu-system-misc gcc-arm-none-eabi libnewlib-arm-none-eabi \
                 libstdc++-arm-none-eabi-newlib cmake python3
rustup target add thumbv7em-none-eabihf riscv32imac-unknown-none-elf
```

## Where to go next

- [`/embedded/README.md`](/embedded/README.md) — sub-project layout, all gate commands, and fatal-path diagnostics summary.
- [`/embedded/baremetal-qemu/README.md`](/embedded/baremetal-qemu/README.md) — Rust no_std runtime reference, check sequence.
- [`/embedded/baremetal-qemu-c/README.md`](/embedded/baremetal-qemu-c/README.md) — C staticlib glue reference.
- [`/embedded/freertos-srt/README.md`](/embedded/freertos-srt/README.md) — FreeRTOS reference port internals and newlib locking.
- [`/docs/reference/compatibility.md`](/docs/reference/compatibility.md) — platform-support tiers, including the bare-metal Tier 2 entries.
