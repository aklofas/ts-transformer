# tst-udp

Raw MPEG-TS over UDP — unicast and multicast (IPv4 + IPv6), with
ffmpeg-compatible URL syntax (`udp://host:port`,
`udp://@group:port` for multicast recv).

Built on `tst-core`'s transport traits; pair with `tst-pipeline`'s
`MuxSender`/`DemuxReceiver` shells for the full mux-to-wire path. See
the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_core::transport::Transport;
use tst_udp::UdpTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = UdpTransport::connect("udp://127.0.0.1:5004")?;

    let ts_packet = [0u8; 188]; // caller-supplied MPEG-TS packet
    tx.send_bytes(&ts_packet)?;
    Ok(())
}
```

**Stability: Stable** — small, settled surface. See the [API
stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0.
