# Cross-thread cancellation — `ResourceCancel` and `FlagCancel`

Every long-lived pipeline shell (`MuxSender`, `Sender`, `RawSender`,
`DemuxReceiver`, `Receiver`, `RawReceiver`) blocks the calling thread
inside `send_*` / `recv_*`. The cancel-handle pattern is how a
*different* thread (a signal-dispatch thread, a lifecycle observer, a
watchdog) ends that blocked call so the process can shut down, without
the caller polling a flag of its own.

SRT uses `ResourceCancel`: the cancel closes the socket. UDP, RIST and
both RTSP handles are a `FlagCancel` from `tst_core::cancel`, and TCP
(`TcpCancelHandle`) and RTP (`RtpCancelHandle`) wrap one: a shared flag
their blocking calls poll.

The handle is `Send + Sync`, one-shot, and idempotent — multiple
`cancel()` calls from any thread are safe and no-op after the first.

`Send + Sync` is not async-signal safety. Do not call `cancel()` from a
raw OS signal handler: the SRT closer takes a mutex and calls
`srt_close`, and a managed wrapper's `CancelSlot` takes a mutex too, so a
signal that lands while the interrupted thread holds one of them
deadlocks. Have the handler only wake an ordinary thread (a self-pipe,
`signalfd`, or the language runtime's own signal dispatch) and call
`cancel()` there.

## Two layers, same primitive

There are two named types in play. The trait `TransportCancel` is
transport-agnostic (every `Transport` impl decides whether to surface a
cancel handle); the concrete struct `ResourceCancel` is SRT-shaped
(wraps an `SRTSOCKET` integer handle with `i64::MIN` as the cancelled
sentinel). Pick the one whose layer you're on:

| Layer | Type | Where it lives |
|-------|------|----------------|
| Pipeline (trait, dynamic dispatch) | [`TransportCancel`](/crates/tst-core/src/transport.rs) (trait) | `tst_pipeline::TransportCancel` (re-export of `tst_core::transport::TransportCancel`) |
| Concrete primitive (SRT-shaped) | [`ResourceCancel`](/crates/tst-core/src/cancel.rs) (struct) | `tst_pipeline::ResourceCancel` (re-export of `tst_core::ResourceCancel`); also re-exported as `tst_srt::ResourceCancel` |

Pipeline shells return the trait shape:

```rust,ignore
fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>>;
```

`Option` because a transport may not support cancellation (a pure
in-memory test mock returns `None`). Every real transport (SRT, UDP, TCP,
RIST and RTP, send and receive) and the managed wrappers around them
return `Some`.

`tst-srt`'s `SrtSocket::cancel_handle()` and `SrtListener::cancel_handle()`
return the concrete `ResourceCancel` struct directly (no `Option`); these
are the paths to use when you're working below the pipeline shells.

## Obtaining a handle

From any pipeline shell:

```rust,ignore
use tst_pipeline::{MuxSender, TransportCancel};
use std::sync::Arc;

let mut sender: MuxSender<_> = /* ... */;
let cancel: Option<Arc<dyn TransportCancel + Send + Sync>> = sender.cancel_handle();
let cancel = cancel.expect("real transports always return Some");
```

From `tst-srt` directly (Socket / Listener):

```rust,ignore
use tst_pipeline::ResourceCancel;  // re-exported from tst_core::ResourceCancel

let socket: tst_srt::SrtSocket = /* ... */;
let cancel: ResourceCancel = socket.cancel_handle();
// `ResourceCancel: Clone` — the inner state is Arc-shared, so all clones
// fire the closer once across the whole set.
```

## The pattern

The cancel-handle pattern ends a call parked in `send_*` / `recv_*`
from another thread, without the caller polling:

```rust,no_run
use tst_pipeline::{Sender, SenderConfig, TransportCancel};
use tst_core::transport::{Transport, TransportError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

# struct Sink;
# impl Transport for Sink {
#     fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> { Ok(()) }
#     fn max_payload(&self) -> usize { 1316 }
#     fn close(&mut self) {}
#     fn is_alive(&self) -> bool { true }
# }
# fn build_real_transport() -> Sink { Sink }
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sender = Sender::new(build_real_transport(), SenderConfig::default());

    // Snapshot the cancel handle BEFORE we start the send loop. After this
    // point the handle is owned by the cancel thread; the sender keeps
    // running on the main thread until cancel() fires.
    let cancel: Arc<dyn TransportCancel + Send + Sync> = sender
        .cancel_handle()
        .expect("real transports return Some");

    // Cancel thread: in real code this is your signal-dispatch thread /
    // lifecycle observer / parent-process watchdog.
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(30));
        cancel.cancel();  // wakes the parked send_ts on the main thread
    });

    // Main thread: the parked `send_ts` returns
    // Err(Transport(ExplicitClose)) once cancel() fires. Loop break is by
    // error, not by polling.
    let pkt = vec![0u8; 188];
    loop {
        match sender.send_ts(&pkt) {
            Ok(()) => continue,
            Err(_e) => break,  // Cancellation surfaces as TransportError::ExplicitClose
        }
    }
    Ok(())
}
```

On SRT, `cancel()` runs the closer (libsrt's `srt_close`) exactly once,
on the thread that calls it. libsrt wakes the parked call for the close,
typically within a few milliseconds; the socket's send and receive
timeouts play no part in it. With linger on (`SrtSocketBuilder::linger`;
the sender preset sets 5 s) and unsent data queued on a connected socket,
`srt_close` first waits for that data to leave, for up to the linger
time, so the `cancel()` call itself can take that long. The other
transports set a flag and the parked call notices it at its next poll
tick; see [Cancel coverage by transport](#cancel-coverage-by-transport).
libsrt reports the closed socket as a connection error,
but `SrtTransport` reads its own cancel latch afterwards and surfaces
`TransportError::ExplicitClose` to the caller — the caller asked for the
close, so it is reported as the close, not as the wire error it provoked.
Every binding projects that to its `CLOSED` kind. Rust, Python, the JVM and
the C send calls use the detail `cancelled from another thread`, as does a C
receive refused at entry after the cancel; a C receive that was parked when
the cancel landed reports `receiver was cancelled or closed by caller`. (Through 0.6.x this was
`TransportError::Broken`.) A direct `SrtSocket::send` / `SrtSocket::recv`, which
is below the transport layer, still reports the libsrt error.

## Per-language idiom

Bindings expose the shell's cancel handle as a language-native shutdown
primitive. The shape maps cleanly:

| Language | Idiom |
|----------|-------|
| Rust | `Arc<dyn TransportCancel>` cloned to a worker thread; `cancel.cancel()` |
| Java / Kotlin | `AutoCloseable` cancel-token; `Job.cancel()` analog inside coroutine wrappers |
| Swift | `Task.cancel()` analog; the handle is held by a structured-concurrency parent |
| Python | `threading.Event`-shaped wrapper; `event.set()` triggers the underlying `cancel()` |
| C (`tst-c`) | no separate handle object: each shell has its own thread-safe `tst_<shell>_cancel(h)` entry point (`tst_mux_sender_cancel`, `tst_demux_receiver_cancel`, `tst_tcp_receiver_cancel`, …) |

The Rust API is the source of truth — a binding shell's cancel reaches
the shell's own Rust cancel handle through `tst_pipeline::binding::Owned`.

## Threading guarantees

**Data calls are exclusive.** A transport's `send_bytes`, `recv_bytes`
and `close()` take `&mut self`, and so do the send, receive and close
calls of `Sender`, `RawSender`, `Receiver`, `RawReceiver` and
`DemuxReceiver`: one caller drives the object at a time. `MuxSender`'s
calls take `&self`, and an internal mutex runs them one at a time, so a
second thread's `send_*` waits behind the first. The bindings serialise
every data call on a handle the same way, on the slot lock of
`tst_pipeline::binding::Owned`.

**Cancel is the one out-of-band call.** The cancel handle is the only way
another thread reaches a call in progress. It never takes the lock a
parked data call holds. How soon the parked call notices depends on the
transport (see [the table below](#cancel-coverage-by-transport)); the call
that observes the cancel reports `TransportError::ExplicitClose`, and so
does every later data call. The cases that may precede or override that
outcome are listed once, in
[binding-authors.md](/docs/reference/binding-authors.md#cancel-handles).

- `ResourceCancel: Send + Sync + Clone`. Stash it in any container, move
  it into any thread, clone it freely — every clone shares the same
  atomic state, so `cancel()` on any clone fires the closer at most
  once across the whole set.
- `Arc<dyn TransportCancel + Send + Sync>` (the shell-level shape) is
  similarly `Send + Sync`; clone the `Arc` to share across threads.
- `cancel()` is idempotent. Calling it twice — or concurrently from
  multiple threads — runs the closer at most once.
- The underlying close (`srt_close` for SRT transports) runs on the
  thread that wins the atomic swap. The closer's return code is
  consumed there, which is why `SrtSocket::close` and `SrtListener::close`
  are infallible.
- `is_cancelled()` (on the concrete `ResourceCancel` struct) is advisory;
  the underlying close may not have completed yet on another thread.
- `is_cancelled()` answers **"did a caller cancel?"**, never "is this
  socket finished?". A peer disconnect or a wire failure leaves it
  `false`, however dead the connection is — use `is_alive()` on the
  transport for that. The owner's internal teardown (`SrtSocket`/`SrtListener`
  `Drop`, and the transport error paths that retire a dead socket) goes
  through `close_without_cancel()`, which closes and wakes without
  latching; `SrtSocket::close()` and a cross-thread `cancel()` latch.

**Reads that never wait.** A getter that has to answer while a data call
is parked on another thread does not take the lock that call holds. It
reads either a value captured when the object was opened (a local or peer
address) or a `Sync` handle the object took when it was built, a view over
counters the call in progress updates: `RtpStatsHandle` from
`RtpRecvTransport::stats_handle()`, `PushStatsHandle` from
`RtspPush::stats_handle()` (its `close_outcome()` too: `None` while a
close is still running), `TsBytesDemuxedHandle` from
`DemuxReceiver::ts_bytes_demuxed_handle()`, `SourceHandle` from
`RecvTransport::source_handle()`, `MuxPublisherStatsHandle` from
`MuxPublisher::stats_handle()`. `MuxPublisher::stats()` reads the same
counters and does not wait either; `MuxPublisher::publisher_stats()`
does, because that record is the publisher's own and lives behind the
sender's lock, which a push holds. A binding object keeps its handle, so a
read through one can also answer after `close()`, with the last values, and,
on a push session, after a conversion into a shell, with the values of the
push the shell now owns. In the bindings these are the receiving shells'
`rtp_stats()` / `rtpStats()` (answers after `close()`), `peer_addr()` /
`peerAddr()`, `source_rejected()` / `sourceRejected()` and `local_addr()` /
`local_port()` (`localAddr()` / `localPort()`; each language guide says
which of these refuse a closed shell), a push session's `stats()`, `end_reason()` / `endReason()`, `end_detail()` /
`endDetail()`, `close_outcome()` / `closeOutcome()`, `is_session_alive()` /
`isSessionAlive()` and cancel handle, and the muxing shells'
`video_handle()` / `videoHandle()` and its four siblings (also while a
`reconfigure` holds the sender). `hls.MuxPublisher.stats()` never waits
for a push either, but it refuses a finished shell (Python
`HlsError(CLOSED)`, the JVM `IllegalStateException`); its
`publisher_stats()` / `publisherStats()` waits, as in Rust.
Every other getter waits for the parked call (a shell's `stats()` and
`socket_stats()`, `last_seen()` / `lastSeen()`); each language guide lists
both kinds ([Python](/docs/languages/python.md#what-answers-while-another-thread-is-parked),
[JVM](/docs/languages/jvm.md#what-answers-while-another-thread-is-parked)).

**What each close, stop and drop waits for.**

- `cancel()` on a flag transport is one atomic store (two on TCP: the
  cancel latch, then the liveness flag), never a wait. On SRT it runs
  `srt_close` on the calling thread, which with linger on can take up to
  the linger time (see [The pattern](#the-pattern)). On a managed wrapper
  it latches, wakes the backoff wait and fires the live inner transport's
  handle, so it costs what that inner's cancel costs.
- A transport's own `close()`: SRT runs `srt_close`, linger included
  (a managed SRT sender armed for silent-peer detection closes the
  connection it abandons with `Transport::abandon`, linger off, without
  waiting for the peer; its own `close()` keeps the linger);
  RIST runs `rist_destroy`, which joins librist's protocol thread. UDP
  only marks the transport closed: its socket stays open, and its port
  bound, until the transport is dropped. TCP marks it closed and shuts the
  stream down (a TLS stream sends its `close_notify` first); the
  descriptor is released when the transport drops. RTP drops its socket
  at `close()` (a receiver fed by an RTSP interleaved session drops its
  channel instead), so the port is free when `close()` returns.
- `MuxSender::close()` cancels first, then waits for a send parked on
  another thread to honour the cancel and release the sender's lock, then
  closes the transport. `MuxSender::finish()` cancels nothing: it waits
  behind a parked send, and each of its drain sends can take as long as
  the transport's send does.
- `Sender::finish()` cancels nothing either: it waits behind a parked
  send, sends any bundle the transport refused, the partial bundle and
  what its end-of-stream pass over the bytes still held verifies, closes
  the transport and returns the first send error, else
  `TsFramingError::UnsentTail` (`InputMalformed`) for bytes it could not
  send as whole aligned TS packets (a partial last packet among them);
  a later `close()` does nothing. It makes one attempt: what a refused
  send left is abandoned. `Sender::close()` sends and closes the same way
  and reports nothing. Python's and the JVM's `Sender.finish()` run
  `finish()`, with no cancel first, where their `close()` cancels first.
  A stream of one or two aligned TS packets is sent by every `finish()`
  and by Rust's `close()`.
- `Drop` of a `MuxSender` or a `Sender` sends what is still buffered (a
  `Sender`: what its end-of-stream pass can verify) and then closes the
  transport, with no cancel first, so it takes as long as
  that send and close take. The other shells' `Drop` adds nothing to the
  transport's own.
- A binding's `close()` (`Owned::close`) cancels, waits for the call
  parked on the handle to return, then closes and drops the shell.
- `ManagedTransport::close()` runs the same wake-up as its cancel (without
  latching the cancel, so later sends report `Closed`), then
  joins an active reconnect worker, which waits for a `factory()` call in
  flight; its `Drop` signals the same way and does not join.
- The servers and the RTSP client own threads or a runtime. The
  [Runtime ownership](/docs/reference/architecture.md#runtime-ownership)
  table lists what each one's `stop()`, `shutdown()`, `close()` and `Drop`
  wait for;
  each figure there is the wait of one phase, not a total.
- Every binding's explicit close of an RTSP client session is
  `RtspClient::close()`: a TEARDOWN under one 500 ms bound with the reply
  awaited, then up to 2 s for the client's thread. In C that is
  `tst_rtsp_session_teardown_and_free` and `tst_rtp_demux_receiver_close`
  on a receiver from `tst_rtsp_session_into_demux_receiver`; on the JVM,
  `RtspSession.close()`, the `close()` of an `H264Receiver` made by
  `intoH264Receiver()` and a failed `intoH264Receiver()`; in Python,
  `RtspSession.close()` and the `with` exit. A close that finds a control
  call in progress on another thread cancels it first, so the session
  ends as a cancel. `teardown()` in Python and the JVM stays the reporting
  call: it waits up to the request timeout, reports a refused TEARDOWN and
  keeps the client. A receiver taken from a Python session holds the
  client too: a session collected without `close()` drops its share, and
  the client is dropped, without a wait for its thread, once the session
  object and every receiver taken from it are gone.
- An `RtspPush` (the RTSP push client) waits for its queued media before
  its TEARDOWN, and that wait has no total bound: it ends early only on the
  cancel or the session's end. `close()` then sends TEARDOWN under one
  500 ms bound and waits up to 2 s for the client's thread;
  `Transport::close` does the same without the wait for the thread; `Drop`
  sets the cancel, discards the queue and writes TEARDOWN without waiting
  for its reply. Python's and the JVM's `close()` (and the `with` exit,
  try-with-resources) run `close()`, and fire the cancel first when a send,
  `pause()` or `resume()` is running on another thread (a call only waiting
  for the session is not cancelled: it runs nothing and ends `CLOSED`). See
  [RTSP push](#rtsp-push-what-a-cancel-and-each-close-do).
- An `RtspServer`'s `stop()`, and every binding call that runs it (the
  JVM's `close()`, Python's `with` exit, `tst_rtsp_server_stop`), is not
  shortened by a hard cancel fired while it runs: the cancel closes the
  publish side within 100 ms, but the Notice writes, the drain wait and
  the shutdown of each session's connection do not read it. For an end
  without the Notice, fire the cancel before the stop.

No call promises an absolute shutdown time: a close waits for the active
operation to honour the cancel.

## Why this and not `close()`?

A listener's `close(self)` consumes it, and some shell close methods need a
mutable borrow; neither shape gives another thread a way to wake a parked
operation. `MuxSender::close(&self)` is synchronized and cancels before it
takes its slot lock. The cancel handle is a separate `Send + Sync + Clone`
capability that wakes the I/O without borrowing or consuming the owner —
the shape for "another thread / signal-dispatch thread / FFI consumer needs
to wake the sender".

`MuxSender::close()` internally invokes the cancel handle first
("cancel-then-close") so that a peer thread parked
inside `send_video` returns promptly, before `close()` proceeds to
flush and tear down. From the outside, `close()` is the right shape
for "stop the shell now" *when the thread doing the close also owns
the shell*; `cancel_handle().cancel()` is the right shape *when it
doesn't*. When the buffered tail matters more than promptness, use
`MuxSender::finish()` instead — it drains pending bytes to the live
transport (fallibly, and possibly blocking like `Drop`) before closing.

## Anti-priorities

The cancel handle (`TransportCancel`) is the supported shape for
sync-blocking shutdown. The API is intentionally synchronous-blocking;
when async lands later as a separate crate (`tst-srt-async` or
feature-gated), it ships `Future::poll`-shaped cancellation alongside but
doesn't deprecate the cancel handle — sync consumers stay on the
trait-object pattern above. See the **Sync vs. async** section in
[`architecture.md`](./architecture.md) for the long form.

## Cancel coverage by transport

| Transport | Cancel handle | Notes |
|-----------|--------------|-------|
| SRT (`tst-srt`) | `ResourceCancel` | From `SrtSocket::cancel_handle()`, `SrtListener::cancel_handle()` or `pipeline_shell.cancel_handle()`. No poll: `cancel()` closes the socket on the calling thread, and libsrt wakes a parked send, receive or accept for the close, typically within a few milliseconds. With linger on and unsent data queued, that close (and so `cancel()`) can take up to the linger time. |
| RTP (`tst-rtp`) | `RtpCancelHandle` | From `RtpTransport::cancel_handle()`, `RtpRecvTransport::cancel_handle()`, `H264Receiver::cancel_handle()` or the pipeline shell. A parked send or socket receive notices it at its next 100 ms socket timeout, a receive fed by an RTSP interleaved session at its next 100 ms channel wait, and `H264Receiver::recv_au` at its next 100 ms read timeout; the H.264 receiver then hands out the complete access units it holds and the one it can flush, and returns `ExplicitClose` on every later call. |
| RTSP client and server (`tst-rtp`) | `RtspCancelHandle`, `RtspServerCancelHandle` | From `RtspClient::cancel_handle()` / `RtspServer::cancel_handle()`. On the client, a request waiting for its reply notices it within 100 ms and returns `RtspError::LocalCancel`. A request made after the cancel is still written first; if its write, its reply wait or its deadline then fails, it returns `LocalCancel` too. A request whose bytes the peer is not taking does not read the cancel until its write completes, its deadline passes or 5 s without progress end the control connection. The client's thread (`tst-rtsp-client`) reads the flag before each keepalive ping and starts no ping once it is set; it keeps reading the control connection and delivering interleaved media, since the receiver has its own `RtpCancelHandle`. On the server it is the hard cancel, on a started or a never-started server: registration and removal, rule edits, mount pushes and `start()` read it and are refused at once, a parked `next_publisher` notices it within 100 ms, and a task on the server's runtime notices it within 100 ms and ends the sessions, the listener, multicast senders and the publish side. Each publish mount's receiver then reads end of stream, as after `stop()`, in every binding; the receiver's own cancel, or its publish mount's, still reads as the cancel. A hard cancel fired while a `stop()` runs does not shorten that `stop()` (see [What each close, stop and drop waits for](#threading-guarantees)); see `RtspServerCancelHandle` for the detail. |
| RTSP push (`tst-rtp`) | `RtspCancelHandle` | From `RtspPush::cancel_handle()` (`cancel_handle()` in Python, `cancelHandle()` on the JVM) or a sender shell over the push. A send waiting for room in the push's queue, `pause()` and `close()` waiting for the queue notice it within about 100 ms; the send returns `ExplicitClose`, as does every later data call. The session stays open until the push is closed or dropped. See [RTSP push](#rtsp-push-what-a-cancel-and-each-close-do). |
| TCP / TLS (`tst-tcp`) | `TcpCancelHandle` | From the inherent `TcpTransport::cancel_handle()`, from `Transport::cancel_handle` / `RecvTransport::cancel_handle` on the same transport, or from the pipeline shell (which reads the trait method). All three hand back the same handle. A parked receive or send notices it at its next 100 ms socket timeout and returns `ExplicitClose`; a parked `TcpListener::accept_blocking` notices it at its next 5 ms poll and returns `TcpError::Closed`. |
| UDP (`tst-udp`) | `UdpCancelHandle` | From the inherent `UdpTransport::cancel_handle()` / `UdpRecvTransport::cancel_handle()`, the trait forms, or the pipeline shell. A parked `recv_bytes` observes it at its next ~100 ms poll tick and returns `ExplicitClose`; `send_bytes` never parks and checks it at entry. `close()` stays `Closed`. `recv_timeout` (per-call deadline) does not observe it — check `is_cancelled()` between slices. |
| RIST (`tst-rist`) | `RistCancelHandle` | From the inherent `RistTransport::cancel_handle()` / `RistRecvTransport::cancel_handle()`, the trait forms, or the pipeline shell. Observed when the current 100 ms `rist_receiver_data_read2` tick returns (`ExplicitClose` instead of that tick's `Backpressure`) and at `send_bytes` entry. `close()` (→ `rist_destroy`) stays `Closed`. |
| Managed wrappers (`tst-pipeline`) | `ManagedCancel`, `ManagedRecvCancel` | From `ManagedTransport` / `ManagedRecvTransport` `cancel_handle()` or the pipeline shell. Latches the wrapper, wakes a backoff wait at once and fires the live inner transport's handle, so a parked inner call notices it as that transport does. A `factory()` call in progress is not reached on the send side (`ManagedTransport::close()` waits for it); on the receive side only through a `FactoryCancel` slot (see [Managed receivers in listener mode](#managed-receivers-in-listener-mode)). |

The table covers the byte transports, the ones that implement `Transport`
/ `RecvTransport`, their managed wrappers and the RTSP control plane. HLS
and WebRTC are not byte transports and have no cancel handle; each has
its own shutdown call. An `HlsPublisher` ends with
`Publisher::finish()`, or, to keep serving the finished playlist, with
`finish_serving()` followed later by `HlsServerHandle::shutdown()`. A
`WhepServer` ends with `stop()`, which waits for its two threads; its
`Drop` cancels the same way and does not wait.

## RTSP push: what a cancel and each close do

An `RtspPush` hands each RTP packet to the client's I/O thread through a
bounded queue. A data call (`send_ts`, `send_video`, `send_klv`, a shell's
send over the push) returns once its packets are queued and waits only when
the queue is full. That wait, and the queue wait of `pause()`, `close()`
and `Transport::close`, ends when room frees (or the queue is written),
within about 100 ms of a cancel, when the session ends, or when the
connection has taken no byte for 5 s, which ends it. The 5 s count from
when the operating system stops taking bytes, not from when the server
stops reading; a server that keeps taking a few bytes within every 5 s
holds the wait for as long as it does so, and the cancel is the way out.
Keepalive pings go out one at a time: one still waiting for the connection
to take its bytes is not followed by another, which falls due one interval
later. However long such a server holds the connection, what waits on it
stays bounded: at most 64 KiB of media and one frame, one request and one
ping.

**The cancel.** It ends a waiting data call, `pause()` or `close()`, and
the push queues no more media: every data call from then on returns
`ExplicitClose` (`RtpError(CLOSED)` in Python, `RtpException(CLOSED)` on
the JVM). It does not end the session. Requests are still written: a
`resume()`, and the TEARDOWN of closing or dropping the push. A `close()`
on a cancelled push writes its TEARDOWN, drops what was queued and returns
`RtspError::LocalCancel`.

**The three ends.**

| Call | Waits for | Then | Reports |
|---|---|---|---|
| `RtspPush::close()` | the queue to be written (a last sender report per track queued first) | TEARDOWN, one 500 ms bound over writing it and its reply; up to 2 s for the client's thread | `Ok(())` when the queued media was written and the server accepted the TEARDOWN; otherwise the first of: `LocalCancel` once the cancel has fired; `Protocol { code: 454 }` when the server had forgotten the session and queued media was dropped, whatever the TEARDOWN is answered; the TEARDOWN's result |
| `Transport::close` | the same | TEARDOWN under the same bound; tells the thread to stop without waiting | nothing; the case of what `RtspPush::close()` would have returned is kept as `PushStatsHandle::close_outcome()` (beside `stats().frames_dropped_at_close` and `end_reason()`) |
| `Drop` | nothing | sets the cancel, discards the queue, writes TEARDOWN without waiting for its reply | nothing |

**The shells over a push.** `MuxSender::close()` cancels first, by this
page's contract, so the push's close finds its queue wait cancelled: media
queued a moment before can be dropped, and the TEARDOWN is written.
`MuxSender::finish()` cancels nothing: it sends what the shell holds and
closes the push, which, while the session is up, writes its queue before the
TEARDOWN. `Sender::finish()` (Python's and the JVM's `Sender.finish()`) does the same
for a plain `Sender`. In Rust, `Sender::close()` and the `Drop` of a
`MuxSender` or a `Sender` close the push without a cancel too. A binding's
`close()` of either shell cancels first: end a binding's shell over a push
with `finish()`. A shell's own result does not say whether the push's queue
was written: `Transport::close` returns nothing, so a session that ended
first drops the queue and the shell's close or `finish()` still succeeds.
In Rust the push keeps how its close ended: take `stats_handle()` before
the push goes under the shell and read `close_outcome()` once the shell
has finished (`Clean`, `MediaLost`, `TeardownRefused { code, reason }`,
`TeardownTimedOut`, `Failed`; `Cancelled` after `MuxSender::close()`,
which cancels first). In Python and on the JVM the push session keeps
the same case for its `close_outcome()` / `closeOutcome()` (a
`PushCloseOutcome` whose kind is `CLEAN`, `CANCELLED`, `MEDIA_LOST`,
`TEARDOWN_REFUSED`, `TEARDOWN_TIMED_OUT` or `FAILED`), read after the
shell's `finish()`; a binding shell's `close()` cancels first, so it
reads `CANCELLED`. The session's own `teardown()` still raises or throws
what its close reports.
The session object's `stats()` follows the push into the shell, so its
`frames_dropped_at_close` (`framesDroppedAtClose()`) shows afterwards what a
shell's end dropped (on the JVM until the session object is closed).

**Python and the JVM.** `RtspPushSession.close()` (the `with` exit,
try-with-resources) runs `RtspPush::close()` and reports nothing;
`teardown()` runs it and raises what it reports (`CLOSED` for the cancel).
Either one fires the cancel first only when a send, `pause()` or `resume()`
holds the session on another thread: that call ends with `CLOSED` and the
queue is dropped. A send, `pause()` or `resume()` still waiting for the
session is not cancelled and not waited for: it gets the session after the
close began, runs nothing and ends with `CLOSED`. A close that finds the
session held only by a read (an `is_session_alive()` looking at it) waits for
it and closes as an idle session does. `stats()`, `end_reason()` /
`endReason()` and `end_detail()` / `endDetail()` read the push's
`PushStatsHandle` and never wait (see
[Reads that never wait](#threading-guarantees)); `is_session_alive()`
(`isSessionAlive()`) and the cancel handle never wait either.
The conversions into a sender shell (`into_mux_sender` / `into_sender`,
`intoMuxSender` / `intoSender`) do not wait for a send, `pause()` or
`resume()`: they raise `RuntimeError` (throw `IllegalStateException`),
consume nothing and leave the session usable; a read holding the session
they wait for. These are the rules of
`tst_pipeline::binding::parking::ParkingSlot`, which both bindings' push
sessions hold the push in. After a panic inside a call the slot refuses
every call that takes the session (`RtspError(CLOSED)`,
`RtspException(CLOSED)`), and `close()` and `teardown()` still end the push.
A Python session collected without `close()` is the `Drop` row.

**One exception to "`Broken` means the end".** `TransportError::Broken`
reads "Transport is dead. Caller must rebuild it (or rely on a managed
wrapper to do so)". Every other `Broken` from a push follows the session's
end (the server closed the connection, the
connection failed or stalled for 5 s, a keepalive ping was answered `454`,
the server broke the framing). A payload `send_ts` refuses is reported
`Broken` while the push stays alive: an empty payload, one that is not
whole 188-byte packets starting `0x47`, and, in Rust, any payload on a push
that announced elementary streams (Python raises `ValueError` and the JVM
`IllegalArgumentException` for that one). Nothing is sent and the next
call goes through; `is_alive()` still reads true. More than 1 316 bytes is
`TooLarge`, which also leaves the push open.

## See also

- [`architecture.md`](/docs/reference/architecture.md) — Cross-thread shutdown section.
- [`binding-authors.md`](/docs/reference/binding-authors.md) — Cancel handles for binding authors.
- [Graceful shutdown from another thread via `ResourceCancel`](/docs/cookbook/operations/graceful-shutdown.md) — the cookbook recipe.
- [`guides/pipeline.md`](/docs/guides/pipeline.md) — Pipeline shell composition (where `cancel_handle()` lives).

## Managed wrappers: `CancelSlot`

`ResourceCancel` wraps *one* fixed socket. A managed wrapper has no such
fixed target: which handle can unblock its worker changes every time the
worker moves on — from a live socket to a reconnect factory to the next
socket. `tst_core::cancel::CancelSlot` is the primitive that bridges the
two. It is a latched, replaceable cancel target: a worker `install`s the
handle that can currently unblock it before it blocks and `clear`s it
afterwards, and `cancel()` fires whatever is installed at that moment.
Because it latches, an `install` that lands *after* a cancel fires
immediately instead of parking on a target nobody will reach again.

`ManagedTransport` and `ManagedRecvTransport` both publish their live
inner transport's cancel handle through such a slot, which is what makes
two guarantees hold: **cancel never waits on a blocked send or receive**
— the handle comes out of the slot, never out of the mutex the blocking
call holds — and **a cancel that lands during a reconnect is honored
before the fresh connection is read**, because the reconnect installs the
new inner into the already-latched slot. `tst_pipeline::FactoryCancel`
is the name factories use for the same type (it is a type alias of
`CancelSlot`), and it is what the listener-mode wiring below plugs in.

## Managed receivers in listener mode

A listener-mode `ManagedRecvTransport` re-runs its factory after a peer
disconnect, and that factory parks in `SrtListener::accept()` until the next
peer arrives — a wait nothing outside the factory can reach. The managed
transport's cancel handle reaches it through a `FactoryCancel` slot: the
factory installs the listener's own cancel handle into the slot around
its `accept()`, and the managed cancel fires whatever is installed (an
install that lands after the cancel fires immediately, so there is no
lost wake-up). The same cancel also interrupts the backoff wait between
attempts.

```rust,ignore
use std::sync::Arc;
use tst_pipeline::{BrokenCause, FactoryCancel, ManagedRecvTransport, ReconnectPolicy, TransportError};
use tst_srt::{SrtListenerBuilder, SrtTransport};

let factory_cancel = Arc::new(FactoryCancel::new());
let fc = Arc::clone(&factory_cancel);
let factory = Box::new(move || -> Result<SrtTransport, TransportError> {
    if fc.is_cancelled() {
        return Err(TransportError::ExplicitClose);
    }
    let mut listener = SrtListenerBuilder::new().bind("0.0.0.0:9000")
        .map_err(|e| TransportError::Broken { msg: e.to_string(), errno_code: None, cause: BrokenCause::Unspecified })?;
    fc.install(Arc::new(listener.cancel_handle()));   // `ResourceCancel: TransportCancel`
    let accepted = listener.accept();
    fc.clear();
    match accepted {
        Ok((socket, _peer)) => Ok(SrtTransport::new(socket)),
        Err(_) if fc.is_cancelled() => Err(TransportError::ExplicitClose),
        Err(e) => Err(TransportError::Broken { msg: e.to_string(), errno_code: None, cause: BrokenCause::Unspecified }),
    }
});
let managed = ManagedRecvTransport::new_with_factory_cancel(initial, factory, ReconnectPolicy::default(), factory_cancel)?;
// `managed.cancel_handle()` now wakes the re-accept as well as a live recv.
```

The C ABI's `tst_managed_*_open_listener` family, both Python managed
receivers (`ManagedReceiver` and `ManagedDemuxReceiver`) and both JVM
ones are wired this way internally, so their `_cancel` / `cancel()` /
`cancelHandle().cancel()` cover the re-accept window. The very first
accept inside a listener open runs before any shell, and so any cancel
handle, exists. Rust reaches it through a slot the caller makes and
passes to the open (`tst_srt::opener::open_recv`,
`tst_srt::shells::managed_recv_transport_from_url` and the two
`*_with_cancel` opens beside it), or takes from a
`tst_pipeline::binding::OpenCancel`
([Cancelling an open](#cancelling-an-open-opencancel)). Python reaches it
through a `tstrans.shells.OpenCancel` passed as `cancel=` to the open (the
interpreter-exit hook reaches it too, with or without one), and the JVM
through an `org.tstrans.shells.OpenCancel` passed as the last argument of
the open. In C it stays uncancellable: the listener opens take no open
cancel, and the rule is cancel, wait, free from the moment an open has
returned its handle.

The bindings do not each hand-roll the sequence above:
`tst_srt::SrtListener::accept_one_cancellable(&cfg, addr, &slot)` is that
whole bind → install → accept → clear → classify body behind one call,
and every SRT listener factory in the C, Python and JVM bindings goes
through it. Reach for it rather than the long form whenever your factory
binds and accepts exactly once; write the long form only when you need
something in the middle of it (a different `SrtListenerConfig` per attempt,
say, or per-phase error mapping).

## Cancelling an open: `OpenCancel`

`tst_pipeline::binding::OpenCancel` is the object behind the Python and
JVM `OpenCancel` classes: a cancel for opens that have not returned a
shell yet. One object may be given to any number of opens, one after
another or at the same time. `cancel()` is sticky: it reaches every open
in progress that was given the object, and every later open given it is
refused with CLOSED before it binds or dials. It ends a listener-mode
first accept at any time; a caller-mode dial is not interrupted.

One outcome rule covers every open given such an object: **the open
reports CLOSED whenever the object was cancelled by the time the open
finished, whatever the open itself did.** A shell that had just opened is
closed again and not handed out, and a failed open reports the cancel
instead of its own error. An open that finished before the cancel is
returned whole, and the object never touches it afterwards: from then on
the shell's own `cancel_handle()` is its cancel, and a managed receiver
opened with the object keeps reconnecting after a later `cancel()`. This
is the one place where a cancel beats a success, and nothing handed out
is lost by it: the open had handed nothing out yet. The C binding has no
such object.
