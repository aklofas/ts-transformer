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
| `crates/` | The pure-Rust core: library + transports + test-infra | `srt-sys` (published as `tstrans-srt-sys`), `rist-sys` (published as `tstrans-rist-sys`) (raw FFI); `tst-core` (engine); `tst-pipeline` (shells); the transports `tst-srt` / `tst-rtp` / `tst-udp` / `tst-tcp` / `tst-hls` / `tst-rist`; `tst-integration`, `tst-interop`, `tst-test-helpers` (test infra) |
| `bindings/` | Language bindings for downstream consumers | `bindings/c` (crate `tst-c` — cdylib/staticlib + `include/tstrans.h`) with its embeddable rlib at `bindings/c/core` (crate `tst-c-core`); `bindings/python` (crate `tst-py`); `bindings/jvm` (crate `tst-jni`); `bindings/apple-android` planned |
| `embedded/` | Bare-metal / QEMU firmware test harnesses (workspace-excluded) | `baremetal-qemu` (no_std muxer/pipeline QEMU smoke), `baremetal-qemu-c` (C-firmware staticlib glue), `freertos-srt` (libsrt-on-FreeRTOS) |
| `examples/` | Runnable Rust examples (crate `tst-examples`, `publish = false`) | Task-oriented subfolders; C examples mirror this taxonomy under `bindings/c/examples/` |
| `vendor/` (workspace root) | No longer used for `srt`/`librist`/`mbedtls` — each now bundles its native source inside its owning crate (crates.io requires a package to be self-contained) | `crates/srt-sys/vendor/srt` (libsrt 1.5.7), `crates/rist-sys/vendor/librist`, `crates/mbedtls-src/vendor/mbedtls` (3.6.7 LTS) |
| `scripts/` | CI ratchets + generators + dev tools | `check/{c,jvm,python,repo,rust}/` rails, `gen/` generators, `dev/` tools, `interop/` (interop, soak and stress harness drivers), `release/`, `apple/`, plus `ratchets/` (TSV-driven coverage); the embedded gates live in `embedded/scripts/check/` |
| `tests/` | Cross-cutting advisory control plane | `tests/coverage/` manifests (fixture/skip-ledger/stream-matrix) |
| `oss-fuzz/` | OSS-Fuzz packaging (options + seed corpora) | Per-crate fuzz targets live in `crates/<c>/fuzz/` |

The boundary is: `crates/` is everything a Rust consumer would depend on; `bindings/`
is everything a non-Rust consumer links against; `embedded/` is firmware test
scaffolding that is not a workspace member. All `bindings/*` and `examples` remain
Cargo workspace members; `embedded/*` are `exclude`d (separate build roots).

## Crate graph

```
tstrans-mbedtls-src ─(build)─→ tstrans-srt-sys  (raw libsrt FFI)  ──→ tst-srt
   (mbedTLS source)  ─(build)─→ tstrans-rist-sys (raw librist FFI) ──→ tst-rist

tst-core (engine: mux / demux / klv / codec + Transport traits)
   ├──→ tst-pipeline (sender / receiver shells, ManagedTransport)
   ├──→ tst-srt   (SRT;  + tstrans-srt-sys)   ┐
   ├──→ tst-rtp   (RTP / RTSP)                │ transports — each depends on
   ├──→ tst-udp   (UDP)                       │ tst-core + tst-pipeline (the
   ├──→ tst-tcp   (TCP / TLS)                 │ shared `binding` layer: error
   ├──→ tst-hls   (HLS)                       │ kinds, handles; tst-rtp also
   └──→ tst-rist  (RIST; + tstrans-rist-sys)  ┘ wraps the shells)

bindings (every one depends on tst-core + tst-pipeline + transports,
never on a *-sys crate):
   tst-c   ──→ tst-c-core ──→ tst-core + tst-pipeline + all six transports
                              (each transport behind an opt-in cargo feature)
   tst-py  ──→ tst-core + tst-pipeline + all six transports (default-on)
   tst-jni ──→ tst-core + tst-pipeline + tst-srt + tst-rtp + tst-hls
   tst-uniffi (planned) — same shape as tst-jni

dev-only: tst-test-helpers (publish = false; shared test fixtures and
helpers consumed by tst-core / tst-pipeline / tst-srt test suites);
tst-integration (cross-binding test harness) and tst-interop (the
interop / soak / stress tool, `tst-interop`) — both publish = false
vendored (bundled per-crate, not at the workspace root): crates/srt-sys/vendor/srt
(libsrt 1.5.7), crates/rist-sys/vendor/librist, crates/mbedtls-src/vendor/mbedtls
(3.6.7 LTS)
```

The layering rule is one-directional: lower layers do not depend on upper
layers. `tst-core` depends on no other workspace crate and on no native
library. `tst-pipeline` depends on `tst-core` only. The transport crates
depend on `tst-core` and on `tst-pipeline` — not for the shells (a shell
and a transport still meet through the `Transport` / `RecvTransport`
traits, not through a crate dependency) but for `tst_pipeline::binding`,
the one error-kind vocabulary and handle model every language binding
is built from. Each transport crate's `opener` module opens its URL scheme
into `tst_pipeline::binding::Opened`, which the Python and JVM bindings wrap
in one shell class per shell; `tst-rtp` additionally wraps the shells for its RTSP
mount surface. Binding crates (`tst-c` via `tst-c-core`, `tst-py`, `tst-jni`,
the planned `tst-uniffi`) depend on `tst-core` + `tst-pipeline` + the
transport crates they expose — never on a `*-sys` crate directly. This
keeps every binding's surface area defined by the same safe Rust API and
means a fix in `tst-core` reaches every binding without per-binding
patches.

`tst_pipeline::binding` stays a module of `tst-pipeline` rather than a crate
below it. It builds `no_std` + `alloc`, so the bare-metal C plane already
uses it; a separate crate would move the same lines, take `MuxSender`'s
lock seam with it, and add a published crate. The four small transports'
dependency on `tst-pipeline` is that module's error-kind table
(`binding_kind.rs` in each crate), not the shells.

`srt-sys` (published as `tstrans-srt-sys`) is the raw FFI layer — bindgen-generated against libsrt 1.5.7,
exposing roughly 72 `srt_*` functions and the full `SRT_*` constant
surface, with `mbedtls` wired in as the encryption backend by default.
`rist-sys` (published as `tstrans-rist-sys`) is its librist twin. Both
sys crates build the vendored mbedTLS tree supplied by
`tstrans-mbedtls-src` (a build-dependency only). `tst-srt` is the only
consumer of `srt-sys` and `tst-rist` the only consumer of `rist-sys`;
`tst-core` is the safe Rust API and never sees either. The vendored
libsrt, librist and mbedTLS submodules are pinned by tag (`v1.5.7`,
`v0.2.20`, `v3.6.7` LTS); submodule advances are deliberate, separate
commits. All vendored builds link statically, so `tst-c`'s shared library
has no runtime dependency on a system libsrt, librist or libmbedtls.

## Inside `tst-core`: four modules

- `klv::*` — KLV codec, generic substrate plus typed ST 0601 / ST 0605 / ST 0102 (sibling-layer Security LS) / ST 0903 (sibling-layer VMTI LS — top-level + per-target `VTargetPack`; nested LSes pass-through) / ST 0805 / ST 0806 / ST 1010 / ST 1204 layers.
- `mpegts::mux::*` — sender-side MPEG-TS muxer for H.264 / H.265 / H.266 / AV1 video + audio (MP2 / AAC ADTS / AAC LATM / AC-3) + subtitles (DVB-sub / DVB-teletext / CEA-708 / WebVTT-in-TS) + KLV.
- `mpegts::demux::*` — receiver-side MPEG-TS demuxer; bytes in, typed `DemuxEvent` out.
- `codec::*` — typed parameter-set parsers for H.264 / H.265 / H.266 / AV1 plus audio frame parsers for `mpegaudio` / `aac::adts` / `ac3`.

Each module is independently usable. A consumer who only needs KLV decode
can pull in `tst-core` and use `klv::st0601::decode` without touching the
muxer or transport. A consumer who already has their own TS muxer can
skip `mpegts::mux` and feed bytes through `tst_pipeline::Sender`. A
consumer who only wants the SRT socket — for an entirely different
streaming protocol on top — can use `tst_srt::SrtSocket` and `tst_srt::SrtListener`
directly.

## Inside `tst-srt`

The SRT-specific surface lives in its own crate so `tst-core` stays
free of any libsrt dependency. `tst-srt` re-exports the safe wrappers
at the crate root: `SrtSocket` / `SrtListener` (connected and listening
sockets), `SrtSocketBuilder` / `SrtListenerBuilder` (fluent construction over
`SrtSocketConfig` / `SrtListenerConfig`), `SrtTransport` (the canonical
`Transport` + `RecvTransport` impl), `ResourceCancel` (one-shot
pre-emptive close), and `SrtStats` (live snapshot of libsrt's internal
counters). The `url::SrtUrl` type parses `srt://host:port?key=value&…`
into a `SrtSocketConfig` using libsrt's documented option vocabulary,
and `addr::*` handles IPv4 / IPv6 sockaddr marshalling. See
[guides/srt.md](/docs/guides/srt.md) for the full surface.

`tst-pipeline` is the composition layer; it depends on `tst-core` only and
is generic over the transport it is handed. It is deliberately thin: its job is composition, not new behaviour. The
send shells delegate framing to `mpegts::mux::Muxer` and wire transport to
a `Transport` implementation; the receive shells delegate framing recovery
to `mpegts::demux::Demuxer` and bytes-in to a `RecvTransport`
implementation. Metadata typing for both directions is `klv::st0601` /
`klv::st0605`. MPEG-TS sync-metadata AU cell carriage lives at
`mpegts::au_cell` (per ITU-T H.222.0 V9 § 2.12.4.2). The canonical
`Transport` + `RecvTransport` impl is `SrtTransport` in `tst-srt` over
`tst_srt::SrtSocket` — the same wrapper handles both directions on a connected
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
plus a gap buffer. In `ReconnectMode::Background` the gap buffer holds
messages for the whole outage while a worker reconnects; in the default
`ReconnectMode::Blocking` the caller's `send_bytes` rides out the
reconnect itself, and a send that still fails is handed back to the
caller — nothing stays queued between calls. Because it satisfies `Transport`, it slots between any sender shell and
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
window (in `ReconnectMode::Background`; in `Blocking` mode a failed
send surfaces to the caller instead). `Sender` re-establishes sync alignment on the byte stream
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
same wrapper handles both directions on a connected `tst_srt::SrtSocket`.

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
`Broken`. Nothing but the gap buffer carries across a reconnect in
`ManagedTransport` / `ManagedRecvTransport`; `ManagedDemuxReceiver` resets
its per-stream demux state (sync alignment, PES reassembly) on reconnect
while aggregate counters and configuration survive, as
[`binding-authors.md`](/docs/reference/binding-authors.md) §"Receiver-side reconnect"
describes.

**Decoupled pairing.** The demuxer surfaces every video AU and every
KLV record as an independent stream-tagged event with full timing.
It does **not** pair sync-KLV with video AUs — pairing tolerance,
sample-and-hold semantics, and multi-stream routing are
consumer-domain decisions the library can't make correctly for
everyone. The canonical pairing patterns live in the cookbook's
[Pairing section](/docs/cookbook/index.md) with runnable example companions
(`pair_sync_klv.rs`, `tee_disk_and_demux.rs`).

## Cross-thread shutdown — `ResourceCancel` and `FlagCancel`

Every long-lived pipeline shell exposes
`cancel_handle() -> Option<Arc<dyn TransportCancel>>` for cross-thread
cancellation; [`ResourceCancel`](./srt-cancel-handle.md) is the SRT
transport's implementation of that trait (an integer handle plus a closer);
UDP, TCP, RIST, RTP and both RTSP handles use `tst_core::cancel::FlagCancel`,
a shared flag their blocking calls poll. The
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
transport-agnostic. The SRT implementation lives in `tst-core`
(`ResourceCancel` wraps an integer handle plus a closer closure — for
SRT, the handle is the `SRTSOCKET` and the closer is `srt_close`) and
is re-exported as `tst_pipeline::ResourceCancel` (and as
`tst_srt::ResourceCancel`) so binding authors have a single import path.
The Rust API stays synchronous-blocking; the cancel handle is the
supported escape hatch for "wake the parked syscall now" without
time-sliced polling. (When async lands later as a separate crate,
`ResourceCancel` remains the sync-blocking primitive — see
**Sync vs. async** below.)

See [`srt-cancel-handle.md`](./srt-cancel-handle.md) for the full pattern,
threading guarantees, and per-language idiom table; the cookbook recipe
[Graceful shutdown from another thread](/docs/cookbook/operations/graceful-shutdown.md)
is the runnable companion.

## Sync vs. async

The public API is sync blocking. `SrtSocket::send` and `SrtSocket::recv` block
the calling thread; `MuxSender::send_video` blocks until the underlying
SRT socket has accepted the bytes; reconnect inside `ManagedTransport`
runs on the caller's thread by default. This isn't an async runtime —
`ManagedTransport` also offers an opt-in `ReconnectMode::Background`
where a dedicated per-outage worker thread owns the reconnect loop so
`send_bytes` itself no longer waits on backoff or factory calls (it can
still block briefly on internal lock contention while the worker
drains), but that's a second thread the caller doesn't manage, not
cooperative scheduling.

Two crates own private tokio runtimes today — one per `RtspServer`
(`tst-rtp`) and one per serving `HlsPublisher` (`tst-hls` owns a runtime
only when it serves HTTP; two workers). Each is created in the
constructor, blocked on from the caller's thread, and shut down with a
bounded timeout by `stop()`/`Drop`; public methods that touch a runtime
must not be called from inside one. The rule is
§"Runtime ownership" below.

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

## Runtime ownership

Rule: a long-lived object owns at most one named OS thread or one private
async runtime. It starts it in its constructor (or its `start`), signals it
through its cancel handle, joins it with a bounded wait in `close()` /
`stop()` / `finish()`, and detaches it in `Drop`. Data-path calls never
wait on that thread's I/O. New crates follow this; the table records
today's owners.

| Crate | Owner | Thread or runtime | Started | Stopped | Exit with it still running |
|---|---|---|---|---|---|
| `tst-rtp` | `RtspServer` | private multi-thread tokio runtime with two worker threads named `tst-rtp-server` | the runtime is built by `RtspServerBuilder::build`; `start()` spawns the listener task | `stop()` sends the Notice 5402 ANNOUNCE, cancels every session, waits for every session task to end (at most `graceful_shutdown_drain`), then shuts each session's write half (500 ms each). `Drop` hard-cancels and waits up to 5 s for the runtime to shut down | no exit hook; the runtime ends with the process and no Notice or TEARDOWN is sent |
| `tst-rtp` | `RtspClient` | two std threads, `rtsp-keepalive` and `rtsp-interleaved-pump` | the keepalive thread at `RtspClientBuilder::connect` (unless auto keepalive is off); the pump at SETUP when media is TCP-interleaved | the client's cancel flag stops both. `Drop` sets it, sends a TEARDOWN bounded to 500 ms, then joins both threads. There is no `close()` | no exit hook; the threads end with the process |
| `tst-rtp` | `RtpRecvTransport` built from an RTSP session | one std thread, `rtsp-rtcp-ingest` | when the transport is built from the interleaved pump's channels | never signalled and never joined; it exits when the pump's RTCP sender drops, which the client's `Drop` causes | no exit hook; the thread ends with the process |
| `tst-hls` | `HlsPublisher` (only when it serves HTTP) | one std thread named `tst-hls` that owns a current-thread tokio runtime; serving is single-threaded, so a segment read or a cut delays the other connections for its duration | `HlsPublisher::with_config`, which binds the listener | `HlsServerHandle::shutdown()` after `finish_serving()`, or `Drop` of the publisher or handle: cancel, then a wait of up to 2 s for the thread to finish when `shutdown()` is called; `Drop` cancels and detaches | no exit hook; the thread ends with the process |
| `tst-pipeline` | `ManagedTransport` with `ReconnectMode::Background` | one std thread per outage, named `tst-reconnect` | the first send that finds the inner transport down spawns it, and so does a send that meets a backlog after a give-up | `close()` latches closed, wakes the backoff wait and the inner transport's cancel handle, then joins; the join waits for an in-flight factory call. `Drop` signals the same way and detaches | no exit hook; a worker still running ends with the process |
| `tst-srt` | libsrt itself (garbage-collector and queue worker threads); not a Rust object | libsrt's threads, process-wide | the first libsrt use (`srt_startup`) | each `SrtSocket` and `SrtListener` `close()`; `srt_cleanup()` stops the library | guarded: the `atexit` handler in `exit_guard.rs` closes every open socket, waits up to 2 s for operations in flight, then runs `srt_cleanup()` |
| `tst-rist` | `RistTransport`, `RistRecvTransport` (one librist context each) | librist's protocol thread | `RistCtx::new` calls `rist_start` | `close()` calls `rist_destroy`, which joins the thread. `Drop` calls `close()`, so it waits too | not guarded: there is no exit hook, so keep the owner alive until `close()` returns |

Where today's code differs from the rule, the difference stays until the
owner's next structural change:

- `RtspClient` and the two RIST transports join in `Drop`; the rule says
  detach.
- `RtspServer`'s `Drop` waits up to 5 s for its runtime to shut down; the
  rule says detach.
- `RtspServer::stop()` quiesces sessions but does not release the runtime;
  `Drop` does.
- `RtspClient` has no `close()`; its `Drop` is the only stop.

## Mount lifecycle

What ends a mount, per mount family. "Handle" is the application's
`MountHandle` / `PublishMountHandle`. A session "completes SETUP on" a
mount when its SETUP is answered `200`; only those sessions receive the
Notice 5402 ANNOUNCE. Plain and TLS sessions share one session writer, so
`rtsps://` sessions receive it too.

| Event | Local mount (`add_mount`) | Publish mount (`add_publish_mount`, on-demand) | WebRTC mount (planned) |
|---|---|---|---|
| The application drops its handle | Nothing. `MountHandle` has no `Drop`; the server's table keeps the mount, readers keep playing, and later PLAYs still find it. No ANNOUNCE. | Nothing. The table keeps the mount, and an on-demand mount stays registered until `remove_mount`. A transport already taken from the handle is unaffected. No ANNOUNCE. | Must match the publish column unless the WebRTC plan says otherwise. |
| `remove_mount(path)` | The entry leaves the table. Every reader session that completed SETUP on the path gets the Notice 5402 ANNOUNCE and is cancelled. A session that has not completed SETUP is left alone. Handles stay valid and `push_*` succeeds with no readers. A multicast mount's sender task keeps running while a handle is alive. | The mount is closed first: the application transport reads `Closed`, the publisher slot is emptied and refuses every later claim (ANNOUNCE `403`, RECORD `455`). Then every reader and publisher session that completed SETUP on the path gets the ANNOUNCE and is cancelled. An on-demand handle still queued for `next_publisher` is not withdrawn; its transport reads `Closed` at once. The on-demand cap slot is freed. | Must match the publish column unless the WebRTC plan says otherwise. |
| `RtspServer::stop()` | The queue of on-demand handles is closed, then every session that completed SETUP on any mount gets the ANNOUNCE and is cancelled. The listener stops accepting. The mounts stay in the table, and handles stay valid with no readers. | The same for sessions, publishers included. Also every publish mount is closed: transports read `Closed`, a parked `next_publisher` wakes with `Shutdown`, and an ANNOUNCE that would create an on-demand mount answers `503`. | Must match the publish column unless the WebRTC plan says otherwise. |
| The last reader or publisher session ends | A reader leaving drops only its own fanout subscription. The mount is untouched and `peer_count` falls. No ANNOUNCE (the peer ended it). | A reader leaving changes nothing. A publisher leaving (TEARDOWN, disconnect or drop) flushes its adapter, stops its UDP ingest and empties the publisher slot. The application transport stays open and silent for the next publisher, and an on-demand mount stays in the table. No ANNOUNCE. | Must match the publish column unless the WebRTC plan says otherwise. |
| `Drop` of the server | The server hard-cancels and its runtime shuts down within 5 s. Sessions end with no ANNOUNCE. Handles outlive the server and `push_*` succeeds with no readers. | The on-demand queue is closed and every publish mount is closed before the runtime shuts down, so transports read `Closed`. Sessions end with no ANNOUNCE. | Must match the publish column unless the WebRTC plan says otherwise. |

## What's deferred

The summary below points at the canonical list once; each item maps to
an entry in [`docs/project/deferred-features.md`](/docs/project/deferred-features.md).

- AV1 / H.266 carriage ships; explicit non-goals remain — full AV1 Frame Header (decoder-scope), AV1 multi-OP, `AV1_video_descriptor`, AVIF helper, H.266 APS / Picture Header NAL parsing, multi-layer H.266, `stream_type 0x32`, AV1 on `0x80`.
- Audio frame parsers — `codec::mpegaudio` (Layer I/II/III), `codec::aac::adts`, and `codec::ac3` (syncframe parser) ship; `codec::aac::latm` is a sync validator only (full `audioMuxElement` decode deferred); E-AC-3 (Annex E) is deferred.
- Reactor / async / `srt_epoll_*` — see the sync-vs-async section above.
- Bonding / connection groups (`SRTO_GROUP*`) — no consumer demand.
- Other typed MISB sets — ST 0102 (Security LS), ST 0903 (top-level VMTI + per-target `VTargetPack`), and ST 0806 (RVT, plus nested POI/AOI/User Defined sub-sets) ship as sibling-layer typed views over the substrate; nested VMTI sets (VMask / VTracker / VChip / Algorithm Series / Ontology Series) remain pass-through.
- Owned-projection variants on borrowed iterator types — `VTargetSeriesIter`, `KlvIterator`, and the indexed NAL iterator are borrow-coupled today; cross-language wrappability needs owned-by-value variants.
- `serde` for typed KLV records — pure additive; behind a feature flag when added. (`klv` is already `no_std`-capable with the rest of `tst-core`.)
- `tst-c` receiver surface — fully shipped (`tst_raw_receiver_t`, `tst_receiver_t`, `tst_demux_receiver_t` + typed `tst_event_t` tagged union + multi-program demux), not deferred. Listed here for cross-reference only. The C ABI has no `add_byte_sink` fan-out and no `tst_pairer_t`; the Pairer gap is tracked in `docs/project/deferred-features.md` ("`pipeline::ext::pairing` C ABI exposure").
- Mirroring the `docs/` guide tree into rustdoc — not planned; each published crate's README is already rendered on crates.io / docs.rs, and the transport crates lift it into their crate-level rustdoc.

See [`docs/project/deferred-features.md`](/docs/project/deferred-features.md) for the
canonical list and the rationale for each entry.

## See also

- [Resource policy](/docs/reference/resource-policy.md) — the bounds, reservations and full-queue policies of the RTSP server.
- [Binding-author starter](/docs/reference/binding-authors.md) — entry point for `tst-jni` and `tst-uniffi` authors (plus the existing `tst-c` ABI).
