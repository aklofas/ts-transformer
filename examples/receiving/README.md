# Receiving

Receivers that pull TS bytes from a transport (SRT, UDP, TCP, RIST,
RTP/RTSP, or file-replay) and either dump, demux, or re-mux them.
Twelve examples, in read-order:

## 1. `srt_caller_dump.rs` — caller-mode SRT receiver

```sh
cargo run -p tst-examples --example srt_caller_dump -- 127.0.0.1:9000 /tmp/dump.ts
```

The simplest receiver: connect as a caller, write raw TS bytes to disk.
Pair with [`../sending/srt_serve_ts_file.rs`](../sending/srt_serve_ts_file.rs)
or any other listener.

Cookbook: [Receive into a file](../../docs/cookbook/receiving/receive-to-file.md).

## 2. `srt_listener_to_file.rs` — listener-mode SRT receiver

```sh
cargo run -p tst-examples --example srt_listener_to_file -- 127.0.0.1:9000 /tmp/out.ts
```

Diff from §1: take the listener role instead of caller. Pair with any
caller-mode sender (e.g.
[`../sending/send_pipeline_to_socket.rs`](../sending/send_pipeline_to_socket.rs)).

## 3. `srt_recv_typed.rs` — typed-event receiver via `DemuxReceiver<T>`

```sh
cargo run -p tst-examples --example srt_recv_typed -- 9000
```

Diff from §1/§2: instead of writing raw bytes, run them through the
`DemuxReceiver<T>` shell so you get typed `DemuxEvent` items
(per-stream samples, PSI events, errors). The shape every consumer
that wants real-time access to the parsed stream uses.

## 4. `ts_relay_from_file.rs` — file → SRT

```sh
cargo run -p tst-examples --example ts_relay_from_file -- input.ts 127.0.0.1:9000
```

Reads a pre-muxed `.ts` from disk and relays it over SRT via the
TS-bytes-in `pipeline::Sender` path (the input is already muxed
upstream, e.g. by ffmpeg). Useful for testing SRT options against a
captured stream without standing up a live encoder. Pair with
`srt-live-transmit srt://:9000 file://con > /tmp/out.ts` (or another
SRT-aware listener) on the receiver side.

Cookbook: [Relay a captured `.ts` file over SRT](../../docs/cookbook/sending/relay-file-to-srt.md).

## 5. `demux_to_events.rs` — full file-feed demux loop

```sh
cargo run -p tst-examples --example demux_to_events -- /path/to/capture.ts
```

Reads a `.ts` file end-to-end and prints every `DemuxEvent` variant —
PSI + per-stream sample headers + non-conformant issues. Use this as
the diagnostic example when you want to see what the demuxer thinks
of a capture.

Cookbook: [Extract subtitle PES bytes from a captured `.ts` file](../../docs/cookbook/receiving/extract-subtitle-pes.md)
(adjacent recipe — the same demux loop with subtitle filtering).

## 6. `recv_udp.rs` — UDP receiver (unicast + multicast)

```sh
cargo run -p tst-examples --example recv_udp -- udp://@0.0.0.0:5004 out.ts
cargo run -p tst-examples --example recv_udp -- 'udp://@239.10.0.1:5004?iface=eth0' out.ts
```

Lets you verify the on-wire format against any UDP MPEG-TS producer
(our [`../sending/send_udp.rs`](../sending/send_udp.rs), ffmpeg,
GStreamer, VLC, STANAG 4609 sensor pods).

Cookbook: [Receive MPEG-TS over UDP](../../docs/cookbook/receiving/udp.md).

## 7. `recv_tcp.rs` — TCP listener receiver

```sh
cargo run -p tst-examples --example recv_tcp -- 0.0.0.0:7001 out.ts
```

The symmetric listener for [`../sending/send_tcp.rs`](../sending/send_tcp.rs):
accepts a single inbound caller on a fixed port, then drains the TS
bytestream to disk. Pairs with `send_tcp`, ffmpeg's `-f mpegts
tcp://host:port`, GStreamer's tcpsink, or any other TCP MPEG-TS
producer.

Cookbook: [Receive MPEG-TS over TCP](../../docs/cookbook/receiving/tcp.md).

## 8. `recv_rist.rs` — `tst-rist` quickstart receiver

```sh
cargo run -p tst-examples --example recv_rist
```

Binds a RIST receiver on a loopback port (`rist://@host:port` — the
`@` follows ffmpeg's listen-address convention) and reads packets
until interrupted. Calls out the `TransportError::Backpressure`
handling the librist poll-timeout produces between packets. Pair with
[`../sending/send_rist.rs`](../sending/send_rist.rs).

Cookbook: [Receive MPEG-TS over RIST](../../docs/cookbook/receiving/rist.md).

## 9. `recv_rtp.rs` — `tst-rtp` quickstart receiver

```sh
cargo run -p tst-examples --example recv_rtp
```

Binds an `rtp://` URL and recvs N packets, printing byte counts + the
malformed-packet counter. RTP has no handshake — `listen()` binds the
socket and is done, unlike the SRT caller/listener pair. Pair with
[`../sending/send_rtp.rs`](../sending/send_rtp.rs).

## 10. `recv_rtsp_camera.rs` — pull MPEG-TS-over-RTP from an RTSP camera

```sh
cargo run -p tst-examples --example recv_rtsp_camera -- rtsp://admin:secret@cam.local/h264
```

The full RTSP client workflow for STANAG 4609 / gimbaled-platform
cameras that advertise a single MPEG-TS m-line (PT=33, video + KLV +
audio multiplexed): OPTIONS/DESCRIBE, `setup_mp2t_auto`, bridge into a
[`DemuxReceiver`] via `into_recv_transport`, PLAY, iterate. Add
`?transport=tcp` to force TCP-interleaved delivery through NAT/firewall
setups that block UDP.

## 11. `recv_rtsp_h264.rs` — RTSP H.264 → Muxer → `.ts` file gateway

```sh
cargo run -p tst-examples --example recv_rtsp_h264 -- rtsp://cam.local/h264
```

Diff from §10: for cameras that expose a bare H.264 elementary stream
over RTSP (no enclosing MPEG-TS m-line), instead of `setup_mp2t_auto`
this uses `setup_h264_auto` + `into_h264_receiver` (the RFC 6184
depacketizer) and pushes each recovered access unit through a `Muxer`
to build a `.ts` file.

Cookbook: [Receive RTSP H.264 into a `.ts` file](../../docs/cookbook/receiving/recv-rtsp-h264-to-ts.md).

## 12. `recv_rtsp_publish.rs` — let encoders push into `RtspServer`

```sh
cargo run -p tst-examples --example recv_rtsp_publish
ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 -c:v libx264 \
    -preset ultrafast -tune zerolatency -f rtsp rtsp://127.0.0.1:8554/demo
```

Diff from §10/§11: the roles invert. The application runs the RTSP
server and publishers connect and push (ANNOUNCE / RECORD). With
`accept_unregistered_publishers(true)` any announced name becomes a
mount on demand; `next_publisher` hands each one back, and
`into_recv_transport` feeds it to a `DemuxReceiver`. The header lists
the ffmpeg (TCP, UDP) and GStreamer (MPEG-TS, H.264 + KLV) push
commands; ffmpeg cannot push KLV.

Cookbook: [Accept RTSP publishers (ANNOUNCE / RECORD ingest)](../../docs/cookbook/receiving/rtsp-publish-ingest.md).
