# SrtCancelHandle — SRT cross-thread shutdown primitive

Every long-lived pipeline shell (`MuxSender`, `Sender`, `RawSender`,
`DemuxReceiver`, `Receiver`, `RawReceiver`) blocks the calling thread
inside `send_*` / `recv_*`. The cancel-handle pattern is how a
*different* thread (or a signal handler) wakes that blocked call so the
process can shut down promptly without time-sliced polling.

The handle is `Send + Sync`, one-shot, and idempotent — multiple
`cancel()` calls from any thread are safe and no-op after the first.

## Two layers, same primitive

There are two named types in play. The trait `TransportCancel` is
transport-agnostic (every `Transport` impl decides whether to surface a
cancel handle); the concrete struct `SrtCancelHandle` is SRT-shaped
(wraps an `SRTSOCKET` integer handle with `i64::MIN` as the cancelled
sentinel). Pick the one whose layer you're on:

| Layer | Type | Where it lives |
|-------|------|----------------|
| Pipeline (trait, dynamic dispatch) | [`TransportCancel`](/crates/tst-core/src/transport.rs) (trait) | `tst_pipeline::TransportCancel` (re-export of `tst_core::transport::TransportCancel`) |
| Concrete primitive (SRT-shaped) | [`SrtCancelHandle`](/crates/tst-core/src/cancel.rs) (struct) | `tst_pipeline::SrtCancelHandle` (re-export of `tst_core::SrtCancelHandle`); also re-exported as `tst_srt::SrtCancelHandle` |

Pipeline shells return the trait shape:

```rust,ignore
fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>>;
```

`Option` because a transport may not support cancellation (a pure
in-memory test mock returns `None`). All real transports — `SrtTransport`,
`SrtRecvTransport`, and `ManagedTransport` decorating either of them —
return `Some`.

`tst-srt`'s `Socket::cancel_handle()` and `Listener::cancel_handle()`
return the concrete `SrtCancelHandle` struct directly (no `Option`); these
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
use tst_pipeline::SrtCancelHandle;  // re-exported from tst_core::SrtCancelHandle

let socket: tst_srt::Socket = /* ... */;
let cancel: SrtCancelHandle = socket.cancel_handle();
// `SrtCancelHandle: Clone` — the inner state is Arc-shared, so all clones
// fire the closer once across the whole set.
```

## The pattern

The cancel-handle pattern unblocks a thread parked in `send_*` /
`recv_*` from another thread without time-sliced polling:

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

    // Cancel thread: in real code this is your signal handler / lifecycle
    // observer / parent-process watchdog.
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(30));
        cancel.cancel();  // wakes the parked send_ts on the main thread
    });

    // Main thread: the parked `send_ts` returns Err(Transport(Broken(_)))
    // once cancel() fires. Loop break is by error, not by polling.
    let pkt = vec![0u8; 188];
    loop {
        match sender.send_ts(&pkt) {
            Ok(()) => continue,
            Err(_e) => break,  // Cancellation surfaces as TransportError::Broken("cancelled")
        }
    }
    Ok(())
}
```

`cancel()` triggers the closer (libsrt's `srt_close` for `SrtTransport`)
exactly once. The parked syscall returns within one libsrt I/O cycle —
in practice 3–10 ms, bounded by the transport's `SRTO_RCVTIMEO` /
`SRTO_SNDTIMEO`. The error surfaces to the caller as
`TransportError::Broken(_)` (the message string is "cancelled" for
shells, or the libsrt error for direct `Socket::send` / `recv`).

## Per-language idiom

Bindings should expose `SrtCancelHandle` as a language-native shutdown
primitive. The shape maps cleanly:

| Language | Idiom |
|----------|-------|
| Rust | `Arc<dyn TransportCancel>` cloned to a worker thread; `cancel.cancel()` |
| Java / Kotlin | `AutoCloseable` cancel-token; `Job.cancel()` analog inside coroutine wrappers |
| Swift | `Task.cancel()` analog; the handle is held by a structured-concurrency parent |
| Python | `threading.Event`-shaped wrapper; `event.set()` triggers the underlying `cancel()` |
| C (`tst-c`) | `tst_cancel_handle_t *` opaque; `tst_cancel_handle_cancel(h)` (deferred to receiver-surface plan) |

The Rust API is the source of truth — every binding crate forwards
`cancel()` to the same idempotent atomic-swap inside `tst-core`.

## Threading guarantees

- `SrtCancelHandle: Send + Sync + Clone`. Stash it in any container, move
  it into any thread, clone it freely — every clone shares the same
  atomic state, so `cancel()` on any clone fires the closer at most
  once across the whole set.
- `Arc<dyn TransportCancel + Send + Sync>` (the shell-level shape) is
  similarly `Send + Sync`; clone the `Arc` to share across threads.
- `cancel()` is idempotent. Calling it twice — or concurrently from
  multiple threads — runs the closer at most once.
- The underlying close (`srt_close` for SRT transports) runs on the
  thread that wins the atomic swap. The closer's return code is
  currently swallowed — see the inline doc on `Socket::close` for
  context.
- `is_cancelled()` (on the concrete `SrtCancelHandle` struct) is advisory;
  the underlying close may not have completed yet on another thread.

## Why this and not `close()`?

`close()` on a shell takes `&mut self`, so it's not callable from
another thread while a `&mut`-borrowing `send_*` is in flight. The
cancel handle gives you a `Send + Sync + Clone` capability that
*doesn't* hold a mutable reference to the shell — the perfect shape
for "another thread / signal handler / FFI consumer needs to wake the
sender".

`MuxSender::close()` internally invokes the cancel handle first
("cancel-then-close" — see plan #18) so that a peer thread parked
inside `send_video` returns promptly, before `close()` proceeds to
flush and tear down. From the outside, `close()` is the right shape
for "stop the shell now" *when the thread doing the close also owns
the shell*; `cancel_handle().cancel()` is the right shape *when it
doesn't*. When the buffered tail matters more than promptness, use
`MuxSender::finish()` instead — it drains pending bytes to the live
transport (fallibly, and possibly blocking like `Drop`) before closing.

## Anti-priorities

`SrtCancelHandle` is the supported shape for sync-blocking shutdown. The
API is intentionally synchronous-blocking; when async lands later as a
separate crate (`tst-srt-async` or feature-gated), it ships
`Future::poll`-shaped cancellation alongside but doesn't deprecate
`SrtCancelHandle` — sync consumers stay on the trait-object pattern
above. See the **Sync vs. async** section in
[`architecture.md`](./architecture.md) for the long form.

## Cancel coverage by transport

| Transport | Cancel handle | Notes |
|-----------|--------------|-------|
| SRT (`tst-srt`) | `SrtCancelHandle` | From `Socket::cancel_handle()` or `pipeline_shell.cancel_handle()`. |
| RTP / RTSP (`tst-rtp`) | `RtpCancelHandle` | From `RtpTransport::cancel_handle()` or the pipeline shell. |
| TCP / TLS (`tst-tcp`) | `TcpCancelHandle` | From `TcpTransport::cancel_handle()` or the pipeline shell. |
| UDP (`tst-udp`) | None | Cooperative shutdown only: pass a finite `timeout_ms` / `recv_timeout` deadline and check a stop flag between calls. `close()` requires `&mut self` — not callable from another thread while a `recv` is in flight. See [/docs/project/deferred-features.md](/docs/project/deferred-features.md). |
| RIST (`tst-rist`) | None | Same as UDP — cooperative shutdown with `timeout_ms` and a stop flag. No race-free cross-thread interrupt of a live `recv`. See [/docs/project/deferred-features.md](/docs/project/deferred-features.md). |

## See also

- [`architecture.md`](/docs/reference/architecture.md) — Cross-thread shutdown section.
- [`binding-authors.md`](/docs/reference/binding-authors.md) — Cancel handles for binding authors.
- [Graceful shutdown from another thread via `SrtCancelHandle`](/docs/cookbook/operations/graceful-shutdown.md) — the cookbook recipe.
- [`guides/pipeline.md`](/docs/guides/pipeline.md) — Pipeline shell composition (where `cancel_handle()` lives).

## Managed wrappers: `CancelSlot`

`SrtCancelHandle` wraps *one* fixed socket. A managed wrapper has no such
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
disconnect, and that factory parks in `Listener::accept()` until the next
peer arrives — a wait nothing outside the factory can reach. The managed
transport's cancel handle reaches it through a `FactoryCancel` slot: the
factory installs the listener's own cancel handle into the slot around
its `accept()`, and the managed cancel fires whatever is installed (an
install that lands after the cancel fires immediately, so there is no
lost wake-up). The same cancel also interrupts the backoff wait between
attempts.

```rust,ignore
use std::sync::Arc;
use tst_pipeline::{FactoryCancel, ManagedRecvTransport, ReconnectPolicy, TransportError};
use tst_srt::{ListenerBuilder, SrtTransport};

let factory_cancel = Arc::new(FactoryCancel::new());
let fc = Arc::clone(&factory_cancel);
let factory = Box::new(move || -> Result<SrtTransport, TransportError> {
    if fc.is_cancelled() {
        return Err(TransportError::ExplicitClose);
    }
    let mut listener = ListenerBuilder::new().bind("0.0.0.0:9000")
        .map_err(|e| TransportError::Broken { msg: e.to_string(), errno_code: None })?;
    fc.install(Arc::new(listener.cancel_handle()));   // `SrtCancelHandle: TransportCancel`
    let accepted = listener.accept();
    fc.clear();
    match accepted {
        Ok((socket, _peer)) => Ok(SrtTransport::new(socket)),
        Err(_) if fc.is_cancelled() => Err(TransportError::ExplicitClose),
        Err(e) => Err(TransportError::Broken { msg: e.to_string(), errno_code: None }),
    }
});
let managed = ManagedRecvTransport::new_with_factory_cancel(initial, factory, ReconnectPolicy::default(), factory_cancel);
// `managed.cancel_handle()` now wakes the re-accept as well as a live recv.
```

The C ABI's `tst_managed_*_open_listener` family, both Python managed
receivers (`ManagedReceiver` and `ManagedDemuxReceiver`) and both JVM
ones are wired this way internally, so their `_cancel` / `cancel()` /
`cancelHandle().cancel()` cover the re-accept window. The one accept that
stays uncancellable is the very first one inside a listener open, before
any handle exists.

The bindings do not each hand-roll the sequence above:
`tst_srt::Listener::accept_one_cancellable(&cfg, addr, &slot)` is that
whole bind → install → accept → clear → classify body behind one call,
and every SRT listener factory in the C, Python and JVM bindings goes
through it. Reach for it rather than the long form whenever your factory
binds and accepts exactly once; write the long form only when you need
something in the middle of it (a different `ListenerConfig` per attempt,
say, or per-phase error mapping).
