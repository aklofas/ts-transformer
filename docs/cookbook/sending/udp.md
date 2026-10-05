# Send MPEG-TS over UDP

> **When to use this:** Raw UDP unicast or multicast is the lowest-common-denominator transport — ffmpeg, VLC, TSDuck, and STANAG 4609 receivers ingest it directly.

> **Related:**
> - [Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md)
> - [Pipeline guide](/docs/guides/pipeline.md) — the `MuxSender` / `Sender` shells
> - [Open a sender from an `srt://...?...` URL](/docs/cookbook/sending/sender-from-url.md) — reliable/encrypted alternative
> - [Use a custom (non-SRT) transport](/docs/cookbook/sending/custom-transport.md) — RTP-wrapped UDP and other wires

Raw MPEG-TS over UDP is the lowest-common-denominator broadcast transport.
ffmpeg, VLC, GStreamer, and most STANAG 4609 ground stations consume it
without configuration.

## Code

```rust
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_udp::UdpTransport;

let tx = UdpTransport::connect("udp://239.10.0.1:5004?iface=eth0&ttl=8")?;
// H.264 + async KLV; build a custom config with `MuxerConfig::builder()`.
let cfg = MuxerConfig::default();
let sender = MuxSender::new(tx, cfg)?;
sender.send_video(&nal_bytes, pts, key_frame)?;
sender.send_klv(&klv_bytes, pts, /*metadata_service_id=*/ 0x00)?;
```

## Verify with ffmpeg

```bash
ffmpeg -i 'udp://@239.10.0.1:5004?fifo_size=1000000' -c copy out.ts
```

For unicast (skip the `@`):

```bash
ffmpeg -i 'udp://0.0.0.0:5004?fifo_size=1000000' -c copy out.ts
```

## URL parameters

| Parameter | Default | Meaning |
|---|---|---|
| `iface` | OS default | Outgoing interface for multicast (IP addr or interface name) |
| `ttl` | OS default (1 for multicast) | IPv4 TTL or IPv6 hop limit |
| `tos` | none | IP TOS / DSCP byte (e.g., `0xb8` for EF) |
| `sndbuf` | OS default | SO_SNDBUF in bytes (suffixes: `K`, `M`) |
| `pkt_size` | 1316 (7×188) | Datagram payload size |
