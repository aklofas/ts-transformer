# tst-pipeline

Transport-agnostic pipeline shells — `Sender`/`MuxSender`/`RawSender`
(and their `Receiver`/`DemuxReceiver`/`RawReceiver` counterparts, plus
`Managed*` reconnect wrappers) that drive any
`tst_core::transport::Transport` implementation.

Built on `tst-core`'s trait contracts; pick a concrete transport crate
(`tst-srt`, `tst-udp`, `tst-tcp`, `tst-rtp`, `tst-hls`, `tst-rist`) to
plug in. See the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start — push pre-muxed TS bytes through any `Transport`

```rust
use tst_pipeline::{Sender, SenderConfig};
use tst_core::transport::{Transport, TransportError};

// Trivial in-memory sink so the example needs no network. Real
// consumers plug in `tst_srt::SrtTransport` (or any other
// `Transport` impl) here.
struct Sink(Vec<u8>);
impl Transport for Sink {
    fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
        self.0.extend_from_slice(b);
        Ok(())
    }
    fn max_payload(&self) -> usize { 1316 }
    fn close(&mut self) {}
    fn is_alive(&self) -> bool { true }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sender = Sender::new(Sink(Vec::new()), SenderConfig::default());

    // One pre-muxed TS packet (188 bytes, sync byte 0x47 first).
    let mut pkt = vec![0x47u8];
    pkt.extend(vec![0u8; 187]);
    sender.send_ts(&pkt)?;
    sender.flush()?;
    Ok(())
}
```

**Stability: Stable**, with three Provisional modules: `mux_publisher`
(HLS-adjacent, newer), `ext` (the `Pairer` extension) and `binding` (the
handle layer shared by the C, Python and JVM bindings). See the
[API stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md)
for the full per-module table.

**License:** MIT OR Apache-2.0.
