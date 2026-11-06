# Operations

Patterns for production deployments — surviving flaky transports,
fanning bytes to multiple consumers, observability. Three examples:

## 1. `managed_reconnect.rs` — `ManagedTransport` reconnect with backoff

```sh
cargo run -p tst-examples --example managed_reconnect
```

Wrap any `Transport` in `ManagedTransport` and the pipeline survives
disconnects: bounded backoff, redelivery of the message that found the
link broken, and connection-state telemetry on the `tracing` facade.
This one runs the default `ReconnectMode::Blocking`: the send that hits
the break stalls until the reconnect lands, so nothing queues up behind
it (sizing the gap buffer for an outage is §2's subject).

Cookbook: [Survive a flaky transport with reconnect + gap buffer](../../docs/cookbook/operations/managed-transport-reconnect.md).

## 2. `managed_reconnect_background.rs` — non-blocking reconnect via `ReconnectMode::Background`

```sh
cargo run -p tst-examples --example managed_reconnect_background
```

Sibling to §1 — same flaky-peer shape, same `SrtTransport`, same
`ManagedTransport` machinery, but `policy.mode` is
`ReconnectMode::Background` instead of the default `Blocking`. A
dedicated worker owns reconnect backoff, factory calls and backlog drain.
While that worker is active or the gap buffer is non-empty, sends enqueue
and return without waiting for it; with no worker and no backlog, a send
uses the connected transport directly and can block. Watch stderr — the send loop keeps producing at a steady
cadence straight through the outage while a separate stats line shows
the gap buffer filling, messages being evicted, and the worker
recovering.

Cookbook: [Survive a flaky transport with reconnect + gap buffer](../../docs/cookbook/operations/managed-transport-reconnect.md)
(same page as §1 — covers both `ReconnectMode` variants).

## 3. `tee_disk_and_demux.rs` — record and demux in one pass

```sh
cargo run -p tst-examples --example tee_disk_and_demux -- <input.ts> <output.ts>
```

Compose a `Receiver` (aligned 188-byte packets), a `FileTransport` (the
recording) and a `Demuxer` (typed events) in one loop: each packet is sent
to the file, fed to the demuxer, and the events are drained. No callback
runs on a library thread. The example also spells out what the composition
leaves to the caller: flush the demuxer on a terminal error as well as at
clean end, and on a reconnecting transport drop the packet read across the
reconnect and reset both halves.

Adjacent cookbook recipes: [Print live `Stats` from a sender](../../docs/cookbook/operations/print-live-stats.md),
[Inject WebVTT POI cues into a live MPEG-TS uplink](../../docs/cookbook/operations/inject-webvtt-cues.md).
