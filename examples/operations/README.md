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
dedicated worker thread owns the factory/backoff/drain loop, so
`send_*` never waits on backoff or a factory call: it enqueues into
the gap buffer and returns immediately, whether or not the link is
currently up. Watch stderr — the send loop keeps producing at a steady
cadence straight through the outage while a separate stats line shows
the gap buffer filling, messages being evicted, and the worker
recovering.

Cookbook: [Survive a flaky transport with reconnect + gap buffer](../../docs/cookbook/operations/managed-transport-reconnect.md)
(same page as §1 — covers both `ReconnectMode` variants).

## 3. `tee_disk_and_demux.rs` — fan-out byte-sink pattern

```sh
cargo run -p tst-examples --example tee_disk_and_demux -- <input.ts> <output.ts>
```

Use `add_byte_sink` on a `DemuxReceiver` to fan TS bytes out to multiple
consumers without copying: write to disk and feed a live demux loop in
one pass. The shape that wires the receiver into both archival and
analysis paths.

Adjacent cookbook recipes: [Print live `Stats` from a sender](../../docs/cookbook/operations/print-live-stats.md),
[Inject WebVTT POI cues into a live MPEG-TS uplink](../../docs/cookbook/operations/inject-webvtt-cues.md).
