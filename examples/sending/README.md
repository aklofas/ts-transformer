# Sending

Senders that push pre-muxed TS bytes (or muxed-on-the-fly TS) over a
transport. Eleven examples, in read-order:

## 1. `send_pipeline_to_socket.rs` — basic SRT sender

```sh
cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
```

Opens a `MuxSender` shell over an SRT caller socket, pushes synthetic
H.264 + KLV at 30 fps. The receiver-side analogue is
[`../receiving/srt_caller_dump.rs`](../receiving/srt_caller_dump.rs).

Cookbook: [Send a single TS packet](../../docs/cookbook/sending/send-single-packet.md).

## 2. `encrypted_send_recv.rs` — passphrase encryption

```sh
cargo run -p tst-examples --example encrypted_send_recv
```

Diff from §1: pass a passphrase + key length on the `SrtSocketBuilder`. The
listener side runs in the same process so the example is self-contained.

Cookbook: [Send video + KLV with passphrase encryption](../../docs/cookbook/sending/send-encrypted.md).

## 3. `sender_from_url.rs` — config from `srt://...?...` URL

```sh
cargo run -p tst-examples --example sender_from_url
```

Diff from §1: derive every SRT option from a URL with query parameters,
the way `tst-c::tst_*_open` consumes URLs. This is the surface that
makes the same configuration trivially shareable across language
bindings.

Cookbook: [Open a sender from an `srt://...?...` URL](../../docs/cookbook/sending/sender-from-url.md).

## 4. `srt_serve_ts_file.rs` — file-replay listener

```sh
cargo run -p tst-examples --example srt_serve_ts_file -- /path/to/source.ts 9000
```

Plays a captured `.ts` file as a listener over SRT, respecting PCR
cadence. Receivers that connect to it look like a live source. Different
shape from §1 — it uses the listener role, not the caller role.

## 5. `custom_transport.rs` — implement the `Transport` trait

```sh
cargo run -p tst-examples --example custom_transport
```

Build your own non-SRT wire by implementing `tst_core::Transport`. The
example wraps an in-memory `VecDeque` so you can see the shape; in
production you'd wrap a real socket / framing layer.

Cookbook: [Use a custom (non-SRT) transport](../../docs/cookbook/sending/custom-transport.md).

## 6. `send_udp.rs` — UDP sender (unicast + multicast)

```sh
cargo run -p tst-examples --example send_udp -- input.ts udp://239.10.0.1:5004
```

Reads an MPEG-TS file and ships it over UDP, datagram-by-datagram —
the lowest-common-denominator transport in broadcast and STANAG 4609 /
ISR deployments. Verify against ffmpeg's
`-f mpegts udp://host:port` on the receiving end.

Cookbook: [Send MPEG-TS over UDP](../../docs/cookbook/sending/udp.md).

## 7. `send_tcp.rs` — TCP caller sender

```sh
cargo run -p tst-examples --example send_tcp -- input.ts tcp://127.0.0.1:7001
```

Reads an MPEG-TS file and ships it over a reliable TCP bytestream —
useful when packet loss matters more than latency, or the receiver is
firewall-gated and a TCP connect-out is easier than UDP multicast.
Verify with `ffmpeg -listen 1 -i tcp://...`.

Cookbook: [Send MPEG-TS over TCP](../../docs/cookbook/sending/tcp.md).

## 8. `send_rist.rs` — `tst-rist` quickstart sender

```sh
cargo run -p tst-examples --example send_rist
```

Builds a sender from a `rist://host:port` URL (mirrors ffmpeg's RIST
URL syntax: `rist://@host:port`, `?profile=main&aes-type=256&secret=...`)
and sends a handful of MPEG-TS-looking packets. Calls out where RIST
differs from UDP (recovery buffer + RTCP + optional AES PSK) and from
SRT (no caller/listener handshake). Pair with
[`../receiving/recv_rist.rs`](../receiving/recv_rist.rs).

Cookbook: [Send MPEG-TS over RIST](../../docs/cookbook/sending/rist.md).

## 9. `send_rtp.rs` — `tst-rtp` quickstart sender

```sh
cargo run -p tst-examples --example send_rtp
```

Parses a `rtp://239.x.x.x:port?ttl=N` URL, opens an `RtpTransport`,
sends a few hand-built MPEG-TS packets through it — no encryption, no
reconnect, no PTS supplied by the transport, just bytes. Pair with
[`../receiving/recv_rtp.rs`](../receiving/recv_rtp.rs).

## 10. `send_rtsp_server.rs` — RTSP server quickstart

```sh
cargo run -p tst-examples --example send_rtsp_server
```

Binds an `RtspServer`, registers a unicast mount, and pushes a
synthetic Annex-B H.264 IDR NAL through the mount's broadcast channel.
The `MountHandle` push surface mirrors `MuxSender::send_*` on method
names and signatures — same caller code regardless of whether the
destination is an SRT `MuxSender` or an RTSP-fanned-out mount. Drive
with [`../receiving/recv_rtsp_camera.rs`](../receiving/recv_rtsp_camera.rs)
or `ffprobe rtsp://127.0.0.1:8554/live`.

## 11. `mux_to_hls.rs` — re-mux a `.ts` file to HLS segments + playlist

```sh
cargo run -p tst-examples --example mux_to_hls -- input.ts /tmp/hls 127.0.0.1:8080
```

Pipes a real `.ts` file through the `Publisher` trait family:
`MuxPublisher` → `HlsPublisher` → on-disk `.ts` segments + `.m3u8` +
an internal HTTP server. KLV passes through transparently. Verify with
`ffplay`/`vlc`/`mpv` against `http://localhost:8080/playlist.m3u8`.

Cookbook: [Serve MPEG-TS as HLS](../../docs/cookbook/sending/hls.md), [HLS + KLV to a web player](../../docs/cookbook/sending/hls-klv-to-web.md).
