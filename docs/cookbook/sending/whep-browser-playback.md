# Serve a live stream to a browser over WebRTC (WHEP)

> **When to use this:** A browser must show a live H.264 or H.265 feed with its MISB KLV telemetry locked to each frame, at sub-second latency, with no media server in between.

> **Related:**
> - [WebRTC playback guide](/docs/guides/webrtc.md): mounts, KLV carriage, the endpoint, timeouts, browser support
> - [Reference player](/docs/webrtc/README.md): the three files the page loads
> - [Send KLV over HLS to a browser (hls.js)](/docs/cookbook/sending/hls-klv-to-web.md): the segmented alternative

The server side is a `WhepServer` with one mount; the page side is the
reference player. This recipe feeds the mount with MPEG-TS received over
SRT, so the mount demultiplexes the stream itself (`MountKind::Mp2t`). A
source that hands you access units and KLV records directly uses an
elementary mount instead; the guide shows that loop.

## Server (Rust)

```rust,no_run
use tst_srt::SrtListenerBuilder;
use tst_webrtc::{MountConfig, MountKind, WhepServer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Accept the encoder's SRT caller; any source of TS bytes works here.
    let mut listener = SrtListenerBuilder::new().bind("0.0.0.0:9000")?;
    let (mut socket, _peer) = listener.accept()?;

    // advertise= is the address viewers send media to: behind NAT, the public IP.
    // The media port defaults to the signalling port, on UDP.
    let server = WhepServer::bind("whep://0.0.0.0:8889?advertise=203.0.113.5")?;
    let mut cfg = MountConfig::default(); // H.264, audio dropped, KLV in SEI + data channel
    cfg.kind = MountKind::Mp2t;
    let live = server.add_mount("/live", cfg)?;

    let mut buf = [0u8; 1500]; // one SRT message (1316 bytes by default) per recv
    let mut chunks = 0u64;
    loop {
        match socket.recv(&mut buf) {
            // TS bytes, any size; a packet may span two calls. push_ts never
            // blocks: a full queue drops the frame and counts it.
            Ok(n) => live.push_ts(&buf[..n])?,
            Err(tst_srt::error::RecvError::ConnectionBroken) => break,
            Err(tst_srt::error::RecvError::TimedOut) => continue,
            Err(e) => return Err(Box::new(e)),
        }
        chunks += 1;
        if chunks % 1000 == 0 {
            let s = live.stats();
            println!("viewers={} frames={} klv_sei={} klv_dc={}",
                     s.viewers, s.frames_pushed, s.klv_sei_inserted, s.klv_dc_messages_sent);
        }
    }
    server.stop();
    Ok(())
}
```

A browser POSTs its offer to `http://203.0.113.5:8889/live`. Set
`cfg.video = VideoCodec::H265` for an HEVC feed; an offer with no usable
payload for the mount's codec is answered 406, so a page can fall back to
HLS. A complete program is the `serve_whep` example, which reads the SRT
side through a managed receiver and stops on Enter:
`cargo run -p tst-examples --example serve_whep -- --help`.

The same loop on the JVM is the `ServeWhep` example
(`./gradlew runServeWhep --args='input.ts'` from `bindings/jvm`), which
paces a `.ts` file instead of reading a socket.

## Page

Serve the three files under [`/docs/webrtc/`](/docs/webrtc/README.md) next
to your page and load the player as a module:

```html
<video id="v" autoplay playsinline muted></video>
<pre id="klv"></pre>
<script type="module">
  import { playWhep } from "./webrtc/whep-player.js";

  const session = await playWhep({
    video: document.getElementById("v"),
    url: "http://203.0.113.5:8889/live",
    onKlv: ({ pts, bytes, via }) => {
      // bytes is the ST 0601 record as the encoder pushed it: UL, BER length, value.
      document.getElementById("klv").textContent =
        `${via} pts=${pts} ${bytes.length} bytes`;
    },
  });

  window.addEventListener("pagehide", () => session.stop());
</script>
```

The record is emitted when its frame is painted where the browser exposes
the painted frame's RTP timestamp (Chromium), else on arrival, a few
frames ahead of the picture. A record that arrives both in the SEI and on
the data channel is emitted once, the SEI copy preferred; pass
`dedupe: false` to receive both copies.

## Production notes

- **HTTPS pages need `wheps://`.** A page served over HTTPS may only POST
  to an HTTPS endpoint:
  `wheps://0.0.0.0:8443?cert=server.crt&key=server.key&advertise=203.0.113.5`.
  Media stays on UDP under DTLS-SRTP either way.
- **Open the UDP port.** Viewers reach `media_port` (default: the
  signalling port number) over UDP at the advertised address. There is no
  TURN or STUN: a viewer that cannot reach that port cannot play.
- **Token.** `token=s3cret` on the URL makes every request carry
  `Authorization: Bearer s3cret`; pass the same `token` to `playWhep`.
- **Origin.** Responses allow every origin by default; set
  `cors_origin=https://ops.example` to restrict the page's origin.
- **Viewer cap.** `max_viewers=` (1 to 64, default 10); an offer over the
  cap is answered 503 with `Retry-After: 5`.
- **I/P-only video.** WebRTC plays frames in the order they arrive, so a
  source with B-frames plays badly; `MountStats::frames_pts_reordered`
  counts them.

## See also

- [WebRTC playback guide](/docs/guides/webrtc.md): the SEI and data-channel layouts, queues and timeouts, the browser table.
- [KLV guide](/docs/guides/klv.md): decoding the ST 0601 record the page receives.
- [Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md): another source for the mount's `push_ts`.
