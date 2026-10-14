# tst-tcp

Raw MPEG-TS over TCP — caller and listener roles, plain or TLS via
`tcp://`/`tcps://` (rustls, native cert store).

Built on `tst-core`'s transport traits; pair with `tst-pipeline`'s
`MuxSender`/`DemuxReceiver` shells for the full mux-to-wire path. See
the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_core::transport::Transport;
use tst_tcp::TcpTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = TcpTransport::connect("tcp://127.0.0.1:7001")?;

    let ts_packet = [0u8; 188]; // caller-supplied MPEG-TS packet
    tx.send_bytes(&ts_packet)?;
    Ok(())
}
```

**Stability: Stable** — small, settled surface. See the [API
stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0.
