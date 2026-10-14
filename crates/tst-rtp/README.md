# tst-rtp

RTP-over-UDP carrying MPEG-TS (RFC 2250) or a bare H.264 elementary
stream (RFC 6184), with an RTSP/1.0 + RTSP/2.0 client and server for
session negotiation. The server serves PLAY readers from mounts the
application pushes into and, since 0.8.0, accepts ANNOUNCE / RECORD
publishers. Pure Rust, no native dependencies.

Built on `tst-core`'s transport traits; pair with `tst-pipeline`'s
`MuxSender`/`DemuxReceiver` shells for the MPEG-TS-over-RTP path. See
the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_core::transport::Transport;
use tst_rtp::RtpTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = RtpTransport::connect("rtp://239.55.55.1:5004?ttl=8")?;

    let ts_packet = [0u8; 188]; // caller-supplied MPEG-TS packet
    tx.send_bytes(&ts_packet)?;
    Ok(())
}
```

**Stability: Stable** for the RTP transports; the `rtsp` module
(client + server) is formally **Provisional** — the client API
(`RtspClient`) is stable in practice, but the module's tier is set by
the still-evolving server surface (`RtspServer`). See the [API
stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

## Feature flags

The crate is a sync facade throughout — the RTSP client never spawns an
async runtime. `RtspServer` is the one exception: it runs an internal tokio
Runtime behind its own sync facade, and that Runtime is what the
`rtsp-server` feature gates.

| Feature | Default | Pulls in | Enables |
| --- | --- | --- | --- |
| `rtsp-server` | **on** | `tokio`, `tokio-util` | `RtspServer` + mounts (the RTSP push server) |
| `tls` | off | sync `rustls` | Client `rtsps://` (`RtspClient`) — no tokio |
| `rtsp-server-tls` | off | `rtsp-server` + `tls` + `tokio-rustls` | The server's `rtsps://` TLS acceptor |

Client-only consumers (e.g. a mobile UniFFI binding shipping just RTP/RTSP
client + SRT) build with `default-features = false` (add `tls` for
`rtsps://`) for a tokio-free dependency tree — `cargo tree -p tst-rtp
--no-default-features --features tls -e normal` has no `tokio` entry.
`rtsp-server-tls` implies both `rtsp-server` and `tls`; Cargo has no way to
express "`tls` AND `rtsp-server`" other than a third feature that requires
both.

**License:** MIT OR Apache-2.0.
