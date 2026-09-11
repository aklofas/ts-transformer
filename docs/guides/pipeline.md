# Pipeline Composition Guide


> **Who this is for:** You're composing the higher-level pipeline shells — `MuxSender`, `Sender`, `DemuxReceiver`, `Receiver`, plus the `ManagedTransport` reconnect wrappers — and want to know how the pieces fit together.

> **You will learn:**
> - The 4-quadrant matrix: `MuxSender` / `Sender` / `DemuxReceiver` / `Receiver` and when to pick each
> - How `Transport` + `RecvTransport` traits decouple the pipeline from libsrt
> - How `ManagedTransport` wraps a `Transport` for automatic reconnect with exponential backoff
> - The `add_byte_sink` fan-out pattern for tee-ing streams to multiple consumers
> - How `Pairer` aligns KLV records with video access units (sync + sample-and-hold)
> - When to use `RawSender` / `RawReceiver` for already-muxed bytes

## Introduction

When you want a ready-made path from typed inputs (NAL units + KLV blobs, or
pre-muxed TS bytes, or arbitrary application messages) to an SRT socket —
optionally with reconnect and a gap buffer — `tst_pipeline` is the composition
layer. It wires `mpegts::mux::Muxer`, the KLV codecs, and `srt::Socket` into
ergonomic sender shells. The shells are thin: their job is to glue framing,
metadata typing, and wire transport together with a stable contract, not to
invent new behaviour.

Reach for `pipeline::*` when you want a ready-made path from NAL units
plus KLV blobs (or pre-muxed TS bytes, or arbitrary application
messages) to an SRT socket, optionally with reconnect and a gap
buffer. Reach past it — straight to `mpegts::mux::Muxer` or
`srt::Socket` — when you have a specialised need (custom buffering,
non-SRT wire, your own reconnect strategy).

For the higher-level composition story, see
[reference/architecture.md](/docs/reference/architecture.md). This guide assumes that vocabulary.

## The composition model

```
                   ┌────────────────────────────────┐
                   │  MuxSender / Sender / RawSender │  (3 sender shells)
                   │  generic over T: Transport     │
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

The two axes are orthogonal. Shells differ by what they accept on the
input side; transports differ by what they talk to on the output side.
Pick a shell based on what you have, a transport based on where it's
going, and they plug together.

## Picking a sender

Decision tree:

- **NAL units plus KLV blobs → `MuxSender`.** Auto-muxes through an
  internal `mpegts::mux::Muxer`; internally synchronized so
  `send_video` and `send_klv` are safe to call concurrently from
  different threads. Lossless across transient transport failures —
  drained-but-not-yet-sent bytes are retained in `pending_bytes` and
  drained first on the next call.
- **Pre-muxed TS bytes → `Sender`.** 3-byte TS sync verification,
  7-packet bundling for the canonical 1316-byte SRT payload size.
  RECOVER mode auto-resyncs to the next sync byte after loss; STRICT
  mode fails fast on any non-aligned input.
- **Arbitrary byte-blind messages → `RawSender`.** One `send` call
  equals one outbound SRT message of the exact length you passed. No
  buffering, no framing, no accumulation.

See [reference/architecture.md](/docs/reference/architecture.md)'s "Why three sender shells" for
the rationale against fusing them.

## `MuxSender` walkthrough

```rust,ignore
impl<T: Transport> MuxSender<T> {
    pub fn new(transport: T, config: MuxerConfig) -> Result<Self, MuxError>;
    pub fn send_video(&self, nal: &[u8], pts: Pts90khz, key_frame: bool)
        -> Result<(), MuxSenderError>;
    pub fn send_klv(&self, klv: &[u8], pts: Pts90khz, metadata_service_id: u8)
        -> Result<(), MuxSenderError>;
    pub fn close(&self);
    pub fn is_alive(&self) -> bool;
}
```

`MuxSenderError` is a struct — `{ kind: ShellErrorKind, source:
MuxSenderErrorSource }`. The `source` enum has two variants:
`Mux(MuxError)` for muxer-side failures (`BufferFull`, `KlvTooLarge`,
`InvalidNal`) and `Transport(TransportError)` for transport-side
failures. Both `MuxError` and `TransportError` convert straight into a
`MuxSenderError` via `#[from]` (the `kind` is derived for you).

An internal `Mutex` wraps the muxer, the transport, and `pending_bytes`.
Concurrent `send_video` / `send_klv` calls are correct but serialize.
The lock is held across push → mux drain → transport send so
back-pressure is honoured end-to-end.

`pending_bytes` is unbounded — the bare `MuxSender` has no cap on how
many drained-but-unsent chunks accumulate during prolonged transport
unavailability. Wrap with `ManagedTransport` when you expect outages
longer than a fraction of a second.

**Sync KLV is muxer-side wrapped.** When the underlying `MuxerConfig` is
set for `KlvStreamType::SynchronousMetadata` plus `carries_pts: true`,
the muxer auto-prepends a 5-byte `Metadata_AU_cell` header per ITU-T
H.222.0 V9 § 2.12.4.2 before TS-framing. `MuxSender::send_klv` passes
your raw KLV LS bytes through to the muxer; the muxer does the wrap.
PTS lives in the PES header (per § 2.12.4.1). See
[guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) for the wire-format details.

Mirroring [../examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs):

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_srt::{SocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = SocketBuilder::new()
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    let sender = MuxSender::new(SrtTransport::new(socket), MuxerConfig::default())?;
    for i in 0..5i64 {
        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
        let klv = vec![/* ... pre-built ST 0601 ... */];
        sender.send_video(&nal, Pts90khz::new(i * 3000), i == 0)?;
        // metadata_service_id = 0x00 is the default per ST 1402.2 App. B Table 2.
        sender.send_klv(&klv, Pts90khz::new(i * 3000), /*metadata_service_id=*/ 0x00)?;
    }
    sender.close();
    Ok(())
}
```

## `Sender` walkthrough

```rust,ignore
impl<T: Transport> Sender<T> {
    pub fn new(transport: T, config: SenderConfig) -> Self;
    pub fn send_ts(&mut self, bytes: &[u8]) -> Result<(), SenderError>;
    pub fn flush(&mut self) -> Result<(), SenderError>;
    pub fn stats(&self) -> SenderStats;
    pub fn close(&mut self);
    pub fn is_alive(&self) -> bool;
}
```

`send_ts` accepts any number of bytes; the sender does 188-alignment
and 7-packet bundling internally. `flush` emits any buffered partial
bundle so the tail of a finite input reaches the wire. Drop also
best-effort flushes.

`SenderConfig` has two knobs:

- `framing_mode: TsFramingMode::Recover` (default) silently skips
  misaligned bytes until it finds a TS sync byte (counts them in
  `bytes_skipped_for_sync`); auto-resyncs after sync loss.
- `framing_mode: TsFramingMode::Strict` returns
  `TsFramingError::SyncLost { offset }` on any non-aligned input.
- `max_unsynced_bytes: usize` — threshold (in bytes consumed while
  UNSYNCED) above which RECOVER mode flags that sync has not been
  acquired. Default 18,800 (≈100 packets' worth). When scanning for
  sync consumes more than this many bytes without acquiring it,
  `send_ts` returns `TsFramingError::NoSyncAfterLimit`. The counter
  resets after the error is raised, so RECOVER mode keeps scanning
  afterward — the watchdog fires again only after another full
  `max_unsynced_bytes` of unrecovered garbage.

`SenderError` is a `{ kind: ShellErrorKind, source: SenderErrorSource }`
struct; its `source` enum has two variants: `Framing(TsFramingError)`
and `Transport(TransportError)`.

`SenderStats` fields: `bytes_pushed`, `bytes_skipped_for_sync`
(bytes discarded while acquiring or re-acquiring sync, RECOVER mode
only), `resync_events`, `packets_sent` — all `u64`.

Mirroring [../examples/receiving/ts_relay_from_file.rs](/examples/receiving/ts_relay_from_file.rs):

```rust,no_run
use tst_pipeline::{Sender, SenderConfig};
use tst_srt::{SocketBuilder, SrtTransport};
use std::fs::File;
use std::io::Read;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = SocketBuilder::new()
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    let mut sender = Sender::new(SrtTransport::new(socket), SenderConfig::default());
    let mut file = File::open("input.ts")?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        sender.send_ts(&buf[..n])?;
    }
    sender.flush()?;
    sender.close();
    Ok(())
}
```

## `RawSender` walkthrough

```rust,ignore
impl<T: Transport> RawSender<T> {
    pub fn new(transport: T, config: RawSenderConfig) -> Self;
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), RawSenderError>;
    pub fn close(&mut self);
    pub fn is_alive(&self) -> bool;
    pub fn transport(&self) -> &T;
}
```

`RawSenderConfig` is currently empty — reserved as a distinct type so
future additions are non-breaking. `send` validates
`bytes.len() <= transport.max_payload()` before delegating; one `send`
call equals one outbound message. Failures surface as `RawSenderError`,
which wraps the transport's `TransportError` (`#[from]`) and carries a
`ShellErrorKind` for binding-crate error mapping.

Use case: custom protocols where you want SRT's reliability and
encryption but not its TS muxing — control channels, application-level
file transfer, single-message latency measurement.

## The `Transport` trait

```rust,ignore
pub trait Transport: Send {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError>;
    fn max_payload(&self) -> usize;
    fn is_alive(&self) -> bool;
    fn close(&mut self);
}
```

The trait is `Send` but not `Sync` — concurrent sends through one
transport are the caller's responsibility. The shells handle this
internally where their thread-safety contract requires it.

`TransportError` has five variants:

- `Backpressure { msg, errno_code }` — transport is alive but momentarily
  refused the bytes. Retrying the same slice is reasonable. `errno_code`
  carries the wire-level code (libsrt major category for `SrtTransport`)
  or `None` for transports that don't expose one.
- `Broken { msg, errno_code }` — transport is dead; rebuild it (or rely
  on `ManagedTransport` to do so).
- `Closed` — peer closed the connection / end-of-stream observed on the
  wire (bare transports also map a caller-initiated close to `Closed`).
- `TooLarge { len, max }` — message exceeds `max_payload`. Caller is
  responsible for chunking on their own framing semantics.
- `ExplicitClose` — caller invoked `close()` / `cancel()`. Today produced
  only by `ManagedRecvTransport::recv_bytes` when its own cancel signal
  fires; bare `SrtTransport` maps caller-close to `Closed` instead.

Implement `Transport` for any byte sink that isn't an SRT socket: UDP,
file, in-memory test harness, named pipe, TCP, your own protocol. The
shells don't care which.

Mirroring [../examples/sending/custom_transport.rs](/examples/sending/custom_transport.rs):

```rust,no_run
use tst_pipeline::{Transport, TransportError};
use std::sync::{Arc, Mutex};

struct MemTransport {
    packets: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: bool,
    max_payload: usize,
}

impl Transport for MemTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge { len: msg.len(), max: self.max_payload });
        }
        if !self.alive { return Err(TransportError::Closed); }
        self.packets.lock().unwrap().push(msg.to_vec());
        Ok(())
    }
    fn max_payload(&self) -> usize { self.max_payload }
    fn is_alive(&self) -> bool { self.alive }
    fn close(&mut self) { self.alive = false; }
}
```

## `SrtTransport` — the canonical impl

```rust,ignore
impl SrtTransport {
    pub const DEFAULT_PAYLOAD: usize = 1316;
    pub fn new(socket: Socket) -> Self;
    pub fn with_max_payload(self, n: usize) -> Self;
}
```

`SrtTransport::new` wraps an already-connected `Socket`. Configure the
socket (passphrase, latency, stream id, etc.) before wrapping —
`SrtTransport` doesn't expose libsrt knobs of its own. `max_payload`
defaults to 1316 (libsrt's `SRTO_PAYLOADSIZE` default) and is NOT
queried from the socket; for a non-default payload size, call
`with_max_payload`.

Error mapping from `SendError` → `TransportError`:

| `SendError` | `TransportError` |
| --- | --- |
| `TimedOut` / `QueueFull` | `Backpressure(...)` |
| `PayloadTooLarge { actual, .. }` | `TooLarge { len: actual, max }` |
| `ConnectionBroken` / `System(_)` | `Broken(...)` (drops socket) |
| `Other { kind: SrtErrno::Async, .. }` | `Backpressure(message)` |
| `Other { .. }` | `Broken(message)` (drops socket) |

When a transport is marked broken, the wrapped `Socket` is dropped
internally — subsequent `send_bytes` calls return `Closed` until a
new `SrtTransport` is built.

### Default sender SocketConfig overrides (tst-c connect path)

When you use the C ABI's `tst_*_open` family or call
`tst_c::connect::connect_srt` directly, the underlying `SocketConfig`
gets these overrides applied (only if the user hasn't set them):

| Field | Default | libsrt default | Why |
| --- | --- | --- | --- |
| `connect_timeout` | 15 s | 3 s | Radio links: LOS-over-terrain, antenna repointing, radio warm-up |
| `linger` | 5 s | off (0 s; drains in background) | Bounds an intentional synchronous drain on close — libsrt's own default returns immediately without waiting for the backlog to flush |
| `role` | `Role::Sender` | `Role::Receiver` (default) | Sets `SRTO_SENDER=1` for HSv4-peer compatibility |

Pure-Rust users who build a `SrtTransport` via `SocketBuilder` directly
do **not** get these defaults — set them explicitly via the builder if
needed. The defaults live in the `tst-c` connect path because that's
where the canonical "default sender Socket" is constructed.

## `ManagedTransport<T>` — reconnect + gap buffer

```rust,ignore
impl<T: Transport + 'static> ManagedTransport<T> {
    pub fn new<F>(inner: T, factory: F, policy: ReconnectPolicy) -> Self
    where F: Fn() -> Result<T, TransportError> + Send + Sync + 'static;
}
```

`ManagedTransport<T>` itself implements `Transport`, so any sender
shell composes with it transparently — the shell sees a `Transport`,
plain or wrapped.

The factory closure is what the manager calls to rebuild the inner
transport. Most callers wire it as a closure running
`SocketBuilder::new()...connect(addr).map(SrtTransport::new)`. The
closure must be `Fn + Send + Sync + 'static` so the manager can hold
it in an `Arc<dyn Fn …>`.

Behaviour on `send_bytes`: drain any queued gap-buffer bytes first,
then try the new bytes. `Backpressure` and `TooLarge` propagate to
the caller without triggering reconnect. `Broken` or `Closed` queues
the new bytes into the gap buffer (subject to `OverflowPolicy`) and
calls the factory with the configured backoff until a fresh transport
materialises or `max_attempts` is exhausted. On reconnect the gap
buffer drains before the call returns.

`ReconnectPolicy.mode` picks where that reconnect loop runs. The
default, `ReconnectMode::Blocking`, runs it on the calling thread — a
single `send_bytes` call may block for the full reconnect window.
Opt in to `ReconnectMode::Background` and a per-outage worker thread
owns the factory/backoff/drain loop instead: while it's active (or the
gap buffer is non-empty) `send_bytes` enqueues under `overflow_policy`
without waiting on backoff or the factory call — `Ok(())` means
*accepted*, not *delivered*. It can still block briefly on lock
contention while the worker is mid-drain (bounded to at most one
in-flight inner send). This is still a synchronous API, not an async
runtime; see the
[cookbook recipe](/docs/cookbook/operations/managed-transport-reconnect.md#background-mode-never-stall-the-producer)
for when to reach for it. True async/reactor exposure remains on the
deferred-features list; see
[project/deferred-features.md](/docs/project/deferred-features.md).

## `ReconnectPolicy`

```rust,ignore
pub struct ReconnectPolicy {
    pub max_attempts: Option<u32>,
    pub backoff: BackoffStrategy,
    pub gap_buffer_capacity: usize,
    pub overflow_policy: OverflowPolicy,
    pub mode: ReconnectMode,
}
```

Defaults (`ReconnectPolicy::default()`):

- `max_attempts: Some(10)` — give up after ten reconnect attempts and
  surface `Broken("reconnect gave up after 10 attempts")` to the
  caller. Set to `None` to retry forever.
- `backoff: BackoffStrategy::Exponential { base: 100ms, max: 10s }` —
  see §11 for the actual variants.
- `gap_buffer_capacity: 256` — messages.
- `overflow_policy: OverflowPolicy::DropOldest` — see §12.
- `mode: ReconnectMode::Blocking` — reconnect on the caller's thread.
  Set to `ReconnectMode::Background` to run reconnect on a per-outage
  worker thread instead; see the cookbook recipe linked above.

## `BackoffStrategy`

Two variants:

- `Constant(Duration)` — fixed wait. Use for tests or controlled
  environments.
- `Exponential { base: Duration, max: Duration }` —
  `wait = base * 2^(attempt - 1)`, capped at `max`. Default
  `base = 100ms`, `max = 10s`. Use for production.

There is no jitter variant. Exponential without jitter can create
thundering-herd issues with many simultaneous clients reconnecting to
the same peer; for the typical single-sender deployment this is fine.

## `OverflowPolicy`

Two variants:

- `DropOldest` (default) — when the gap buffer is full, drop the
  oldest queued message to make room. Counts the drop in
  `messages_dropped` / `bytes_dropped`.
- `Reject` — return `GapBufferError::Full` from the enqueue path; the
  caller decides what to do.

Trade-off: `DropOldest` keeps the receiver caught up to "now" once
reconnect lands, at the cost of losing the tail of the gap — the
right call for live video. `Reject` preserves every message at the
cost of stalling once the disconnect window exceeds the buffer; reach
for it when correctness matters more than freshness (telemetry,
control-plane messages, audit-logged events).

## Choosing a backoff and gap-buffer size

Reasonable starting point: `BackoffStrategy::Exponential { base: 100ms,
max: 10s }` plus

```text
gap_buffer_capacity = ceil(max_disconnect_window × send_rate)
```

Worked example: a five-second worst-case disconnect on a 30 fps stream
with one KLV record per frame produces 150 video messages plus 150 KLV
messages = 300 entries. Round up to 512. The 256-message default fits
roughly 4 seconds of that load.

If your worst-case window is longer than the buffer can hold, either
raise the capacity (cheap — each entry is a `Vec<u8>` of at most
`max_payload` bytes) or accept `DropOldest` as a deliberate
freshness-over-completeness choice.

## Failure semantics

Where errors come from:

- `MuxSenderError::Mux` — the encoder produced something the muxer
  rejects (`BufferFull`, `KlvTooLarge`, `InvalidNal`). Rare in
  practice with reasonable buffer sizing.
- `MuxSenderError::Transport(TransportError)` — the transport layer
  reported an error (`MuxSender`).
- `SenderError::Framing(TsFramingError)` — STRICT mode rejected
  unaligned input (`SyncLost`). RECOVER mode returns
  `NoSyncAfterLimit` once scanning for sync consumes more than
  `max_unsynced_bytes` without acquiring it — see `max_unsynced_bytes`
  under the `SenderConfig` knobs above.
- `SenderError::Transport(TransportError)` — transport error (`Sender`).
- `RawSender::send` returns `RawSenderError`, which wraps the underlying
  `TransportError`.

With `ManagedTransport` wrapping the inner transport, transient
`Broken` errors are absorbed and the caller's `send_*` call appears to
succeed once reconnect lands. Only `Closed` (after `max_attempts`
exhausted) and `TooLarge` propagate. With a bare transport, every
`Broken` propagates — you reconnect by rebuilding the transport and
re-creating the sender.

## Threading

- `MuxSender` is internally synchronized. `send_video` and `send_klv` are
  safe to call concurrently; calls serialize on the internal mutex.
- `Sender` and `RawSender` are not internally synchronized. Wrap in
  an external `Mutex` if shared across threads, or — simpler — use
  thread-per-connection.
- `ManagedTransport` itself is internally synchronized.

Don't over-engineer this. The typical deployment has one sender per
process, or a small fixed number of senders each on its own thread.

## Receive side

The receive shells mirror the send shells. They differ by what they
emit; all three are generic over a `RecvTransport` (the receive
counterpart to `Transport`).

### Picking a receiver

- **Typed events out → `DemuxReceiver`.** Composes `Receiver → Demuxer`;
  emits `DemuxEvent` per call. Auto-flushes the demuxer's reassembly
  state on `TransportError::Closed`. The default for "I want a stream
  of NALs and KLV records out of an SRT socket."
- **TS-aligned packets out → `Receiver`.** One 188-byte aligned TS
  packet per `next_packet` call. Internal sync recovery via the
  HUNT/VERIFY/LOCKED state machine. Use when you want to feed bytes
  into your own demuxer (FFmpeg, JavaCV, Bento4).
- **One byte vec per recv → `RawReceiver`.** No TS framing, no demux —
  one transport message per `recv_one` call, returned as `Vec<u8>`.
  Use as the receive counterpart to `RawSender`, or as a building
  block for tests.

### `DemuxReceiver` walkthrough

```rust,ignore
impl<R: RecvTransport> DemuxReceiver<R> {
    pub fn new(transport: R) -> Self;
    pub fn with_demux_options(transport: R, options: DemuxerConfig) -> Self;
    pub fn add_byte_sink(&mut self, sink: ByteSink);
    pub fn recv_event(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError>;
    pub fn is_alive(&self) -> bool;
    pub fn close(&mut self);
}
```

`DemuxReceiver` also implements `Iterator<Item = Result<DemuxEvent,
DemuxReceiverError>>`, so `for result in &mut rx` is the idiomatic drain
pattern. Iterator termination (`for` loop simply ends) is the
clean-EOF signal — `recv_event` returned `Ok(None)` after auto-flushing
the demuxer.

`DemuxReceiverError` is a `{ kind: ShellErrorKind, source:
DemuxReceiverErrorSource }` struct; its `source` enum has two variants:
`Transport(TransportError)` for transport failures and `Demux(DemuxError)`
for strict-mode rejections, unrecoverable sync loss, or malformed PES.

```rust,no_run
use tst_core::mpegts::demux::DemuxEvent;
use tst_pipeline::DemuxReceiver;
use tst_srt::{ListenerBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = ListenerBuilder::new()
        .latency(Duration::from_millis(120))
        .bind("0.0.0.0:9000")?;
    let (socket, _peer) = listener.accept()?;
    let mut rx = DemuxReceiver::new(SrtTransport::new(socket));
    for item in &mut rx {
        match item? {
            DemuxEvent::ProgramMap(m) => println!("PMT streams={}", m.streams.len()),
            DemuxEvent::Sample { stream, pts, .. } => {
                println!("Sample PID=0x{:04X} pts={pts}", stream.pid);
            }
            _ => {}
        }
    }
    Ok(())
}
```

Mirroring [../examples/receiving/srt_recv_typed.rs](/examples/receiving/srt_recv_typed.rs).

### `add_byte_sink` fan-out

```rust,ignore
pub type ByteSink = Box<dyn FnMut(&[u8]) + Send>;

impl<R: RecvTransport> DemuxReceiver<R> {
    pub fn add_byte_sink(&mut self, sink: ByteSink);
}
```

Register a callback that sees every 188-byte TS packet pulled from the
transport, in registration order, before the demuxer parses them. The
canonical "save raw `.ts` to disk AND parse for KLV in one pass"
workflow. Multiple sinks may be registered; each sees the same bytes.

Contract:

- Sinks fire once per TS packet (188 bytes — NOT 1316; the demuxer
  pulls 1316-byte SRT messages and breaks them down to packet-aligned
  chunks before the sinks fire).
- Sinks fire in registration order, all before the demuxer feed.
- The slice is valid only for the duration of the call. Copy bytes
  into an owned buffer if they need to outlive the callback.
- Sinks must not panic — a panic unwinds through `recv_event`.
- Sinks run synchronously on the receive thread. For high-throughput
  workflows or expensive work, push to a channel and let a worker
  thread do the slow work.

Mirroring [../examples/operations/tee_disk_and_demux.rs](/examples/operations/tee_disk_and_demux.rs).

### `Receiver` and `RawReceiver`

```rust,ignore
impl<R: RecvTransport> Receiver<R> {
    pub fn new(transport: R, config: ReceiverConfig) -> Self;
    pub fn next_packet(&mut self) -> Result<[u8; 188], ReceiverError>;
    pub fn is_alive(&self) -> bool;
    pub fn close(&mut self);
}

impl<R: RecvTransport> RawReceiver<R> {
    pub fn new(transport: R, config: RawReceiverConfig) -> Self;
    pub fn recv_one(&mut self) -> Result<Vec<u8>, RawReceiverError>;
    pub fn is_alive(&self) -> bool;
    pub fn close(&mut self);
}
```

Both are simple drain loops. `Receiver` runs the syncer state machine
internally — feed bytes from any source, get out 188-byte aligned TS
packets. `RawReceiver` is the simplest possible shell; one transport
message per call.

### `RecvTransport` trait

```rust,ignore
pub trait RecvTransport: Send {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    fn max_payload(&self) -> usize;
    fn is_alive(&self) -> bool;
    fn close(&mut self) {}
}
```

Receive-side counterpart to `Transport`. `SrtTransport` implements
both — the same wrapper handles both directions on a connected socket.
`close` is defaulted as a no-op so test mocks and channel-backed
implementors can opt in only when they own a tear-down resource.

`recv_bytes` returns the number of bytes written. Returns
`TransportError::Closed` once the transport is closed or the connection
has been broken; `TransportError::Backpressure` on a recv timeout
(transport still alive, caller may retry).

Whether the timeout arm can fire is per-transport configuration: SRT
via `SocketBuilder::recv_timeout` / `Socket::set_recv_timeout`
(`SRTO_RCVTIMEO`), RTP via `RtpRecvTransport::set_recv_timeout`. With
one configured, a `DemuxReceiver` pump regains control on every
deadline tick as a retryable `Backpressure`-kind error — a stall
watchdog for a stalled-but-healthy session (peer stops sending; no
error, no EOS) with no cancel thread.

### `ManagedRecvTransport<R>`

```rust,ignore
impl<R: RecvTransport> ManagedRecvTransport<R> {
    pub fn new(
        inner: R,
        factory: Box<dyn FnMut() -> Result<R, TransportError> + Send>,
        policy: ReconnectPolicy,
    ) -> Self;
}
```

Sibling to `ManagedTransport<T>` for the receive direction — same
factory-closure + `ReconnectPolicy` cadence pattern.

**No gap buffer.** Receive-side bytes that never arrived can't be
replayed, so the decorator simply restarts the recv loop on a fresh
transport when the inner returns `Closed` or `Broken`. The demuxer-side
state (sync alignment, PES reassembly) carries over across reconnect,
which costs at most one re-VERIFY pass on the syncer.

The factory is `Box<dyn FnMut + Send>` rather than the send-side
`Arc<dyn Fn + Send + Sync>` because `recv_bytes(&mut self, …)` is
exclusive-mutable — there's no concurrent close-from-any-thread
requirement to design around.

### `ManagedDemuxReceiver<R>`

```rust,ignore
impl<R: RecvTransport> ManagedDemuxReceiver<R> {
    pub fn new(
        inner: ManagedRecvTransport<R>,
        config: ManagedDemuxReceiverConfig,
    ) -> Self;
    pub fn with_demux_options(
        inner: ManagedRecvTransport<R>,
        options: DemuxerConfig,
        config: ManagedDemuxReceiverConfig,
    ) -> Self;
    pub fn recv_event(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError>;
}
```

Sibling shell to `DemuxReceiver<R>` that wraps a
`ManagedRecvTransport<R>` and integrates the reconnect lifecycle with
the demuxer's sync-recovery state.

**What it adds over `DemuxReceiver<ManagedRecvTransport<R>>`:** on each
underlying-transport reconnect, `ManagedDemuxReceiver` calls
[`Demuxer::reset_sync`](#demuxerreset_sync) and emits a
`DemuxEvent::ReconnectDiscontinuity` event so consumers can mark a hard
discontinuity in their downstream state (PTS reset, PSI re-fetch, etc.)
rather than guessing from the byte stream. The 188-byte sync rail is
re-VERIFIED on the first packet from the new transport — the demuxer
does *not* assume the new connection picks up where the old one left
off.

**Data-loss budget on reconnect.** A few unfinished bytes from the old
transport may be discarded when sync is rebuilt; the drop is bounded by
`SocketConfig::max_payload` (typically ≈ 7 TS packets, never an entire
flow). See the rustdoc on `ManagedDemuxReceiver` for the exact
contract; cookbook recipe 22 has a runnable companion.

### Stream-end contract

The receive surface distinguishes three end-of-stream signals:

- **Iterator termination (`for` loop ends naturally).** The clean-EOF
  signal. Fires when the underlying `RecvTransport` returns
  `TransportError::Closed`, which `DemuxReceiver::recv_event` translates
  into `Ok(None)` after first calling `Demuxer::flush()` to recover
  any trailing PES. Loop callers do not see `Closed` as an `Err`.
- **`Err` whose `source` is `DemuxReceiverErrorSource::Transport(TransportError::Broken { .. })`** (`kind = ShellErrorKind::TransportBroken`). Peer-
  initiated cleanup or unrecoverable link. On `tst-tcp` a clean peer
  FIN (or TLS `close_notify`) is also `Broken`, marked
  `cause: BrokenCause::CleanEof` so a caller can tell it from a read
  error without parsing the message. `SrtTransport` collapses
  these into one `Broken` surface by design — it lets a managed-receive
  decorator distinguish a self-initiated close (`Closed`) from a peer-
  initiated break (`Broken`). On `Broken` the demuxer is NOT auto-
  flushed (the receive thread can't tell mid-stream hiccup from a
  clean end).
- **`Err` whose `source` is `DemuxReceiverErrorSource::Demux(_)`.** Strict-mode rejection or
  malformed PES. Re-entry into `recv_event` after a `MalformedPes` is
  discouraged — the demuxer's reassembly state is undefined past a bad
  PES header. Treat as stream-fatal until lenient PES recovery lands.

## KLV ↔ video pairing (`tst_pipeline::ext::pairing`)

The demuxer emits independent stream-tagged events; it does not pair
sync-KLV with video AUs. This is a deliberate design choice: pairing
tolerance, sample-and-hold semantics, and multi-stream routing are
domain decisions the library cannot make correctly without consumer
context.

For consumers who would otherwise reimplement the same nearest-PTS or
sample-and-hold pattern, `tst_pipeline::ext::pairing::Pairer` is an opt-in
convenience.

### When to reach for `Pairer`

Use it when:

- You're pairing sync-KLV at video frame rate (one KLV per frame).
- You're sample-and-holding async-KLV (1–10 Hz) against video frames.
- You want telemetry counters for pairing rate.
- You want a typed `(VideoSample, KlvSample)` boundary instead of
  re-matching `DemuxEvent` arms after the pair.

Stay with the inline `DemuxEvent` match (cookbook recipes 12–14) when:

- You have non-canonical pairing semantics (e.g., custom multi-stream
  routing, KLV-driven indexing into a separate timeline, etc.).
- You want full visibility over every event with no library state in
  between.

### Strategy chooser

| Pattern | Constructor | Mode |
|---|---|---|
| Sync-KLV at frame rate, low-latency consumer | `Pairer::with_config(...)` | `PairerMode::Realtime` |
| Sync-KLV at frame rate, batch / archival ingest | `Pairer::with_config(...)` | `PairerMode::Buffered { max_lag: Duration::from_secs(2) }` |
| Async-KLV (1–10 Hz) against video frames | `Pairer::last_before_pts(...)` | n/a (past-only) |
| EO + IR sharing one async-KLV stream | Two `Pairer::last_before_pts` instances side-by-side | n/a |

See cookbook [Recipes 24–27](../cookbook/index.md#-receiving--consume-a-ts-stream-includes-klv-to-video-pairing) for runnable patterns.

### What you give up

The pairer is video-driven and consumes events on the configured
`video_pid` and `klv_pid`. Off-route events (other PIDs,
`ProgramMap`, `NonConformant`, `Discontinuity`, audio, subtitles) flow
through unchanged via `PairerOutput::PassThrough`, so topology
discovery and diagnostics are preserved. But a single `Pairer` is
single-pair: multi-video shapes (EO+IR) compose at the call site with
two instances, not via a single multi-pair builder.

The pairer's C ABI / JNI / UniFFI exposure is deferred to the future
receiver-surface plan — Rust API only for now.

## Out-of-band cancellation

By default `MuxSender` (and `DemuxReceiver`, `Receiver`, etc.) hold their
underlying transport behind an internal lock. A naive `close()` would
have to wait for any in-flight `send_*`/`recv_*` call to return before
it could acquire that lock — which means a thread parked inside
libsrt's `srt_sendmsg` (e.g. when the peer stopped draining and
back-pressure built up) would block the close indefinitely.

Every shell exposes a `cancel_handle()` that returns a clone-able,
`Send + Sync` token. Calling `.cancel()` on that token from any thread
atomically closes the underlying SRT handle, which causes any thread
parked inside libsrt to return promptly (typically as
`TransportError::Broken`).

`MuxSender::close()` already does this internally — a `close()` call from
a watchdog thread wakes any sender thread parked inside `send_video`
within milliseconds, then completes. (The flip side of that promptness:
pending bytes retained from a prior transient send error are abandoned.
`MuxSender::finish()` is the graceful counterpart — it drains them to
the live transport first and reports failure, at the cost of possibly
blocking like `Drop`.) You only need to grab a
`cancel_handle()` directly when you want to keep the shell alive but
still wake a worker:

```rust,ignore
let s = Arc::new(MuxSender::new(transport, config)?);
let cancel = s.cancel_handle().expect("real SRT transport supports cancel");

// Worker thread parks in s.send_video(...) when peer back-pressures.
let s_worker = s.clone();
std::thread::spawn(move || s_worker.send_video(&nal, pts, true));

// On a SIGINT or watchdog timeout, wake the worker out-of-band:
cancel.cancel();
```

`cancel()` is idempotent: calling it many times (or from multiple
threads concurrently) closes the SRT handle exactly once.

## Examples

Eight runnable examples cover the pipeline surface — four send, four receive.

Send side:

- [../examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs)
  — `MuxSender` → `SrtTransport` → connected SRT socket. The canonical
  setup.
- [../examples/receiving/ts_relay_from_file.rs](/examples/receiving/ts_relay_from_file.rs)
  — `Sender` reading a `.ts` file and relaying to an SRT peer.
- [../examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs)
  — `ManagedTransport<SrtTransport>` plus a deliberately flaky peer
  thread; demonstrates the reconnect + gap-buffer + drain cycle
  end-to-end.
- [../examples/sending/custom_transport.rs](/examples/sending/custom_transport.rs)
  — implementing the `Transport` trait against an in-memory byte
  collector. Template for any non-SRT sink.

Receive side:

- [../examples/receiving/srt_recv_typed.rs](/examples/receiving/srt_recv_typed.rs)
  — `DemuxReceiver` → `SrtTransport` → typed `DemuxEvent` stream from a
  live SRT peer. Mirror of `send_pipeline_to_socket.rs`.
- [../examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs)
  — `Demuxer` driven by a file (no transport). Triage-grade
  diagnostic for any `.ts` capture.
- [../examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs)
  — nearest-PTS pairing of KLV records with video AUs (Cookbook §12).
- [../examples/operations/tee_disk_and_demux.rs](/examples/operations/tee_disk_and_demux.rs)
  — `add_byte_sink` fan-out: write `.ts` to disk while consuming
  typed events, in a single pass.

## See also

- **Runnable example:** `cargo run -p tst-examples --example send_pipeline_to_socket` — [examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs)
- [guides/srt.md](/docs/guides/srt.md) — the `Socket` and `Listener` that sit behind `SrtTransport`.
- [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — the muxer that `MuxSender` wraps.
- [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — the demuxer that `DemuxReceiver` wraps.
