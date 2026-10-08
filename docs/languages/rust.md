# Rust bindings

> **Who this is for:** You write Rust and want to add MPEG-TS + KLV + SRT
> to your application.

> **You will learn:**
> - How to add the ts-transformer crates to your `Cargo.toml`
> - How to mux H.264 video + KLV into a `.ts` file with ~10 lines of code
> - How to send the result over an SRT socket
> - How to demux a `.ts` file and dispatch typed `DemuxEvent` items
> - The role of feature flags (`mbedtls`, `file`) and the workspace MSRV
> - The Rust-specific gotchas: `#[non_exhaustive]` enums, SRT init, reconnect
>   wrappers, and when to pick `MuxSender` vs `Sender`
> - Where to find the deep guides for each subsystem

## Install

When you want the full sender + receiver pipeline (mux/demux + SRT), pull
the three top-level crates in:

```toml
[dependencies]
tst-core     = "0.7"  # MPEG-TS mux/demux + KLV + codec parsers
tst-pipeline = "0.7"  # Sender / Receiver / MuxSender / DemuxReceiver shells
tst-srt      = "0.7"  # SRT transport
```

When you only need to inspect or build `.ts` bytes (no live transport),
`tst-core` alone is enough — it has no SRT dependency and skips the
libsrt / mbedTLS compile step entirely.

**MSRV:** Rust **1.85** (workspace-pinned via `rust-toolchain.toml`).
Running `cargo` inside the workspace auto-uses 1.85 via rustup.

**Feature flags worth knowing:**

| Crate         | Feature   | Default | Effect                                                           |
| ------------- | --------- | ------- | ---------------------------------------------------------------- |
| `srt-sys` (published as `tstrans-srt-sys`) | `mbedtls` | on | Builds encryption support using mbedTLS. Set a passphrase on both peers to encrypt a connection. |
| `tst-srt`     | `mbedtls` | on      | Propagates to `srt-sys/mbedtls`.                                 |
| `tst-core`    | `std`     | on      | Standard library: file I/O, net helpers, JSON/TOML. Off = `#![no_std]` + `alloc` (see [embedded](/docs/languages/embedded.md)). |
| `tst-core`    | `file`    | on      | File I/O helpers. `file` and `std` imply each other, so they switch on and off together. |

A clean rebuild compiles libsrt 1.5.7 and mbedTLS 3.6.7 from vendored
submodules — expect **3–5 minutes** on a cold cache, seconds when warm.
Force the vendored path with `SRT_FORCE_VENDORED=1` (otherwise the build
script tries `pkg-config srt ≥ 1.5.0` first).

For the per-target support matrix, see
[`/docs/reference/compatibility.md`](/docs/reference/compatibility.md).

## Hello world

The smallest useful thing: mux one H.264 access unit + one KLV blob into
188-byte TS packets, entirely in memory — no SRT, no peer, no file.

The short byte arrays below are synthetic placeholders. They demonstrate
the push/pull API; they do not contain playable video or a valid ST 0601
record. The [hello-world example](/examples/getting-started/hello_world.rs)
shows how to build the KLV bytes from a typed record.

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default config: one program, H.264 video on PID 0x1011, async KLV on 0x1031.
    let mut muxer = Muxer::new(MuxerConfig::default())?;

    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xA5, 0xA5, 0xA5]; // minimal Annex-B IDR
    muxer.push_video(&nal, Pts90khz::new(0), /* key_frame */ true)?;
    muxer.push_klv(&[0x06, 0x0E, 0x2B, 0x34, 0xDE, 0xAD, 0xBE, 0xEF], Pts90khz::new(0), 0x00)?;

    let mut buf = [0u8; 1316];
    let n = muxer.pull(&mut buf);
    println!("muxed {n} bytes ({} TS packets)", n / 188);
    Ok(())
}
```

The sequence is: configure a `Muxer`, push encoded payloads, then pull TS
bytes. To send encoder output over a network, `MuxSender` combines these
steps with a transport.

## First send

When you want to ship those TS bytes over SRT to a peer, compose the
muxer with an `SrtTransport` via a `MuxSender`. The shell owns
synchronization between push and send, handles transient transport
failures, and gives you a single `send_video` / `send_klv` API:

```rust
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_srt::{SocketBuilder, SrtTransport};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open an SRT socket to the peer. 120 ms latency is a reasonable
    //    LAN/regional WAN default; bump for transcontinental / cellular paths.
    let mut sb = SocketBuilder::new();
    sb.latency_ms(120);
    sb.recv_timeout(Duration::from_secs(5));
    let socket = sb.connect("127.0.0.1:9000")?;
    let transport = SrtTransport::new(socket);

    // 2. Wrap muxer + transport. Default config = 1 program, H.264 + async KLV.
    //    Argument order is (transport, config).
    let sender: MuxSender<SrtTransport> =
        MuxSender::new(transport, MuxerConfig::default())?;

    // 3. Push payloads. Each push muxes into TS packets and ships them.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xA5, 0xA5, 0xA5];
    sender.send_video(&nal, Pts90khz::new(0), /* key_frame */ true)?;
    // send_klv(klv, pts, metadata_service_id) — 0x00 is the ST 1402.2 default.
    sender.send_klv(&[0x06, 0x0E, 0x2B, 0x34, 0xDE, 0xAD, 0xBE, 0xEF], Pts90khz::new(0), 0x00)?;

    // finish(): fallible graceful shutdown — drains any buffered tail
    // and reports whether it was delivered (close() is the prompt,
    // best-effort primitive and returns ()).
    sender.finish()?;
    Ok(())
}
```

On the receiver side, run something like:

```bash
srt-live-transmit srt://:9000 file://con > /tmp/out.ts
```

For the full runnable version with synthetic frames + commentary on every
config knob:

```bash
cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
```

Other send-side examples worth knowing:

- [`examples/sending/encrypted_send_recv.rs`](/examples/sending/encrypted_send_recv.rs) — AES passphrase encryption end to end.
- [`examples/sending/srt_serve_ts_file.rs`](/examples/sending/srt_serve_ts_file.rs) — listen mode (peer dials in).
- [`examples/sending/sender_from_url.rs`](/examples/sending/sender_from_url.rs) — config via `srt://host:port?key=value`.
- [`examples/sending/custom_transport.rs`](/examples/sending/custom_transport.rs) — bring your own `Transport` impl (UDP, file, etc.).

See [`/docs/guides/mpegts-mux.md`](/docs/guides/mpegts-mux.md) for the full
`MuxerConfig` surface, and [`/docs/guides/pipeline.md`](/docs/guides/pipeline.md)
for picking among `MuxSender` / `Sender` / `RawSender`.

## First receive

Pulling typed events out of a `.ts` file (or live SRT stream) takes the
same shape: build a `Demuxer`, feed bytes, dispatch by event variant.

```rust
use std::env;
use std::fs;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: <file.ts>")?;
    let bytes = fs::read(&path)?;

    // Lenient by default. The demuxer keeps going past every recoverable
    // problem and surfaces what it found as `NonConformant` events. For
    // hard-fail behavior, swap to `Demuxer::with_config(DemuxerConfig::builder().strict(...).build())`.
    let mut d = Demuxer::new();
    d.feed(&bytes)?;  // `feed` accepts unaligned slices and re-syncs internally
    d.flush();        // end-of-stream: commits the trailing PES-length-0 AU

    while let Some(event) = d.next_event() {
        match event {
            // ProgramMap wraps a single `ProgramMap` struct (one per PMT seen).
            DemuxEvent::ProgramMap(m) => {
                println!("PSI: program {} ({} streams)", m.program_number, m.streams.len());
            }
            // Sample / Metadata / Discontinuity / NonConformant are STRUCT
            // variants — `pid` lives on the `stream` field, not the event.
            DemuxEvent::Sample { stream, pts, .. } => {
                println!("Sample pid=0x{:04X} pts={}", stream.pid, pts.as_ticks());
            }
            DemuxEvent::Metadata { stream, kind, .. } => {
                println!("Metadata pid=0x{:04X} kind={:?}", stream.pid, kind);
            }
            DemuxEvent::Discontinuity { stream, kind } => {
                eprintln!("Discontinuity pid=0x{:04X} {kind:?}", stream.pid);
            }
            DemuxEvent::NonConformant { stream, issue } => {
                eprintln!("NonConformant pid=0x{:04X} {issue:?}", stream.pid);
            }
            // #[non_exhaustive] enum — wildcard arm required.
            _ => {}
        }
    }
    Ok(())
}
```

Run the full version against any `.ts` file:

```bash
cargo run -p tst-examples --example demux_to_events -- /path/to/capture.ts
```

For the live SRT-side analogue, see
[`examples/receiving/srt_recv_typed.rs`](/examples/receiving/srt_recv_typed.rs) —
same event shape, but reading from a connected SRT socket instead of a
file. To dump bytes straight to a file:
[`examples/receiving/srt_listener_to_file.rs`](/examples/receiving/srt_listener_to_file.rs).

The full demuxer contract — strict-mode ladder, override surface, AU-cell
unwrap behavior, decoupled-pairing rationale — is in
[`/docs/guides/mpegts-demux.md`](/docs/guides/mpegts-demux.md).

## Language-specific gotchas

**`#[non_exhaustive]` enums require a wildcard arm.** `DemuxEvent`,
`MuxError`, `DemuxError`, `NonConformantIssue`, and many other public
enums in this workspace are marked `#[non_exhaustive]` so new variants
land without a major version bump. Your `match` arms must include a
`_ => { ... }` catch-all; the compiler error is explicit when you
forget. The current `#[non_exhaustive]` count is ratcheted in CI (see
`BASELINE` in `.github/workflows/ci.yml`).

**SRT initialization is automatic.** `tst-srt` calls `srt_startup` /
`srt_cleanup` on your behalf — don't call them manually. Cleanup runs
at process exit. If you build with `--no-default-features`, encryption
(mbedTLS) is omitted but the libsrt init / teardown path is unchanged.

**Pick the right sender shell.** `MuxSender<T>` is the canonical choice
when you have raw encoded video NALs + KLV records and want this library
to mux. Use `Sender<T>` (raw TS-bytes-through-transport) only when you
already have pre-muxed TS bytes from elsewhere (e.g. ffmpeg pipe).
`RawSender<T>` is the byte-blind one-message-per-call primitive — rarely
the right choice unless you're building your own framing layer.

**Reconnect is opt-in via wrappers.** A bare `SrtTransport` connects
once and fails hard on disconnect. To get exponential-backoff reconnect
+ a configurable gap buffer, wrap with `ManagedTransport<T>` on the
send side or `ManagedRecvTransport<T>` / `ManagedDemuxReceiver<T>` on
the receive side. The reconnect policy is a single `ReconnectPolicy`
struct — see [`/docs/guides/pipeline.md`](/docs/guides/pipeline.md) for
the full state machine.

**Feature flag interactions.** `--no-default-features` on `tst-srt`
disables mbedTLS and turns the libsrt build into unencrypted-only.
`tst-core`'s `file` feature implies `std`, so a `no_std` target builds
`tst-core` with `--no-default-features` and no `file`. The two flag sets
are independent.

**Builders use bind-then-step, not single-chain.** `SocketBuilder` and
`ListenerBuilder` mutators take `&mut self` but their terminal methods
(`connect`, `bind`) take `&self`. A single fluent chain off a temporary
dangles. Always bind the builder to a local first:

```rust
let mut sb = SocketBuilder::new();
sb.latency_ms(120);             // &mut self
let socket = sb.connect(addr)?; // &self
```

**Pairing KLV to video.** The demuxer emits KLV and video as independent
events on the same PTS clock; aligning them is the consumer's job. The
`Pairer` shell in `tst_pipeline::ext::pairing` is the standard solution —
configurable window, drop policy, and event-order preservation. See the
[`pairing/` examples directory](/examples/pairing/) and
[`/docs/cookbook/index.md`](/docs/cookbook/index.md).

## Where this binding differs from the Rust core

You're already at the canonical surface — there's no Rust-specific
deviation to document. Everything visible from the other language pages
exists here, in its richest form.

If you're integrating with another language, the dedicated pages call
out each binding's deviations relative to this surface:

- **C / C++:** [`/docs/languages/c.md`](/docs/languages/c.md) — opaque
  handles, libsrt-style negative error codes + thread-local last-error,
  ABI versioning.
- **Python:** [`/docs/languages/python.md`](/docs/languages/python.md) —
  offline file I/O plus the full live-transport surface (UDP / TCP / RTP+RTSP /
  SRT / RIST), `match`-friendly `DemuxEvent` subclasses, pandas /
  NumPy adapters, GIL release on long calls.
- **JVM:** [`/docs/languages/jvm.md`](/docs/languages/jvm.md) — the
  offline, RTP and SRT surface for JDK 17+, heap-copied `ByteBuffer`
  payloads.

The "Where this binding differs" section on each of those pages is the
authoritative gap list. Anything not called out there matches Rust 1:1.

## H.264-over-RTP ingest (RFC 6184)

`tst-rtp` ships a blocking H.264 depacketizer and receiver that reassemble
Annex-B access units from RFC 6184 RTP packets (single-NALU, STAP-A, FU-A;
modes 0 and 1). Mode 2 (interleaved — STAP-B / MTAP / FU-B / DON) is
rejected at SETUP with `RtspError::UnsupportedPacketizationMode(2)`.

The canonical path for an RTSP camera is the four-step sequence in the
example: `connect` → `describe` → `setup_h264_auto` → `into_h264_receiver`.
Run it with:

```bash
cargo run -p tst-examples --example recv_rtsp_h264 -- rtsp://cam.local/h264
# Force TCP-interleaved (useful when UDP is blocked by NAT):
cargo run -p tst-examples --example recv_rtsp_h264 -- 'rtsp://cam.local/h264?transport=tcp'
```

The example is at
[`examples/receiving/recv_rtsp_h264.rs`](/examples/receiving/recv_rtsp_h264.rs).
It covers every step with rich `// why + how` commentary.

### Key types

| Type | Notes |
|---|---|
| `H264Depacketizer` | Low-level state machine: `feed(header, payload)` / `next_au()` / `flush()`. Use when you are not going through `H264Receiver`. |
| `H264Receiver` | High-level shell: `listen("rtp://…?pt=96")` or `session.into_h264_receiver(config)`. `recv_au()` → `Option<H264Au>`. |
| `H264Au` | Output: `annexb` (Annex-B bytes), `pts: Pts90khz` (90 kHz decode-order), `key_frame`, `rtp_timestamp`. |
| `H264DepayConfig` | `payload_type`, `parameter_set_injection: ParameterSetInjection`, `initial_parameter_sets`, `max_au_bytes` (8 MiB default cap). |
| `ParameterSetInjection` | `None` — pass through as received; `BeforeIdr` (default) — prepend cached SPS/PPS before every IDR frame. |
| `H264DepayStats` | `aus_emitted`, `aus_dropped`, `seq_gaps`, `parameter_set_updates`, … |

### Minimal direct-UDP form

```rust,no_run
use tst_rtp::H264Receiver;

let mut rx = H264Receiver::listen("rtp://0.0.0.0:5004?pt=96")?;
// `into_h264_receiver` from an RTSP session is the more common path.
// Direct UDP listen is available for custom RTP senders.
while let Some(au) = rx.recv_au()? {
    println!(
        "AU: {} bytes, key={}, pts={}",
        au.annexb.len(),
        au.key_frame,
        au.pts.as_ticks(),
    );
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Integration notes

- **`sprop-parameter-sets` handled automatically.** `setup_h264_auto` decodes the
  `a=fmtp:N sprop-parameter-sets=…` SDP attribute from base64 into raw NALU
  bytes and stores them in `H264DepayConfig::initial_parameter_sets`. With
  `ParameterSetInjection::BeforeIdr` (the default), the depacketizer prepends
  the stored SPS/PPS before every IDR frame — giving decoders a clean
  self-contained start point even when the camera omits in-band parameter sets.

- **B-frame / DTS limitation.** `H264Au::pts` is derived from the RTP
  timestamp, which reflects decode order. If the source encoder uses B-frames
  (PTS ≠ DTS), the `pts` values may be non-monotonic. Feed them directly to
  `muxer.push_video` for low-latency live cameras (no B-frames, PTS = DTS); use
  `push_video_to_with_dts` and derive DTS separately for B-frame content.

- **Loss behavior.** A sequence-number gap poisons the currently-accumulating
  AU and the AU this packet joins, then increments `H264DepayStats::aus_dropped`
  and `seq_gaps`. Loss is whole-AU: no partial frames surface.

- **KLV pairing slot.** In a STANAG 4609 gateway you push `au.pts` to
  `muxer.push_video` and the matching KLV (from a separate UDP or SRT feed)
  to `muxer.push_klv` using the same `pts`. See [Ingest H.264 from an RTSP camera and remux to MPEG-TS](/docs/cookbook/receiving/recv-rtsp-h264-to-ts.md) in the cookbook.

- **RTCP is not processed on the H.264 path.** No RR/SR is sent, and
  received RTCP is discarded. See
  [`/docs/project/deferred-features.md`](/docs/project/deferred-features.md).

## RTSP publisher ingest

`RtspServer` also accepts publishers: an encoder connects and pushes with
ANNOUNCE / SETUP `mode=record` / RECORD (RFC 2326 §10.3, §10.11), over
TCP-interleaved or UDP. The application reads each pushed stream as MPEG-TS
through an `RtpRecvTransport`, so `DemuxReceiver` works unchanged, and the
same mount re-serves PLAY readers.

Nothing new to install: this is part of `tst-rtp`'s default-on `rtsp-server`
feature. Run the example and push to it with ffmpeg:

```bash
cargo run -p tst-examples --example recv_rtsp_publish
ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 -c:v libx264 \
    -preset ultrafast -tune zerolatency -f rtsp rtsp://127.0.0.1:8554/demo
```

The example is at
[`examples/receiving/recv_rtsp_publish.rs`](/examples/receiving/recv_rtsp_publish.rs);
the cookbook recipe is
[Accept RTSP publishers](/docs/cookbook/receiving/rtsp-publish-ingest.md).

### API

| Item | Notes |
|---|---|
| `RtspServerBuilder::accept_unregistered_publishers(bool)` | Off by default (an ANNOUNCE on an unknown path answers `404`). On: the ANNOUNCE creates a publish mount and queues its handle. |
| `RtspServer::add_publish_mount(path)` | Registers a name up front and returns its `PublishMountHandle`. |
| `RtspServer::next_publisher(timeout)` | Takes the next on-demand mount; `Ok(None)` when the timeout passes; `Err(Shutdown)` after `stop()`. |
| `RtspServer::remove_mount(path)` | Removes a mount of any kind: every session on it gets the server-initiated TEARDOWN notice and is closed, and the path is freed. `MountNotFound` for an unknown path. |
| `RtspServer::stats()` | `ServerStats` with `active_publishers`, `total_rtp_packets_received`, `total_rtp_bytes_received`. |
| `PublishMountHandle` | Cheap `Clone`. `mount_path()`, `peer_count()` (PLAY readers), `generation()`, `publisher()`, `stats()`, `cancel()`, `into_recv_transport()`. |
| `PublisherInfo` | The current publisher: `peer` (control-connection address), `shape`, `since`, `generation`. |
| `PublishShape` | `Mp2t` or `Elementary { klv }`. |
| `ClockAlignment` | `NotApplicable`, `Pending`, `Provisional`, `SenderReport`; see the stats below. |

### Accepted shapes

| Announced tracks | Application receives |
|---|---|
| One MPEG-TS track (`MP2T/90000`, or static payload type 33) | The publisher's TS bytes, unchanged |
| One H.264 track (`H264/90000`) | TS re-muxed by the server, video on PID 0x100 |
| H.264 plus one KLV track (`smpte336m/90000`) | TS re-muxed by the server, video on PID 0x100, KLV on PID 0x101 (async, with PTS) |

Any other announce answers `415`: audio, H.265, two video tracks, KLV
without video, or an H.264 or KLV track whose clock rate is not 90000.
ffmpeg pushes the H.264 shape only and refuses KLV (`Unsupported codec
klv`); to push KLV, use GStreamer (`rtpmp2tpay`, or `rtph264pay` plus
`rtpklvpay`) or send the TS over SRT or UDP.

### Rules worth knowing

- **One publisher per mount.** A second ANNOUNCE on a mount with a live
  publisher answers `403`; the first keeps streaming. When a publisher ends,
  the mount idles and `generation()` goes up by one. The application
  transport stays open and silent, and the next publisher feeds it.
- **`into_recv_transport` is take-once per mount.** A second call, from any
  clone of the handle, returns `RtspServerError::TransportTaken` (the
  bindings report it as the `Closed` kind). Clone the handle before
  consuming it if you want `stats()` and `publisher()` afterwards.
- **How the transport ends.** `PublishMountHandle::cancel()` ends it with
  `TransportError::ExplicitClose` and leaves readers and the publisher
  alone. `remove_mount` and `stop()` end it with `TransportError::Closed`,
  which `DemuxReceiver` reports as end of stream. A publisher leaving does
  neither.
- **Idle names expire only through `remove_mount`.** An on-demand mount
  stays in the table after its publisher leaves and after its handle is
  dropped. At most 64 on-demand handles wait for `next_publisher` and at
  most 256 on-demand mounts exist; an ANNOUNCE past either bound answers
  `503`. Registered mounts never count. `remove_mount`, like `stop()`,
  blocks the calling thread: never call it from inside a tokio runtime.
- **One role per connection.** A reader SETUP or PLAY on a publisher's
  connection, or an ANNOUNCE on a reader's, answers `455`.
- **One credential set.** Readers and publishers authenticate against the
  server's single `auth_basic` / `auth_digest_*` credential.

### Stats

`PublishMountHandle::stats()` returns `PublishMountStats`, cumulative over
the mount's life across publishers:

- `rtp_packets_received`, `bytes_received`, `malformed_packets`, and
  `source_rejected` (UDP datagrams from an IP other than the publisher's
  control connection).
- `frames_emitted`, `frames_dropped_app` (the application queue was full:
  it holds one maximum-size 8 MiB access unit, re-muxed, so it fills only
  when the application stops reading) and `frames_dropped_readers`.
- `aus_emitted`, `aus_dropped` (including access units before the first
  keyframe) and `aus_reordered`: nonzero means the publisher sends
  B-frames, which the re-muxed TS carries with a PTS and no DTS.
- `klv_units_emitted`, `klv_units_dropped`, `ssrc_changes`.
- `alignment`: how KLV is placed on the video clock. `NotApplicable` for
  MPEG-TS and video-only publishers, `Pending` while KLV is held,
  `SenderReport` once RTCP sender reports for both tracks arrived,
  `Provisional` after two seconds without them (first-packet coincidence).
  `alignment_steps` counts mapping replacements.
- `generation` and `peer_count` (live PLAY readers).

What the publisher role leaves out (RTCP receiver reports, audio and H.265
tracks, separate publisher credentials, `rcvbuf` on publisher sockets, DTS
for B-frames) is listed in
[`/docs/project/deferred-features.md`](/docs/project/deferred-features.md).

## Where to go next

- [`/docs/start/concepts.md`](/docs/start/concepts.md) — the conceptual
  model (mux/demux, KLV, transport, pipeline shells) before any code.
- [`/docs/cookbook/index.md`](/docs/cookbook/index.md) — recipes keyed to runnable
  examples for the most common patterns.
- [`/docs/guides/srt.md`](/docs/guides/srt.md) — full SRT surface:
  encryption, latency, stats, error model, URL parsing.
- [`/docs/guides/klv.md`](/docs/guides/klv.md) — generic KLV substrate
  plus typed ST 0601 / ST 0102 / ST 0605 / ST 0903 layers, ST 0806 RVT,
  ST 1010 SDCC error covariance, and the ST 0805 KLV→CoT conversion layer.
- [`/docs/guides/codec.md`](/docs/guides/codec.md) — stateless H.264 /
  H.265 / H.266 / AV1 parameter-set parsers off demuxer NAL / OBU bytes.
- [`/docs/troubleshooting.md`](/docs/troubleshooting.md) — symptom →
  diagnosis → fix for build, connection, KLV, framing, and reconnect
  issues.
- [`/docs/reference/compatibility.md`](/docs/reference/compatibility.md)
  — feature-by-feature support matrix (SRT options, MISB specs, typed
  ST 0601 items, codecs, platforms).
