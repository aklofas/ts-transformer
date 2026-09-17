# Survive a flaky transport with reconnect + gap buffer

> **When to use this:** The wire is lossy — radio links, NAT timeouts, listener restarts.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `ManagedTransport`, `ReconnectPolicy`, and gap-buffer behavior
> - [Example: `managed_reconnect`](/examples/operations/managed_reconnect.rs)

Reach for this when the wire is lossy — radio links, NAT timeouts, listener restarts. `ManagedTransport<T>` decorates any `Transport` impl with a reconnect loop and a bounded gap buffer; the wrapped sender shell sees a `Transport` that occasionally pauses but never fails on transient breakage.

The factory closure rebuilds the inner transport on demand. `ReconnectPolicy` controls retries, backoff, and gap-buffer overflow behaviour.

```rust,no_run
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, MuxSender, OverflowPolicy, ReconnectPolicy, TransportError,
};
use tst_srt::{SocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let factory = || -> Result<SrtTransport, TransportError> {
        // Bind-then-chain: mutators borrow, terminal `connect` borrows.
        let mut sb = SocketBuilder::new();
        sb.latency(Duration::from_millis(120));
        let socket = sb
            .connect("127.0.0.1:9000")
            .map_err(|e| TransportError::Broken(format!("connect failed: {e}")))?;
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
    };
    let managed = ManagedTransport::new(initial, factory, policy);
    let _sender = MuxSender::new(managed, MuxerConfig::default())?;
    Ok(())
}
```

Runnable: [examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs).

## Background mode: never stall the producer

> **When to use this:** a single-threaded relay pump — one thread both
> produces frames and calls `send_bytes`/`send_video` — where blocking
> that thread through a whole reconnect window means the upstream
> source backs up or drops frames on the floor anyway. If losing the
> freshest frame is worse than losing an old one, "fresh beats
> complete" is the right tradeoff, and `Background` mode is what makes
> it possible: the producer thread never waits out the reconnect
> backoff or a factory call.

By default (`ReconnectMode::Blocking`) a `send_bytes` call that hits a
broken transport blocks the caller for the whole reconnect window.
Set `mode: ReconnectMode::Background` and a per-outage worker thread
takes over the factory/backoff/drain loop instead — `send_bytes`
enqueues into the gap buffer under `overflow_policy` without waiting
on backoff or the factory call, whether or not the sink is currently
reachable. It does not wait on the worker's in-flight inner send
either (that one call is unbounded against a peer that has stopped
reading), only on the gap buffer's own short critical sections — and
the same is true of `stats_handle().stats()`:

```rust,ignore
let policy = ReconnectPolicy {
    mode: ReconnectMode::Background,
    max_attempts: Some(20),
    backoff: BackoffStrategy::Exponential {
        base: Duration::from_millis(100),
        max: Duration::from_secs(10),
    },
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
