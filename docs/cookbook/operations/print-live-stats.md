# Print live `Stats` from a sender

> **When to use this:** Building an operational dashboard, instrumenting a sender for production telemetry, or debugging packet loss in the field.

> **Related:**
> - [guides/srt.md](/docs/guides/srt.md) — `Stats` field list and `*_send_side` / `*_recv_side` split
> - [Example: `managed_reconnect`](/examples/operations/managed_reconnect.rs) — peer-thread observation pattern

Reach for this when building an operational dashboard, instrumenting a sender for production telemetry, or debugging packet loss in the field. `Socket::stats()` returns a snapshot of libsrt's per-socket counters — call it periodically and surface the deltas.

The most operationally interesting fields on a sender: `bytes_sent`, `packets_lost_send_side`, `packets_retransmitted`, `rtt`, and `mbps_estimated_bandwidth`. (Loss/drop counters are split by which side observed them — read `*_send_side` on a sender, `*_recv_side` on a receiver.) There's no standalone example for this; see [guides/srt.md](/docs/guides/srt.md) §`Stats` for the full field list and [examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs) for similar peer-thread observation patterns.

```rust,no_run
use tst_srt::SrtSocketBuilder;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sb = SrtSocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect("127.0.0.1:9000")?;
    for _ in 0..10 {
        thread::sleep(Duration::from_secs(1));
        let s = socket.stats()?;
        println!(
            "bytes_sent={} packets_lost_send_side={} retrans={} rtt={:?} bw_mbps={:.2}",
            s.bytes_sent, s.packets_lost_send_side, s.packets_retransmitted,
            s.rtt, s.mbps_estimated_bandwidth,
        );
    }
    Ok(())
}
```

No standalone example; see [examples/operations/managed_reconnect.rs](/examples/operations/managed_reconnect.rs) and [guides/srt.md](/docs/guides/srt.md) §`Stats`.
