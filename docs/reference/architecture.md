# Architecture

## Introduction

This document covers how `ts-transformer` is laid out: the crate graph, the
internal structure of `tst-core`, and the pipeline composition model that
ties the muxer, transport, and reconnect behaviour together. It targets
evaluators sizing up the project and contributors finding their way around;
integrators may want it as background but should start at
[start/quickstart.md](/docs/start/quickstart.md) if they just want to use the
library.

The vocabulary established here — `Transport`, `RecvTransport`, "sender
shell", "receive shell", the layering rule — is reused by the per-module
guides ([guides/srt.md](/docs/guides/srt.md), [guides/klv.md](/docs/guides/klv.md),
[guides/mpegts-mux.md](/docs/guides/mpegts-mux.md),
[guides/mpegts-demux.md](/docs/guides/mpegts-demux.md),
[guides/pipeline.md](/docs/guides/pipeline.md)). Read this first if you plan to
read more than one of those.

## Repository layout

The workspace is organized by role, not by a flat crate list. Each top-level
directory owns one concern:

| Directory | Owns | Notes |
|---|---|---|
| `crates/` | The pure-Rust core: library + transports + test-infra | `srt-sys` (published as `tstrans-srt-sys`), `rist-sys` (published as `tstrans-rist-sys`) (raw FFI); `tst-core` (engine); `tst-pipeline` (shells); the transports `tst-srt` / `tst-rtp` / `tst-udp` / `tst-tcp` / `tst-rist`; `tst-integration`, `tst-test-helpers` (test infra) |
| `bindings/` | Language bindings for downstream consumers | `bindings/c` (crate `tst-c` — cdylib/staticlib + `include/tstrans.h`) with its embeddable rlib at `bindings/c/core` (crate `tst-c-core`); `bindings/python` (crate `tst-py`); `bindings/jvm` (crate `tst-jni`); `bindings/apple-android` planned |
| `embedded/` | Bare-metal / QEMU firmware test harnesses (workspace-excluded) | `baremetal-qemu` (no_std muxer/pipeline QEMU smoke), `baremetal-qemu-c` (C-firmware staticlib glue), `freertos-srt` (libsrt-on-FreeRTOS) |
| `examples/` | Runnable Rust examples (crate `tst-examples`, `publish = false`) | Task-oriented subfolders; C examples mirror this taxonomy under `bindings/c/examples/` |
| `vendor/` (workspace root) | No longer used for `srt`/`librist`/`mbedtls` — each now bundles its native source inside its owning crate (crates.io requires a package to be self-contained) | `crates/srt-sys/vendor/srt` (libsrt 1.5.7), `crates/rist-sys/vendor/librist`, `crates/mbedtls-src/vendor/mbedtls` (3.6.7 LTS) |
| `scripts/` | CI ratchets + generators + dev tools | `check/{c,python,rust,embedded,repo}/` rails, `gen/` generators, `dev/` tools, plus `ratchets/` (TSV-driven coverage) and `lib/` |
| `tests/` | Cross-cutting advisory control plane | `tests/coverage/` manifests (fixture/skip-ledger/stream-matrix) |
| `oss-fuzz/` | OSS-Fuzz packaging (options + seed corpora) | Per-crate fuzz targets live in `crates/<c>/fuzz/` |

The boundary is: `crates/` is everything a Rust consumer would depend on; `bindings/`
is everything a non-Rust consumer links against; `embedded/` is firmware test
scaffolding that is not a workspace member. All `bindings/*` and `examples` remain
Cargo workspace members; `embedded/*` are `exclude`d (separate build roots).

## Crate graph

```
srt-sys (raw FFI)  ──→  tst-core  ──→  tst-c (cdylib + staticlib + cbindgen)
                              │
                              ├──→  tst-pipeline (pipeline shells)
                              ├──→  tst-srt      (SRT transports)
                              ├──→  tst-jni      (JVM JNI bindings)
                              └──→  tst-uniffi   (planned)

dev-only: tst-test-helpers (publish = false; shared test fixtures and
helpers consumed by tst-core / tst-pipeline / tst-srt test suites)
vendored (bundled per-crate, not at the workspace root): crates/srt-sys/vendor/srt
(libsrt 1.5.7), crates/rist-sys/vendor/librist, crates/mbedtls-src/vendor/mbedtls
(3.6.7 LTS)
```

The layering rule is one-directional: lower layers do not depend on upper
layers. Binding crates (`tst-c`, `tst-jni`, `tst-uniffi`) depend on
`tst-pipeline` + `tst-srt` (and transitively `tst-core`) — never on
`srt-sys` directly. This keeps every binding's surface area defined by
the same safe Rust API and means a fix in `tst-core` reaches every
binding without per-binding patches.

`srt-sys` (published as `tstrans-srt-sys`) is the raw FFI layer — bindgen-generated against libsrt 1.5.7,
exposing roughly 72 `srt_*` functions and the full `SRT_*` constant
surface, with `mbedtls` wired in as the encryption backend by default.
`tst-core` is the safe Rust API; nothing above it should ever pull
`srt-sys` into its dependency graph. The vendored libsrt and mbedTLS
submodules are pinned by tag (`v1.5.7`, `v3.6.7` LTS); submodule advances
are deliberate, separate commits. Both vendored builds link statically,
so `tst-c`'s shared library has no runtime dependency on a system libsrt
or libmbedtls.

## Inside `tst-core`: four modules

- `klv::*` — KLV codec, generic substrate plus typed ST 0601 / ST 0605 / ST 0102 (sibling-layer Security LS) / ST 0903 (sibling-layer VMTI LS — top-level + per-target `VTargetPack`; nested LSes pass-through) layers.
- `mpegts::mux::*` — sender-side MPEG-TS muxer for H.264 / H.265 / H.266 / AV1 video + audio (MP2 / AAC ADTS / AAC LATM / AC-3) + subtitles (DVB-sub / DVB-teletext / CEA-708 / WebVTT-in-TS) + KLV.
- `mpegts::demux::*` — receiver-side MPEG-TS demuxer; bytes in, typed `DemuxEvent` out.
- `codec::*` — typed parameter-set parsers for H.264 / H.265 / H.266 / AV1 plus audio frame iterators for `mpegaudio` / `aac::adts`.

Each module is independently usable. A consumer who only needs KLV decode
can pull in `tst-core` and use `klv::st0601::decode` without touching the
muxer or transport. A consumer who already has their own TS muxer can
skip `mpegts::mux` and feed bytes through `tst_pipeline::Sender`. A
consumer who only wants the SRT socket — for an entirely different
streaming protocol on top — can use `tst_srt::Socket` and `tst_srt::Listener`
directly.

## Inside `tst-srt`

The SRT-specific surface lives in its own crate so `tst-core` stays
free of any libsrt dependency. `tst-srt` re-exports the safe wrappers
at the crate root: `Socket` / `Listener` (connected and listening
sockets), `SocketBuilder` / `ListenerBuilder` (fluent construction over
`SocketConfig` / `ListenerConfig`), `SrtTransport` (the canonical
`Transport` + `RecvTransport` impl), `SrtCancelHandle` (one-shot
pre-emptive close), and `Stats` (live snapshot of libsrt's internal
counters). The `url::SrtUrl` type parses `srt://host:port?key=value&…`
into a builder overlay using libsrt's documented option vocabulary,
and `addr::*` handles IPv4 / IPv6 sockaddr marshalling. See
[guides/srt.md](/docs/guides/srt.md) for the full surface.

`tst-pipeline` is the composition layer that depends on the other crates.
It is deliberately thin: its job is composition, not new behaviour. The
send shells delegate framing to `mpegts::mux::Muxer` and wire transport to
a `Transport` implementation; the receive shells delegate framing recovery
to `mpegts::demux::Demuxer` and bytes-in to a `RecvTransport`
implementation. Metadata typing for both directions is `klv::st0601` /
`klv::st0605`. MPEG-TS sync-metadata AU cell carriage lives at
`mpegts::au_cell` (per ITU-T H.222.0 V9 § 2.12.4.2). The canonical
`Transport` + `RecvTransport` impl is `SrtTransport` in `tst-srt` over
`tst_srt::Socket` — the same wrapper handles both directions on a connected
socket. The opt-in `tst_pipeline::ext::pairing` module ships a stateful `Pairer` for
KLV ↔ video pairing — see `docs/guides/pipeline.md` for the
nearest-PTS / sample-and-hold strategy chooser.

## The pipeline composition model

```
                   ┌────────────────────────────────┐
                   │  MuxSender / Sender / RawSender │  (3 sender shells)
                   │  generic over T: Transport      │
                   └──────────────┬─────────────────┘
                                  │ T: Transport
              ┌───────────────────┴───────────────────┐
              │  ManagedTransport<T>  (decorator)     │  (optional)
              │  reconnect + gap buffer               │
              └───────────────────┬───────────────────┘
                                  │ T: Transport
              ┌───────────────────┴───────────────────┐
              │  SrtTransport (canonical)             │
              │  Custom Transport impl (yours)        │
              └───────────────────────────────────────┘
```

Any sender shell composes with any `Transport` implementation. The shells
differ by what they accept on the input side — NAL units plus KLV blobs
(`MuxSender`), pre-muxed TS bytes (`Sender`), or arbitrary byte-blind
messages (`RawSender`). The transport differs by what it talks to on the
output side — SRT (`SrtTransport`), a custom UDP socket, a file, an
in-memory buffer for tests. The two axes are orthogonal: you pick a shell
based on what you have, a transport based on where it's going, and they
plug together.

`ManagedTransport<T>` is itself a `Transport` implementation — it
implements the trait by delegating to an inner `T` and adding reconnect
plus a gap buffer that holds messages while the inner transport is down.
Because it satisfies `Transport`, it slots between any sender shell and
the underlying transport without the shell needing to know about
reconnect at all. The shell sees a `Transport`; whether that `Transport`
is plain `SrtTransport` or `ManagedTransport<SrtTransport>` is a
construction-time choice.

For worked examples, see [examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs)
(reconnect + gap buffer in action) and
[examples/sending/custom_transport.rs](/examples/sending/custom_transport.rs)
(implementing the `Transport` trait against something other than SRT).

## Why three sender shells

Decision tree:

- You have NAL units plus KLV blobs → `MuxSender` (auto-muxes through `Muxer`, internally synchronized).
- You have pre-muxed TS bytes → `Sender` (3-byte sync verify, 7-packet bundling, RECOVER or STRICT framing mode).
- You have arbitrary byte-blind messages → `RawSender` (one `send` call = one outbound SRT message).

There are three shells rather than one because the contracts differ.
`MuxSender` enforces NAL unit boundaries and KLV record boundaries — it
needs to know where one ends and the next begins to mux them into a TS
stream correctly. `Sender` enforces TS sync alignment — it needs to
know where each 188-byte packet starts so it can bundle them into SRT
messages of the right shape. `RawSender` enforces nothing beyond the
`SRTO_PAYLOADSIZE` size cap — the caller has already framed the bytes.
Fusing the three into one API would force a least-common-denominator
contract that satisfies none of the use cases well. The shells also
differ in what they do on transient failure. `MuxSender` carries an
in-flight buffer and replays buffered bytes on reconnect, so a brief
outage is invisible to the receiver above the transport gap-buffer
window. `Sender` re-establishes sync alignment on the byte stream
after a transport failure — RECOVER mode auto-resyncs to the next sync
byte, STRICT mode fails fast. `RawSender` has no recovery contract by
construction; one `send` either lands as one SRT message or returns an
error. The full mechanics of each shell are covered in
[guides/pipeline.md](/docs/guides/pipeline.md).

## The receive pipeline

```
              ┌───────────────────────────────────────┐
              │  RawReceiver  /  Receiver             │
              │  DemuxReceiver  (full demux)          │
              │  generic over R: RecvTransport        │
              └──────────────┬────────────────────────┘
                             │ R: RecvTransport
              ┌──────────────┴────────────────────────┐
              │  ManagedRecvTransport<R>              │  (optional)
              │  reconnect on Closed/Broken           │
              └──────────────┬────────────────────────┘
                             │ R: RecvTransport
              ┌──────────────┴────────────────────────┐
              │  SrtTransport (canonical)             │
              │  Custom RecvTransport impl (yours)    │
              └───────────────────────────────────────┘
```

The receive side mirrors the send side. Three shells differ by what
they emit: `RawReceiver::recv_one` returns one byte vec per call (no
TS framing), `Receiver::next_packet` emits one 188-byte aligned TS
packet per call (sync recovery internal), `DemuxReceiver::recv_event`
emits one typed `DemuxEvent` per call (full TS sync + PSI parse + PES
reassembly + NAL split + KLV unwrap). All three are generic over the
`RecvTransport` trait — the receive-side counterpart to `Transport`,
exposing `recv_bytes`, `max_payload`, `is_alive`, and `close`.
`SrtTransport` implements both `Transport` and `RecvTransport`, so the
same wrapper handles both directions on a connected `tst_srt::Socket`.

`DemuxReceiver` is the canonical full-demux shell. It composes
`Receiver → Demuxer` internally and exposes a single `recv_event`
draining loop. It also implements `Iterator<Item = Result<DemuxEvent,
DemuxReceiverError>>` so the idiomatic drain pattern is `for result in &mut rx`.
Iterator termination is the clean-EOF signal: when the underlying
`RecvTransport` returns `TransportError::Closed`, `DemuxReceiver` calls
`Demuxer::flush` (recovering any trailing PES) and returns `Ok(None)`,
which the iterator turns into `None`. Peer-disconnect or unrecoverable
link errors surface as `Err(DemuxReceiverError::Transport(Broken(_)))`;
strict-mode rejections surface as `Err(DemuxReceiverError::Demux(_))`.

`DemuxReceiver::add_byte_sink` is the fan-out hook: register a callback
(`Box<dyn FnMut(&[u8]) + Send>`), and the callback receives every
188-byte TS packet pulled from the transport — in registration order,
before the demuxer parses them. The canonical use case is a
"write-to-disk + forward-via-RTP + demux-for-KLV" workflow where
multiple consumers tee off the same byte stream in one pass.

`ManagedRecvTransport<R>` is the receive-side reconnect decorator,
sibling to `ManagedTransport<T>`. It implements `RecvTransport`, so it
slots between any receive shell and the underlying transport
transparently. **It has no gap buffer** — receive-side bytes that
never arrived can't be replayed, so the decorator simply restarts the
recv loop on a fresh transport when the inner returns `Closed` or
`Broken`. The demuxer-side state (sync alignment, PES reassembly) does
carry over across reconnect, which costs at most one re-VERIFY pass
on the syncer.

**Decoupled pairing.** The demuxer surfaces every video AU and every
KLV record as an independent stream-tagged event with full timing.
It does **not** pair sync-KLV with video AUs — pairing tolerance,
sample-and-hold semantics, and multi-stream routing are
consumer-domain decisions the library can't make correctly for
everyone. The three canonical pairing patterns live as cookbook
recipes (12, 13, 14) with runnable example companions
(`pair_sync_klv.rs`, `tee_disk_and_demux.rs`).

## Cross-thread shutdown — `SrtCancelHandle`

Every long-lived pipeline shell exposes a
[`SrtCancelHandle`](./srt-cancel-handle.md) for cross-thread cancellation. The
handle is `Send + Sync`, one-shot, idempotent, and `Clone` — fire it
from any thread (a signal handler, a lifecycle observer, a parent-process
watchdog) and the parked `send_*` / `recv_*` on the calling thread
returns within one libsrt I/O cycle (3–10 ms). Bindings expose this as
a language-native shutdown primitive (Kotlin `Job.cancel()` analog,
Swift `Task.cancel()` analog, Python `threading.Event` analog, and
per-shell `tst_*_cancel()` entries in the C ABI —
`tst_mux_sender_cancel` / `tst_sender_cancel` / `tst_raw_sender_cancel`
on the send side, `tst_demux_receiver_cancel` / `tst_receiver_cancel` /
`tst_raw_receiver_cancel` on the receive side, plus matching
`tst_managed_*_cancel` siblings on the reconnect decorators).

This is the supported shape for breaking a sync-blocking shell from
another thread. Shells return the trait-object form
`Option<Arc<dyn TransportCancel + Send + Sync>>` — pipeline-layer,
transport-agnostic. The concrete struct lives in `tst-core`
(`SrtCancelHandle` wraps an integer handle plus a closer closure — for
SRT, the handle is the `SRTSOCKET` and the closer is `srt_close`) and
is re-exported as `tst_pipeline::SrtCancelHandle` (and as
`tst_srt::SrtCancelHandle`) so binding authors have a single import path.
The Rust API stays synchronous-blocking; the cancel handle is the
supported escape hatch for "wake the parked syscall now" without
time-sliced polling. (When async lands later as a separate crate,
`SrtCancelHandle` remains the sync-blocking primitive — see
**Sync vs. async** below.)

See [`srt-cancel-handle.md`](./srt-cancel-handle.md) for the full pattern,
threading guarantees, and per-language idiom table; cookbook recipe 31
is the runnable companion.

## Sync vs. async

The public API is sync blocking. `Socket::send` and `Socket::recv` block
the calling thread; `MuxSender::send_video` blocks until the underlying
SRT socket has accepted the bytes; reconnect inside `ManagedTransport`
runs on the caller's thread by default. This isn't an async runtime —
`ManagedTransport` also offers an opt-in `ReconnectMode::Background`
where a dedicated per-outage worker thread owns the reconnect loop so
`send_bytes` itself no longer waits on backoff or factory calls (it can
still block briefly on internal lock contention while the worker
drains), but that's a second thread the caller doesn't manage, not
cooperative scheduling.

Sync was chosen for three reasons. First, the target deployment shape
is small — a process talks to ≤10 SRT peers, so the thread-per-
connection cost is negligible. Second, sync code is simpler to reason
about and debug, especially when bridging into C / JVM / Swift / Kotlin
through the binding crates, none of which have a portable async story.
Third, the sync API mirrors `std::net::TcpStream` semantics, so a Rust
caller already knows the shape from the standard library.

Async is on the deferred-features list, not ruled out. Two viable paths
exist when a consumer asks. The lightweight path is a Tokio integration
that wraps each blocking call in `spawn_blocking` — minimal extra surface,
acceptable for low connection counts, no changes to `tst-core`'s
internals. The heavier path is a full async reactor backed by libsrt's
`srt_epoll_*` family with `tokio::io::unix::AsyncFd` or equivalent
registration — better scalability, much bigger surface to design and
test. The choice is consumer-driven; until then the sync API stays. See
[`docs/project/deferred-features.md`](/docs/project/deferred-features.md) for the current note.

Note that "sync" applies to the public API surface — internally, the
`MuxSender` family uses an internal mutex so the data path is safe to
call from multiple threads if a consumer wants to fan in NAL and KLV
inputs on separate producers. The synchronization is invisible to single-
threaded callers and adds no contention when only one thread is
producing.

## What's deferred

The summary below points at the canonical list once; each item maps to
an entry in [`docs/project/deferred-features.md`](/docs/project/deferred-features.md).

- AV1 / H.266 carriage ships; explicit non-goals remain — full AV1 Frame Header (decoder-scope), AV1 multi-OP, `AV1_video_descriptor`, AVIF helper, H.266 APS / Picture Header NAL parsing, multi-layer H.266, `stream_type 0x32`, AV1 on `0x80`.
- Audio frame parsers — `codec::mpegaudio` (Layer I/II/III), `codec::aac::adts`, and `codec::ac3` (syncframe parser) ship; `codec::aac::latm` is a sync validator only (full `audioMuxElement` decode deferred); E-AC-3 (Annex E) is deferred.
- Reactor / async / `srt_epoll_*` — see the sync-vs-async section above.
- Bonding / connection groups (`SRTO_GROUP*`) — no consumer demand.
- Other typed MISB sets — ST 0102 (Security LS), ST 0903 (top-level VMTI + per-target `VTargetPack`), and ST 0806 (RVT, plus nested POI/AOI/User Defined sub-sets) ship as sibling-layer typed views over the substrate; nested VMTI sets (VMask / VTracker / VChip / Algorithm Series / Ontology Series) remain pass-through.
- Owned-projection variants on borrowed iterator types — `VTargetSeriesIter`, `KlvIterator`, and the indexed NAL iterator are borrow-coupled today; cross-language wrappability needs owned-by-value variants.
- `serde` / `no_std` for `klv` — pure additive; behind feature flags when added.
- `tst-c` receiver surface — fully shipped (`tst_raw_receiver_t` Phase 1 plan #59, `tst_receiver_t` Phase 2 plan #60, `tst_demux_receiver_t` + typed `tst_event_t` tagged union + multi-program demux Phase 3 plan #62), not deferred. Listed here for cross-reference only. The two genuinely-still-deferred C-ABI hooks are `add_byte_sink` fan-out and `tst_pairer_t` (both tracked in `docs/project/deferred-features.md`).
- Rustdoc lift to docs.rs — these markdown files are written CommonMark-clean so the lift is mechanical when scheduled.

See [`docs/project/deferred-features.md`](/docs/project/deferred-features.md) for the
canonical list and the rationale for each entry.

## See also

- [Binding-author starter](./binding-authors.md) — entry point for `tst-jni` and `tst-uniffi` authors (plus the existing `tst-c` ABI).
