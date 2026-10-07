# Receive MPEG-TS over UDP

> **When to use this:** Ingest raw UDP unicast or multicast MPEG-TS from ffmpeg, VLC, or STANAG 4609 senders.

> **Related:**
> - [Send MPEG-TS over UDP](/docs/cookbook/sending/udp.md)
> - [MPEG-TS demux guide](/docs/guides/mpegts-demux.md)

```rust
use tst_pipeline::DemuxReceiver;
use tst_udp::UdpRecvTransport;

let rx = UdpRecvTransport::listen("udp://@239.10.0.1:5004?iface=eth0&rcvbuf=8M")?;
let mut receiver = DemuxReceiver::new(rx);

for event in &mut receiver {
    match event? {
        // ... handle DemuxEvent variants
        _ => {}
    }
}
```

## URL parameters

| Parameter | Default | Meaning |
|---|---|---|
| `iface` | OS default | Multicast join interface (IP addr or interface name) |
| `rcvbuf` | OS default | SO_RCVBUF in bytes (`8M` typical for high-bitrate streams); Linux clamps to `net.core.rmem_max` — a warning is logged when clamped |

For unicast bind, prefix the host with `@` to make intent explicit:
`udp://@0.0.0.0:5004`.
