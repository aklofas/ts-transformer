# Survive a flaky transport with reconnect + gap buffer

> **When to use this:** An SRT connection can break during a network outage, NAT timeout, or listener restart, and your sender needs to reconnect.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `ManagedTransport`, `ReconnectPolicy`, and gap-buffer behavior
> - [Example: `managed_reconnect`](/examples/operations/managed_reconnect.rs)

`ManagedTransport<T>` wraps a transport and calls your factory to create a
new connection when the old one breaks. With SRT, packet retransmission is
already handled by the protocol; this wrapper handles rebuilding the
connection after it fails.

The factory closure rebuilds the inner transport on demand. `ReconnectPolicy` controls retries, backoff, and gap-buffer overflow behaviour.

Choose how the producer should behave during an outage:

| Mode | What a send call does | When to use it |
|---|---|---|
| `Blocking` (default) | Waits for reconnect and retries the interrupted send; returns an error if recovery fails. | The producer can pause while the connection recovers. |
| `Background` | Queues data while a worker sends and reconnects; the overflow policy decides what happens when the queue fills. | The producer must keep accepting new data during an outage. |

The example below configures **Blocking** mode. Its gap buffer holds the
interrupted message, so the buffer settings are left at their defaults.
In [Background mode](#background-mode-reconnect-without-waiting-on-the-producer), size the
buffer for the traffic you want to retain during an outage.

```rust,no_run
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, MuxSender, OverflowPolicy, ReconnectMode,
    ReconnectPolicy, TransportError,
};
use tst_srt::{SocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let factory = || -> Result<SrtTransport, TransportError> {
        // Every reconnect opens a new socket with the same configuration.
        let mut sb = SocketBuilder::new();
        sb.latency(Duration::from_millis(120));
        let socket = sb
            .connect("127.0.0.1:9000")
            .map_err(|e| TransportError::Broken {
                msg: format!("connect failed: {e}"),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })?;
        Ok(SrtTransport::new(socket))
    };
    let initial = factory()?;
    let policy = ReconnectPolicy {
        max_attempts: Some(20),
        backoff: BackoffStrategy::Exponential {
            base: Duration::from_millis(100),
            max: Duration::from_secs(10),
        },
        gap_buffer_capacity: 256,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Blocking,
    };
    let managed = ManagedTransport::new(initial, factory, policy);
    let _sender = MuxSender::new(managed, MuxerConfig::default())?;
    Ok(())
}
```

Runnable: [examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs).

<a id="background-mode-never-stall-the-producer"></a>

## Background mode: reconnect without waiting on the producer

Use `ReconnectMode::Background` when one thread both receives upstream
frames and forwards them. Waiting for reconnect on that thread would also
stop upstream reads. Once recovery starts, a worker handles reconnect
attempts, backoff, and draining queued data. While that worker is active
or the queue is nonempty, the producer queues messages without waiting
for those operations.

When no worker is active and the queue is empty, sends go directly to the
transport and can block on network I/O. Background mode moves reconnect
work off the producer thread; it does not make every send nonblocking.

For monitoring, use `stats_handle().stats()`. It reads queue and reconnect
counters without waiting for the worker's network I/O. Both enqueueing
and reading these counters still briefly lock the gap buffer.

The same guarantee does **not** apply to every query on a sender shell:

- `socket_stats()` queries the inner transport and can wait behind a
  blocked network send.
- `is_alive()` returns `true` while a reconnect worker is active; otherwise
  it queries the inner transport.
- `MuxSender` holds one mutex around its muxer and transport. A
  `socket_stats()` call that blocks while holding that mutex can also delay
  another thread's `send_video()` call.

Obtain the managed stats handle before moving the transport into the sender:

```rust,ignore
let policy = ReconnectPolicy {
    mode: ReconnectMode::Background,
    max_attempts: Some(20),
    backoff: BackoffStrategy::Exponential {
        base: Duration::from_millis(100),
        max: Duration::from_secs(10),
    },
    // Size this one: worst-case outage × messages per second. Whatever
    // does not fit is evicted (DropOldest) or refused (Reject).
    gap_buffer_capacity: 256,
    overflow_policy: OverflowPolicy::DropOldest,
};
let managed = ManagedTransport::new(initial, factory, policy);

// Grab the stats handle BEFORE moving `managed` into the sender shell —
// the shell takes ownership of `managed`, but the handle keeps reading
// live counters (same pattern as `cancel_handle()`).
let stats = managed.stats_handle();
let sender = MuxSender::new(managed, MuxerConfig::default())?;
```

**`Ok(()) != delivered.`** Under the default `OverflowPolicy::DropOldest`,
a `send_bytes` call that returns `Ok(())` while the worker is
reconnecting only means the bytes were accepted into the gap buffer —
if the outage outlasts `gap_buffer_capacity`, older queued messages
(possibly including these) get silently evicted to make room. An
integrator's single-threaded relay pump that only checks `is_ok()`
will not notice frames going missing; poll `stats_handle()` if you
need to know.

**Visibility via `stats_handle()`.** `ManagedStatsHandle::stats()`
returns `Option<ManagedTransportStats>` — `None` only if the
gap-buffer lock was poisoned by a prior panic (same precedent as
`socket_stats()`); a healthy pipeline always gets `Some`. The snapshot
carries `reconnecting` (a worker is currently active), `gap_len`
(messages queued right now), and `gap_messages_dropped` /
`gap_bytes_dropped` (cumulative loss counts) — poll these from a
separate thread or an occasional check in the producer loop to detect
a flapping link or a growing backlog before it becomes a silent-loss
incident. `gap_messages_dropped` isn't only `DropOldest` eviction: it
also counts a queued message that no longer fits the *rebuilt*
transport's `max_payload` (dropped during drain rather than wedging it
forever) — under `Blocking` mode the same oversized message would
instead surface synchronously to the caller as `TooLarge`.

**Give-up reporting.** If the worker exhausts `max_attempts` for one
continuous outage (the budget resets after every successful
reconnect), the give-up surfaces exactly once: the *next* `send_bytes`
call after the worker quits returns `TransportError::Broken` instead
of the usual `Ok(())`. That call's own bytes are **not** queued — the
caller sees the error and owns the resend decision. Set
`max_attempts: None` to retry forever instead (only safe if your
transport factory is itself rate-limited or backed by exponential
backoff, otherwise a permanent peer outage produces a hot reconnect
loop on the worker thread).

Runnable: [examples/operations/managed_reconnect_background.rs](/examples/operations/managed_reconnect_background.rs).
