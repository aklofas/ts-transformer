# Send MPEG-TS over TCP

> **When to use this:** You want UDP's payload with a reliable bytestream (and optional TLS via rustls) — pairs with `ffmpeg -listen 1 -i tcp://…` on the receive side.

> **Related:**
> - [Receive MPEG-TS over TCP](/docs/cookbook/receiving/tcp.md)
> - [Pipeline guide](/docs/guides/pipeline.md)
> - [Send MPEG-TS over UDP](/docs/cookbook/sending/udp.md) — when latency matters more than reliability
> - [Open a sender from an `srt://...?...` URL](/docs/cookbook/sending/sender-from-url.md) — reliable + encrypted transport

Raw MPEG-TS over TCP is the reliable-bytestream sibling of UDP. Useful when
packet loss matters more than latency, or when firewall topology blocks
multicast. `ffmpeg -listen 1 -i tcp://...` and GStreamer's `tcpsink` are
common counterparts on the receiver side.

## Code

```rust
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_tcp::TcpTransport;

let tx = TcpTransport::connect("tcp://127.0.0.1:7001?nodelay=1")?;
// H.264 + async KLV; build a custom config with `MuxerConfig::builder()`.
let cfg = MuxerConfig::default();
let sender = MuxSender::new(tx, cfg)?;
sender.send_video(&nal_bytes, pts, key_frame)?;
sender.send_klv(&klv_bytes, pts, /*metadata_service_id=*/ 0x00)?;
```

## TLS (`tcps://`)

`tcps://` caller URLs accept both DNS hostnames and IP literals. TLS presents
whatever you dialed as the SNI and verifies the server certificate against it:

- Dial a **hostname** (e.g. `tcps://relay.example.com:7001`) → the certificate
  must carry a matching `dnsName` SubjectAltName (SAN).
- Dial an **IP literal** (e.g. `tcps://192.168.1.10:7001`) → the certificate
  must carry a matching `iPAddress` SAN.

Listener URLs (`?listen=1`) still require an IP literal because the OS must
bind to a specific address. Use `0.0.0.0` (or `::` for IPv6) to listen on all
interfaces.

Generate a certificate with a hostname SAN using OpenSSL:
```bash
openssl req -x509 -nodes -newkey rsa:2048 \
  -subj "/CN=relay.example.com" \
  -addext "subjectAltName=DNS:relay.example.com" \
  -out server.crt -keyout server.key
```

Or with an IP SAN if you dial by IP:
```bash
openssl req -x509 -nodes -newkey rsa:2048 \
  -subj "/CN=server" \
  -addext "subjectAltName=IP:192.168.1.10" \
  -out server.crt -keyout server.key
```

```rust
use tst_tcp::TcpTransport;

// Dial by hostname — cert must have a dnsName SAN for "relay.example.com".
let tx = TcpTransport::connect("tcps://relay.example.com:7001?ca=ca-bundle.pem")?;

// Dial by IP literal — cert must have an iPAddress SAN for 192.168.1.10.
let tx = TcpTransport::connect("tcps://192.168.1.10:7001?ca=ca-bundle.pem")?;
```

## URL parameters

| Parameter | Default | Meaning |
|---|---|---|
| `nodelay` | `1` (enabled) | TCP_NODELAY; disable Nagle by default for low-latency streaming (`0` re-enables Nagle for bulk throughput) |
| `keepalive` | disabled | SO_KEEPALIVE idle time in seconds |
| `rcvbuf` | OS default | SO_RCVBUF in bytes (suffixes: `K`, `M`) |
| `sndbuf` | OS default | SO_SNDBUF in bytes |
| `connect_timeout` | 10 | Caller-side connect timeout in seconds. Applied per resolved address — a hostname resolving to multiple addresses may take up to N× this value before failing |
| `ca` | OS native store | Custom CA bundle PEM path (TLS caller only) |

## Verify with ffmpeg

```bash
# Listener first:
ffmpeg -listen 1 -i tcp://0.0.0.0:7001 -c copy out.ts

# Then caller (from our send_tcp example):
cargo run -p tst-examples --example send_tcp -- input.ts tcp://127.0.0.1:7001
```
