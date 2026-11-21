# Pipeline Composition Guide


> **Who this is for:** You're composing the higher-level pipeline shells — `MuxSender`, `Sender`, `DemuxReceiver`, `Receiver`, plus the `ManagedTransport` reconnect wrappers — and want to know how the pieces fit together.

> **You will learn:**
> - The 4-quadrant matrix: `MuxSender` / `Sender` / `DemuxReceiver` / `Receiver` and when to pick each
> - How `Transport` + `RecvTransport` traits decouple the pipeline from libsrt
> - How `ManagedTransport` wraps a `Transport` for automatic reconnect with exponential backoff
> - Counting demuxed bytes and composing `Receiver` + `Demuxer` for raw access
> - How `Pairer` aligns KLV records with video access units (sync + sample-and-hold)
> - When to use `RawSender` / `RawReceiver` for already-muxed bytes

## Introduction

`tst-pipeline` connects media processing to network I/O. Its **shells** are
wrappers that combine common steps: for example, `MuxSender` muxes encoded
video and KLV into MPEG-TS, then sends the resulting bytes through a
transport. `DemuxReceiver` receives bytes and turns them into demux events.

Choose a shell for the data you have and a transport for the connection
you need. The shells use the `Transport` and `RecvTransport` traits, so
they can work with SRT, other supported transports, or your own
implementation. Add a managed wrapper when you need automatic reconnect.

Use `Muxer` or `Demuxer` directly when your application already handles
I/O. Use a raw socket when it already handles framing. The
[architecture reference](/docs/reference/architecture.md) explains the
crate boundaries in more detail.

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
  drained first on the next call, re-split to the transport's current
  `max_payload()`. A ceiling below one TS packet (188 bytes) is refused
  with `TooLarge` before the push, so the call's input is not consumed.
- **Pre-muxed TS bytes → `Sender`.** 3-byte TS sync verification and
  7-packet bundles, sent to the transport in whole TS packets that fit
  its `max_payload()`: one 1316-byte message at the SRT default, split
  when the ceiling is smaller. RECOVER mode auto-resyncs to the next
  sync byte after loss; STRICT mode fails fast on any non-aligned
  input.
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

`pending_bytes` holds the muxed output a failed transport send left
behind, and it does not grow with the length of an outage. Every
`send_*` drains it first; if that drain fails, the call returns with
`input_consumed: Some(false)` before its own input reaches the muxer, so
the caller still owns that input and decides whether to retry or drop it.
Only a call whose input was muxed (`input_consumed: Some(true)`) leaves
output in `pending_bytes`, and the next call sends it exactly once
(`close` tries once, best effort). During an outage the bare `MuxSender` therefore pushes the errors
back to the producer rather than buffering for it. Wrap the transport
with `ManagedTransport` when you want outages ridden out by reconnect and
a gap buffer instead.

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
use tst_srt::{SrtSocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = SrtSocketBuilder::new()
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
    pub fn finish(&mut self) -> Result<(), SenderError>;
    pub fn stats(&self) -> SenderStats;
    pub fn close(&mut self);
    pub fn is_alive(&self) -> bool;
}
```

`send_ts` accepts any number of bytes; the sender does 188-alignment
and 7-packet bundling internally. Each bundle goes to the transport in
whole TS packets that fit `max_payload()`: 1316 bytes, seven packets, is
one message at the SRT default, and a smaller ceiling splits the bundle
into several messages. A ceiling below one TS packet is refused with
`TransportError::TooLarge` before the input is touched
(`input_consumed = Some(false)`); bundles already queued stay queued
for a later call. `flush` emits any buffered partial bundle so the tail
of a finite input reaches the wire, and it checks every packet's sync
byte before sending: on damage it sends the packets before it, counts
one `resync_events` and keeps the rest buffered for the next `send_ts`
to resync. A stream of one or two packets goes out too. STRICT mode
has already checked every boundary. RECOVER mode flushes a stream that
has been aligned since its first byte (no byte skipped, no boundary
failed) on the same evidence: the whole packets held are sent when every
boundary present reads `0x47`, the first byte of a partial packet
included, so one stray trailing byte that is not `0x47` holds a one- or
two-packet stream back (`finish` reports it). Once a byte has been
skipped or a boundary has failed, only three consecutive sync bytes
send anything.

A flush is the caller's word that what it sent so far is a stream, and
it costs something when the start only looks aligned. A `flush` while
the sender holds the stream's first 188 to 376 bytes, with `0x47` at
offset 0 and (when more than 188 bytes are held) at offset 188, sends
those bytes as one or two packets although they are not packets. When
they were the head of a real stream lying off that 188-byte grid (a
stream joined mid-packet whose first byte happens to read `0x47`), the
real packets that start inside them are lost with them: one when 188
bytes are sent, two when 376 are; without the flush the sender finds the
real grid and delivers them. The sender is then in sync on the wrong
grid, so a later `send_ts` or `flush` can send another 188 bytes that
are not a packet (each further one when its first byte reads `0x47`)
before a boundary fails and it resyncs, and each costs the real packet
that starts inside it. This needs a flush (or an end) inside the first
376 bytes; a flush with fewer than 188 bytes held sends nothing; for
random bytes it happens about once in 256 starts. STRICT sends the same
bytes and then refuses what follows with an error.

`finish` ends the stream: it sends what `flush` sends, then makes one
end-of-stream pass over what is still held, so three or more
consecutive good packets behind damage, which a later `send_ts` would
have verified, are sent now (the pass scans everything held, also
behind more garbage than `max_unsynced_bytes`). Then it closes the
transport without cancelling it and returns the first send error, or
else `TsFramingError::UnsentTail { bytes }` (kind `InputMalformed`) for
what it could not send as whole aligned TS packets: a partial last
packet (in both modes), fewer than three packets behind skipped bytes,
bytes that are not TS. The count is what was still held at the end;
bytes the scan skipped on the way, a good packet in front of early
damage among them, show only in `bytes_skipped_for_sync`. A send that
the transport refuses during `finish` is its result, and what it left
is abandoned with the sender: `finish` makes one attempt. To retry a
refused send, call `flush` until it succeeds before `finish`; packets
that only the end-of-stream pass recovers still get one attempt.
`close` and drop make the same end-of-stream pass and report nothing: a
send error is dropped, with what it left, without a log line; otherwise
bytes they could not send leave one `tracing` warning with their
count. A binding's `close()` cancels first and sends nothing
buffered; end a binding's sender with `finish()` (C has none: flush,
then close).

`SenderConfig` has two knobs:

- `framing_mode: TsFramingMode::Recover` (default) silently skips
  misaligned bytes until it finds a TS sync byte (counts them in
  `bytes_skipped_for_sync`); auto-resyncs after sync loss. On a lost
  sync it sends the verified packets before the damage, keeps the data
  after it and resumes the scan at the damaged boundary, so only the
  damaged bytes are skipped and counted.
- `framing_mode: TsFramingMode::Strict` returns
  `TsFramingError::SyncLost { offset }` on any non-aligned input. It
  checks every packet boundary of the call before taking any of it, so
  a rejected call changes nothing (buffer, stats and sync state) and
  reports `input_consumed = Some(false)`: retrying corrected input is
  safe.
- `max_unsynced_bytes: usize` — threshold (in bytes consumed while
  UNSYNCED) above which RECOVER mode flags that sync has not been
  acquired. Default 18,800 (≈100 packets' worth). When scanning for
  sync consumes more than this many bytes without acquiring it,
  `send_ts` returns `TsFramingError::NoSyncAfterLimit`. The counter
  resets after the error is raised, so RECOVER mode keeps scanning
  afterward — the watchdog fires again only after another full
  `max_unsynced_bytes` of unrecovered garbage. A call that frames a
  bundle before the limit is crossed returns that bundle, and the error
  surfaces on the next call.

`SenderError` is a `{ kind: ShellErrorKind, source: SenderErrorSource }`
struct; its `source` enum has two variants: `Framing(TsFramingError)`
and `Transport(TransportError)`.

`SenderStats` fields: `bytes_pushed`, `bytes_skipped_for_sync`
(bytes discarded while acquiring or re-acquiring sync, RECOVER mode
only), `resync_events`, `packets_sent` — all `u64`.

Mirroring [../examples/receiving/ts_relay_from_file.rs](/examples/receiving/ts_relay_from_file.rs):

```rust,no_run
use tst_pipeline::{Sender, SenderConfig};
use tst_srt::{SrtSocketBuilder, SrtTransport};
use std::fs::File;
use std::io::Read;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = SrtSocketBuilder::new()
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
    // Provided (default `None`): override to expose a cross-thread cancel
    // handle and per-socket statistics.
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> { None }
    fn socket_stats(&self) -> Option<SocketStats> { None }
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
- `Broken { msg, errno_code, cause }` — transport is dead; rebuild it (or
  rely on `ManagedTransport` to do so). `cause` is `BrokenCause::CleanEof`
  when the peer ended the stream cleanly, `Unspecified` otherwise.
- `Closed` — peer closed the connection / end-of-stream observed on the
  wire (bare transports also map a caller-initiated close to `Closed`).
- `TooLarge { len, max }` — message exceeds `max_payload`. Caller is
  responsible for chunking on their own framing semantics.
- `ExplicitClose` — a cancel handle fired. Every transport reports it the
  same way: a call parked in `send_bytes` / `recv_bytes` when `cancel()`
  runs returns `ExplicitClose`, and so does every call after it — SRT,
  TCP/TLS, RTP, UDP, RIST and both managed wrappers, as well as
  `tst_srt::SrtListener::accept_one_cancellable`. `SrtTransport`'s cancel
  closes the socket under the parked call and libsrt reports a connection
  error, but the transport reads its own cancel latch and reports the
  cancel; a cancel never surfaces as `Broken`. `ManagedRecvTransport` also
  reports `ExplicitClose` after its own `close()`. Both managed wrappers
  treat an `ExplicitClose` from the transport they wrap (it was cancelled
  through its own handle, not the wrapper's) as terminal: they report it
  and do not reconnect.

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
}
```

`SrtTransport::new` wraps an already-connected `SrtSocket`. Configure the
socket (passphrase, latency, stream id, etc.) before wrapping —
`SrtTransport` doesn't expose libsrt knobs of its own. `max_payload` is
read from the socket's negotiated `SRTO_PAYLOADSIZE`
(`SrtSocket::payload_limit`) — 1316 for an unconfigured socket, or e.g. 1456
when `payloadsize=` / `SrtSocketConfig::payload_size` was set. `DEFAULT_PAYLOAD`
is the 1316-byte fallback constant.

Error mapping from `SendError` → `TransportError`:

| `SendError` | `TransportError` |
| --- | --- |
| `TimedOut` / `QueueFull` | `Backpressure(...)` |
| `PayloadTooLarge { actual, .. }` | `TooLarge { len: actual, max }` |
| `ConnectionBroken` / `System(_)` | `Broken(...)` (drops socket) |
| `Other { kind: SrtErrno::Async, .. }` | `Backpressure(message)` |
| `Other { .. }` | `Broken(message)` (drops socket) |

When a transport is marked broken, the wrapped `SrtSocket` is dropped
internally — subsequent `send_bytes` calls return `Closed` until a
new `SrtTransport` is built.

### Default sender SrtSocketConfig overrides (tst-c connect path)

A caller-mode open from an `srt://` URL — `tst_srt::SrtUrl::connect`, and
therefore every caller-mode `tst_*_open` in the C ABI — merges these
overrides into the URL's `SrtSocketConfig`
(`SrtSocketConfig::merge_sender_defaults`, only for fields the URL left
unset):

| Field | Default | libsrt default | Why |
| --- | --- | --- | --- |
| `connect_timeout` | 15 s | 3 s | Radio links: LOS-over-terrain, antenna repointing, radio warm-up |
| `linger` | 5 s | off (0 s; drains in background) | Bounds an intentional synchronous drain on close — libsrt's own default returns immediately without waiting for the backlog to flush |
| `role` | `Role::Sender` | `Role::Receiver` (default) | Sets `SRTO_SENDER=1` for HSv4-peer compatibility |

Pure-Rust users who build a `SrtTransport` via `SrtSocketBuilder` directly
do **not** get these defaults — set them explicitly via the builder, or
start from `SrtSocketConfig::sender_defaults()`.

## `ManagedTransport<T>` — reconnect + gap buffer

```rust,ignore
impl<T: Transport + 'static> ManagedTransport<T> {
    pub fn new<F>(inner: T, factory: F, policy: ReconnectPolicy)
        -> Result<Self, ReconnectPolicyError>
    where F: Fn() -> Result<T, TransportError> + Send + Sync + 'static;
}
```

`ManagedTransport<T>` itself implements `Transport`, so any sender
shell composes with it transparently — the shell sees a `Transport`,
plain or wrapped.

The factory closure is what the manager calls to rebuild the inner
transport. Most callers wire it as a closure running
`SrtSocketBuilder::new()...connect(addr).map(SrtTransport::new)`. The
closure must be `Fn + Send + Sync + 'static` so the manager can hold
it in an `Arc<dyn Fn …>`. Every call that fails is logged at `warn`
(`reconnect attempt failed`, or `background reconnect attempt failed`
from a `Background` worker; target `tst_pipeline::reconnect`; with
`attempt`, `max_attempts`, `errno_code` and the error's text as
`error`), and the give-up warning carries the last error as
`last_error`, so keep secrets out of the error the closure returns: no
passphrase, no URL with credentials (`tst_core::url::redact_url` strips
them from a URL).

Behaviour on `send_bytes`: drain any queued gap-buffer bytes first,
then try the new bytes. `Backpressure` and `TooLarge` propagate to
the caller without triggering reconnect. `Broken` or `Closed` queues
the new bytes into the gap buffer (subject to `OverflowPolicy`) and
calls the factory with the configured backoff until a fresh transport
materialises or `max_attempts` is exhausted. On reconnect the gap
buffer drains before the call returns. If the call returns an error
instead, its bytes are taken back out of the gap buffer first: a send
that failed is the caller's to retry, and nothing is left queued to be
delivered behind its back. Either way the gap buffer is empty again
when a `Blocking` call returns: it only ever holds the one message
whose send found the link broken, so in this mode
`gap_buffer_capacity` and `overflow_policy` change nothing. At capacity
0 nothing is staged: the call waits for the reconnect and then sends
its message on the new connection, under either policy.

`ReconnectPolicy.mode` picks where that reconnect loop runs. The
default, `ReconnectMode::Blocking`, runs it on the calling thread — a
single `send_bytes` call may block for the full reconnect window.
Opt in to `ReconnectMode::Background` and a per-outage worker thread
owns the factory/backoff/drain loop instead: while it's active (or the
gap buffer is non-empty) `send_bytes` enqueues under `overflow_policy`
without waiting on backoff or the factory call — `Ok(())` means
*accepted*, not *delivered*. It does not wait on the worker's in-flight
inner send either (that call is unbounded against a peer that has
stopped reading), only on the gap buffer's own short critical sections.
`stats_handle().stats()`, and a **send** through a sender shell
(`MuxSender` / `Sender` / `RawSender`) wrapped around it, have the same
guarantee. The liveness/socket-stats queries those shells forward do
not: `socket_stats()` always asks the inner transport and so waits for
an in-flight inner send to return, while `is_alive()` answers from the
reconnect flag whenever a worker is active and consults the inner only
otherwise. On `MuxSender` that spills over onto sends: it guards its
muxer and transport with a single mutex, so a send on one thread queues
behind a `socket_stats()` call another thread already has in flight.
Use the stats handle for outage monitoring.
When no worker is active and the gap buffer is empty, a send goes directly
to the transport and can still block on network I/O.
This is still a synchronous API, not an async
runtime; see the
[cookbook recipe](/docs/cookbook/operations/managed-transport-reconnect.md#background-mode-reconnect-without-waiting-on-the-producer)
for when to reach for it. True async/reactor exposure remains on the
deferred-features list; see
[project/deferred-features.md](/docs/project/deferred-features.md).

## `ReconnectPolicy`

```rust,ignore
#[non_exhaustive]
pub struct ReconnectPolicy {
    pub max_attempts: Option<u32>,
    pub backoff: BackoffStrategy,
    pub gap_buffer_capacity: usize,
    pub overflow_policy: OverflowPolicy,
    pub mode: ReconnectMode,
}
```

The struct is `#[non_exhaustive]`: start from `ReconnectPolicy::default()` and
override fields with the `with_max_attempts` / `with_backoff` /
`with_gap_buffer_capacity` / `with_overflow_policy` / `with_mode` setters.

Defaults (`ReconnectPolicy::default()`):

- `max_attempts: Some(10)` — give up after ten reconnect attempts and
  surface `Broken("reconnect gave up after 10 attempts")` to the
  caller. Set to `None` to retry forever.
- `backoff: BackoffStrategy::Exponential { base: 100ms, max: 10s }` —
  see [`BackoffStrategy`](#backoffstrategy) for the variants.
- `gap_buffer_capacity: 256` — messages. Accumulates an outage's
  messages only under `ReconnectMode::Background`.
- `overflow_policy: OverflowPolicy::DropOldest` — see
  [`OverflowPolicy`](#overflowpolicy). Likewise a
  `Background`-mode knob.
- `mode: ReconnectMode::Blocking` — reconnect on the caller's thread.
  Set to `ReconnectMode::Background` to run reconnect on a per-outage
  worker thread instead; see the cookbook recipe linked above.

`ReconnectPolicy::validate` refuses zero backoff with unlimited attempts
(`max_attempts: None` and a zero `Constant`, or an `Exponential` with a zero base or a zero cap), which
would retry with no pacing. `ManagedTransport::new`,
`ManagedRecvTransport::new` and every managed open return the error as
`ReconnectPolicyError::UnpacedUnlimitedRetry`, or the config-invalid kind in
the bindings, before touching a socket.

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
- `Reject` — refuse the new message: the full gap buffer surfaces to the
  caller as `TransportError::Backpressure` ("gap buffer full"), and the
  caller decides what to do.

Trade-off: `DropOldest` keeps the receiver caught up to "now" once
reconnect lands, at the cost of losing the oldest part of the gap — the
right call for live video. `Reject` keeps what is already queued and
refuses new messages with `Backpressure` once the disconnect window
exceeds the buffer; reach for it when correctness matters more than
freshness (telemetry, control-plane messages, audit-logged events).

The policy only has something to decide under
`ReconnectMode::Background`, where sends keep arriving while the link
is down. Under `ReconnectMode::Blocking` the buffer holds one message
at a time and never fills, and with `gap_buffer_capacity: 0` the call
that meets the outage waits for the reconnect and sends its message
after it, under either policy (a give-up returns `Broken`, with the
message still the caller's). Under `Background` a capacity of 0 holds
nothing: every message sent during the outage is dropped and counted
(`DropOldest`) or refused with `Backpressure` (`Reject`) while the
worker reconnects, and sends after it has reconnected go to the new
connection. The one exception holds at every capacity in `Background`
mode: with a finite `max_attempts`, the first send after the worker has
given up returns `Broken` ("reconnect gave up after N attempts"), its
message neither queued nor counted as dropped, and the send after it
starts a new reconnect cycle.

## Choosing a backoff and gap-buffer size

Reasonable starting point: `BackoffStrategy::Exponential { base: 100ms,
max: 10s }` plus, **for `ReconnectMode::Background`**,

```text
gap_buffer_capacity = ceil(max_disconnect_window × send_rate)
```

Under the default `ReconnectMode::Blocking` there is nothing to size:
the caller is parked inside `send_bytes` for the whole outage, so no
backlog builds up in the wrapper, and a longer outage costs a longer
block rather than more buffer. Leave the capacity at its default.

Worked example: a five-second worst-case disconnect on a 30 fps stream
with one KLV record per frame produces 150 video messages plus 150 KLV
messages = 300 entries. Round up to 512. The 256-message default fits
roughly 4 seconds of that load.

If your worst-case window is longer than the buffer can hold (still
`Background` mode), either
raise the capacity (cheap — each entry is a `Vec<u8>` of at most
`max_payload` bytes) or accept `DropOldest` as a deliberate
freshness-over-completeness choice.

## Failure semantics

Where errors come from:

- `MuxSenderErrorSource::Mux` — the encoder produced something the muxer
  rejects (`BufferFull`, `KlvTooLarge`, `InvalidNal`). Rare in
  practice with reasonable buffer sizing.
- `MuxSenderErrorSource::Transport(TransportError)` — the transport layer
  reported an error (`MuxSender`).
- `SenderErrorSource::Framing(TsFramingError)` — STRICT mode rejected
  unaligned input (`SyncLost`). RECOVER mode returns
  `NoSyncAfterLimit` once scanning for sync consumes more than
  `max_unsynced_bytes` without acquiring it — see `max_unsynced_bytes`
  under the `SenderConfig` knobs above.
- `SenderErrorSource::Transport(TransportError)` — transport error (`Sender`).
- `RawSender::send` returns `RawSenderError`, which wraps the underlying
  `TransportError`.

With `ManagedTransport` wrapping the inner transport, transient
`Broken` errors are absorbed and the caller's `send_*` call appears to
succeed once reconnect lands. Once `max_attempts` is exhausted the call
returns `Broken` ("reconnect gave up after N attempts"); `Backpressure`,
`TooLarge` and a cancel (`ExplicitClose`) propagate without a reconnect. With a bare transport, every
`Broken` propagates — you reconnect by rebuilding the transport and
re-creating the sender.

## Threading

- `MuxSender` is internally synchronized. `send_video` and `send_klv` are
  safe to call concurrently; calls serialize on the internal mutex.
- `Sender` and `RawSender` are not internally synchronized. Wrap in
  an external `Mutex` if shared across threads, or — simpler — use
  thread-per-connection.
- `ManagedTransport` takes `&mut self` for `send_bytes` and `close`, like
  any `Transport`: one thread drives it. Its internal locks only
  coordinate that thread with its own reconnect worker. Other threads get
  its cancel handle and its stats handles.

Don't over-engineer this. The typical deployment has one sender per
process, or a small fixed number of senders each on its own thread.

## Receive side

The receive shells mirror the send shells. They differ by what they
emit; all three are generic over a `RecvTransport` (the receive
counterpart to `Transport`).

### Picking a receiver

- **Typed events out → `DemuxReceiver`.** Composes `Receiver → Demuxer`;
  emits `DemuxEvent` per call. Auto-flushes the demuxer's reassembly
  state when the stream ends, the transport breaks, or the receiver is
  closed. The default for "I want a stream
  of NALs and KLV records out of an SRT socket."
- **TS-aligned packets out → `Receiver`.** One 188-byte aligned TS
  packet per `next_packet` call. Internal sync recovery via the
  HUNT/VERIFY/LOCKED state machine: VERIFY locks after four aligned
  packets (four `0x47` bytes at 188-byte strides, 752 bytes) and emits
  nothing before that while the stream goes on. When the transport ends
  (end of stream, a break, a close or a cancel; not `Backpressure`, not
  a managed reconnect) the one to three whole packets still waiting are
  delivered before the end is reported, if every byte one packet apart
  that is present reads `0x47`, the first byte of a trailing partial
  packet included: a session shorter than four packets arrives, and so
  does the tail after a mid-stream resync. One failing boundary (one
  stray trailing byte that is not `0x47`) refuses the whole tail, and a
  single good packet behind damage that holds a `0x47` within the 187
  bytes before it is still lost. The release counts once in
  `resync_events`. This end rule is weaker evidence than the lock rule:
  before the first lock or after a resync, bytes that are not TS reach
  the demuxer as up to three packets at an end when a `0x47` sits
  exactly one, two or three packets before the last byte. `ReceiverStats` (`resync_events`, `bytes_skipped_for_sync`)
  shows when a resync happened. A caller that drives a `Syncer` itself
  gets none of the end handling.
  Use when you want to feed bytes into your own demuxer (FFmpeg,
  JavaCV, Bento4). `DemuxReceiver` inherits the same lock rule.
- **One byte vec per recv → `RawReceiver`.** No TS framing, no demux —
  one transport message per `recv_one` call, returned as `Vec<u8>`.
  Use as the receive counterpart to `RawSender`, or as a building
  block for tests.

### `DemuxReceiver` walkthrough

```rust,ignore
impl<R: RecvTransport> DemuxReceiver<R> {
    pub fn new(transport: R) -> Self;
    pub fn with_demux_options(transport: R, options: DemuxerConfig) -> Self;
    pub fn ts_bytes_demuxed(&self) -> u64;
    pub fn ts_bytes_demuxed_handle(&self) -> TsBytesDemuxedHandle;
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
use tst_srt::{SrtListenerBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = SrtListenerBuilder::new()
        .latency(Duration::from_millis(120))
        .bind("0.0.0.0:9000")?;
    let (socket, _peer) = listener.accept()?;
    let mut rx = DemuxReceiver::new(SrtTransport::new(socket));
    for item in &mut rx {
        match item? {
            DemuxEvent::ProgramMap(m) => println!("PMT streams={}", m.streams.len()),
            DemuxEvent::Sample { stream, pts, .. } => {
                println!("Sample PID=0x{:04X} pts={}", stream.pid, pts.as_ticks());
            }
            _ => {}
        }
    }
    Ok(())
}
```

Mirroring [../examples/receiving/srt_recv_typed.rs](/examples/receiving/srt_recv_typed.rs).

### Raw TS access: count or compose

`DemuxReceiver` keeps no hook that hands raw bytes to your code. Two tools
cover the need, and neither runs your code on a library thread.

**Count.** `ts_bytes_demuxed()` returns the aligned TS bytes the shell fed
its demuxer. `ts_bytes_demuxed_handle()` returns a cloneable
`TsBytesDemuxedHandle` that reads the same counter and outlives the shell.
The count is a lock-free atomic read. It never waits on a parked receive,
and it keeps its last value after close. The C getters are
`tst_<family>_get_ts_bytes_demuxed`, and the Python and JVM methods are
`ts_bytes_demuxed()` and `tsBytesDemuxed()`.

| Situation | `ts_bytes_demuxed` | `stats().bytes_received` |
|---|---|---|
| Syncer not yet locked (garbage before 4 aligned packets) | not counted | same |
| One to three packets released when the transport ends (see "Picking a receiver") | +188 each, before parsing | +188 each |
| Each aligned packet fed to the demuxer, a strict-mode rejection included | +188, before parsing | +188 |
| The packet read across a reconnect (managed), and bytes the sync reset drops | not counted | the packet counted, the dropped bytes not |
| After a reconnect | continues from the old value | continues |
| `reset_stats()` | unchanged | back to 0 |
| Read while another thread is parked in `recv_event` | returns at once (Rust: through a `ts_bytes_demuxed_handle()` taken before the receive) | `stats()` waits for the slot (bindings) |
| After cancel or close | last value (C: the pointer is freed at close) | as before |
| `--no-default-features` | absent | available |

**Compose.** To record, relay or inspect the packets, pull them from a
`Receiver` and feed the same packet to a `Demuxer`:

```rust,ignore
let mut rx = Receiver::new(transport, ReceiverConfig::default());
let mut demux = Demuxer::with_config(DemuxerConfig::default());
loop {
    let pkt = match rx.next_packet() {
        Ok(pkt) => pkt,
        Err(e) if e.kind == ShellErrorKind::EndOfStream => break,
        Err(_) => break, // still falls through to the flush below
    };
    if let Err(e) = recording.send_bytes(&pkt) { // your consumer
        eprintln!("recording error: {e}");
        break; // falls through to the flush below
    }
    if let Err(e) = demux.feed_aligned(&pkt) {
        eprintln!("demux error: {e}");
        break; // falls through to the flush below
    }
    while let Some(ev) = demux.next_event() { /* handle ev */ }
}
demux.flush();
while let Some(ev) = demux.next_event() { /* handle ev */ }
// Then return the error that ended the loop, if any (see the example).
```

The loop is yours, so two duties are too:

- **End of input flushes the demuxer.** A clean end and a terminal error
  both end in `flush`, then a last drain. Without it the final length-0
  (unbounded) PES, usually the last video access unit, is lost.
- **A reconnect resets both halves.** Over a `ManagedRecvTransport`, watch
  `reconnects_handle()`. When it rises, drop the packet read across the
  rise and call `reset_sync()` on the `Receiver` and on the `Demuxer`, so
  neither stitches a packet of the dead link onto the new one.
  `ManagedDemuxReceiver` does this for you; the composition does not.
- **A reconnect does not reach a downstream `Pairer`.** `reset_sync()`
  resets only the `Receiver` and the `Demuxer`, and the composition emits no
  `ReconnectDiscontinuity`, so a `Pairer` fed from it keeps KLV from the dead
  link. Call the `Pairer`'s `flush()` at the same reconnect and handle the
  outputs it returns.

The bindings compose the same way: loop `recv_bytes()` (`recvBytes()` on the
JVM) on a `Receiver`, write each chunk, call `Demuxer.feed(chunk)`, then
iterate `next_event()` (`nextEvent()`), and `flush()` at the end. Every chunk
is a whole number of 188-byte packets.

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
    // Provided (default `None`), as on `Transport`.
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> { None }
    fn socket_stats(&self) -> Option<SocketStats> { None }
}
```

Receive-side counterpart to `Transport`. `SrtTransport` implements
both — the same wrapper handles both directions on a connected socket.
`close` is defaulted as a no-op so test mocks and channel-backed
implementors can opt in only when they own a tear-down resource.

`recv_bytes` returns the number of bytes written. Returns
`TransportError::Closed` once the transport has been closed by its own
`close()` or the peer ended the stream where the protocol can say so;
`TransportError::Broken` on a wire failure; `TransportError::ExplicitClose`
once the cancel handle has fired; `TransportError::Backpressure` on a recv
timeout (transport still alive, caller may retry). It never returns
`Ok(0)` for a non-empty buffer — the receive shells read `Ok(0)` as
closed.

Whether the timeout arm can fire is per-transport configuration: SRT
via `SrtSocketBuilder::recv_timeout` / `SrtSocket::set_recv_timeout`
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
    ) -> Result<Self, ReconnectPolicyError>;
}
```

Sibling to `ManagedTransport<T>` for the receive direction — same
factory-closure + `ReconnectPolicy` cadence pattern.

Construction refuses zero backoff with unlimited attempts as
`ReconnectPolicyError::UnpacedUnlimitedRetry`; propagate the result with `?`.

**No gap buffer.** Receive-side bytes that never arrived can't be
replayed, so the decorator simply restarts the recv loop on a fresh
transport when the inner returns `Closed` or `Broken`. The demuxer-side
state (sync alignment, PES reassembly) carries over across reconnect,
which costs at most one re-VERIFY pass on the syncer.

The factory is `Box<dyn FnMut + Send>` rather than the send-side
`Arc<dyn Fn + Send + Sync>` because `recv_bytes(&mut self, …)` is
exclusive-mutable — there's no concurrent close-from-any-thread
requirement to design around. A failed factory call is logged as on
the send side (`reconnect attempt failed`, target
`tst_pipeline::managed_receive`, the error's text as `error`), so the
same rule holds: no secret in the error it returns.

One retry budget (`max_attempts`) covers every rebuild attempt of one
`recv_bytes` call, the ones that connected included: a connection that
breaks before it delivers anything uses one attempt, and the count
starts again with the next call.

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
[`Demuxer::reset_sync`](/docs/guides/mpegts-demux.md#demuxer-methods) and emits a
`DemuxEvent::ReconnectDiscontinuity` event so consumers can mark a hard
discontinuity in their downstream state (PTS reset, PSI re-fetch, etc.)
rather than guessing from the byte stream. The 188-byte sync rail is
re-VERIFIED on the first packet from the new transport — the demuxer
does *not* assume the new connection picks up where the old one left
off.

**Data-loss budget on reconnect.** The first aligned packet after a
reconnect is always dropped, and so are any bytes the syncer had buffered
but not yet emitted — bounded by the transport's `max_payload` (≈ one SRT
payload, 7 TS packets; never an entire flow). See the rustdoc on
`ManagedDemuxReceiver` for the exact contract.

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
  `cause: BrokenCause::CleanEof`; the shell classifies that one as
  `kind = ShellErrorKind::EndOfStream` (the source still reads
  `Broken { cause: CleanEof, .. }`), so the host bindings report
  `END_OF_STREAM` for it. `SrtTransport` collapses
  these into one `Broken` surface by design — it lets a managed-receive
  decorator distinguish a self-initiated close (`Closed`) from a peer-
  initiated break (`Broken`). On `Broken` the demuxer is still flushed
  first: any events the flush recovers are returned before the error,
  after the events of the packets the end releases.
- **`Err` whose `source` is `DemuxReceiverErrorSource::Demux(_)`.** Strict mode rejected the
  stream: `StrictRejection` for a non-conformant issue, `MalformedPes` for a
  malformed PES header. In the default lenient mode a malformed PES
  surfaces as a `NonConformant` event and the stream continues.

After the call that reports the end, the receiver reads its transport
no more and flushes its demuxer no more. Later calls return `Ok(None)`
after a clean end, the `Closed` error after a cancel (for ever), and
`Ok(None)` after a `TransportBroken` error, which is reported once, so a
`for` loop over a failed receiver ends. A cancel fired after the end is
reported once the events already produced are handed out, on every
transport. After the receiver's own `close()`, whenever it was called, a
cancel fired later, or one no call had reported before the close, does
not change the answer. In the bindings a cancel or a close refuses every
call after the one it interrupted, so of the packets an end releases a
binding caller gets at most the first after a cancel, and none in a call
made after a close; the peer's end, a broken transport and a managed
give-up deliver the whole tail there too.

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

Stay with the inline `DemuxEvent` match (the first three pairing recipes
in the cookbook) when:

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

Both strategies clear their history at a reconnect (`ReconnectDiscontinuity`) and at a signalled discontinuity on the video, KLV or program-clock PID (the program-clock PID counts only when it also carries an elementary stream): the KLV that never paired leaves as `UnpairedKlv` ahead of the passed-through event, and nothing pairs across it. In `PairerMode::Buffered` the held video is first classified against the old history. The first KLV and the first video completing after a flag within two seconds of the pre-flag clock leave unpaired, and a new clock landing that close loses its first KLV sample and its first video frame the same way (with `last_before_pts`, every frame until the next KLV sample is then unpaired). A flag on any other PID and a silent encoder restart are invisible, so flush the pairer yourself there. A PID that carries only PCR produces no demuxer event, so flush at a restart on it too. A `PairingDemuxer` (the pairer of the Python and JVM bindings) owns a plain demuxer and never sees a `ReconnectDiscontinuity`; only the signalled discontinuity reaches it.

See the `Pairer` recipes in the [cookbook's pairing section](../cookbook/index.md#-pairing--align-klv-with-video-frames) for runnable patterns.

### What you give up

The pairer is video-driven and consumes events on the configured
`video_pid` and `klv_pid`. Off-route events (other PIDs,
`ProgramMap`, `NonConformant`, `Discontinuity`, audio, subtitles) flow
through unchanged via `PairerOutput::PassThrough`, so topology
discovery and diagnostics are preserved. But a single `Pairer` is
single-pair: multi-video shapes (EO+IR) compose at the call site with
two instances, not via a single multi-pair builder.

The Python (`tstrans.pipeline`) and JVM (`org.tstrans.pipeline.Pairer`)
bindings expose the pairer; the C ABI does not (see
[deferred-features.md](/docs/project/deferred-features.md)).

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
parked inside libsrt to return promptly with
`TransportError::ExplicitClose` (shell kind `Closed`).

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
  thread; demonstrates the `Blocking` reconnect cycle end-to-end
  (its sibling `managed_reconnect_background.rs` is the one that fills
  and overflows the gap buffer).
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
  — nearest-PTS pairing of KLV records with video AUs.
- [../examples/operations/tee_disk_and_demux.rs](/examples/operations/tee_disk_and_demux.rs)
  — `Receiver` + `Demuxer` composition: write `.ts` to disk while
  consuming typed events, in a single pass.

## See also

- **Runnable example:** `cargo run -p tst-examples --example send_pipeline_to_socket` — [examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs)
- [guides/srt.md](/docs/guides/srt.md) — the `SrtSocket` and `SrtListener` that sit behind `SrtTransport`.
- [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — the muxer that `MuxSender` wraps.
- [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — the demuxer that `DemuxReceiver` wraps.
