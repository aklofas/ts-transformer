# Accept RTSP publishers (ANNOUNCE / RECORD ingest)

> **When to use this:** Encoders push to you over RTSP instead of you pulling
> from them: ffmpeg `-f rtsp`, GStreamer `rtspclientsink`, a relay forwarding a
> stream, or a hardware encoder configured with an RTSP push URL. You want each
> pushed stream as MPEG-TS in your application, and you may want to re-serve it
> to RTSP players, without running a separate media server in front.

> **Related:**
> - [`/docs/languages/rust.md#rtsp-publisher-ingest`](/docs/languages/rust.md#rtsp-publisher-ingest): the API list, the accepted shapes and the stats
> - [`examples/receiving/recv_rtsp_publish.rs`](/examples/receiving/recv_rtsp_publish.rs): runnable Rust twin with full commentary
> - [`bindings/c/examples/receiving/recv_rtsp_publish.c`](/bindings/c/examples/receiving/recv_rtsp_publish.c): the C twin (`TST_HAS_RTP`), with the API list in [`/docs/languages/c.md#rtsp-publisher-ingest-tst_has_rtp`](/docs/languages/c.md#rtsp-publisher-ingest-tst_has_rtp)
> - [Ingest H.264 from an RTSP camera and remux to MPEG-TS](/docs/cookbook/receiving/recv-rtsp-h264-to-ts.md): the pull direction, when the camera is the server
> - [Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md): a simpler path when the sender can push plain MPEG-TS
> - [Deferred features](/docs/project/deferred-features.md): what the publisher role does not do yet

`RtspServer` takes the publisher role of RTSP 1.0 (RFC 2326 §10.3 ANNOUNCE,
§10.11 RECORD, `mode=record` SETUP over TCP-interleaved or UDP). A publish
mount turns whatever the publisher pushed into MPEG-TS and hands it to the
application as an `RtpRecvTransport`, so the ordinary `DemuxReceiver` loop
reads it. The same mount serves PLAY readers from the same bytes.

---

## Rust

```rust,no_run
use std::time::Duration;
use tst_pipeline::DemuxReceiver;
use tst_rtp::RtspServerBuilder;

let mut builder = RtspServerBuilder::new("rtsp://0.0.0.0:8554")?;
builder.accept_unregistered_publishers(true); // any announced name becomes a mount
let server = builder.build()?;
server.start()?;
while let Some(mount) = server.next_publisher(Duration::from_secs(3600))? {
    let path = mount.mount_path().to_string();
    let transport = mount.into_recv_transport()?; // take-once per mount
    std::thread::spawn(move || {
        for ev in DemuxReceiver::new(transport) {
            println!("{path}: {ev:?}");
        }
    });
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`next_publisher` returns `Ok(None)` when its timeout passes with no new
mount, which ends this loop. A long-running service loops on a short timeout
instead and checks its own stop condition, as the example does. To accept
only names you choose, leave the flag off and register each one with
`server.add_publish_mount("/name")`, which returns the same handle.

---

## Push commands

```bash
# ffmpeg, TCP-interleaved (pushes elementary H.264):
ffmpeg -re -i in.mp4 -c:v libx264 -preset ultrafast -tune zerolatency \
    -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/demo

# ffmpeg, UDP:
ffmpeg -re -i in.mp4 -c:v libx264 -preset ultrafast -tune zerolatency \
    -f rtsp -rtsp_transport udp rtsp://127.0.0.1:8554/demo

# GStreamer, MPEG-TS over RTP (video + KLV muxed by the publisher).
# rtspclientsink payloads its input itself: feed it the TS, and it
# picks rtpmp2tpay and announces MP2T/90000 (payload type 33).
gst-launch-1.0 filesrc location=in.ts ! tsparse set-timestamps=true \
    ! rtspclientsink location=rtsp://127.0.0.1:8554/demo

# GStreamer, elementary H.264 + KLV (two tracks, re-muxed by the server).
# rtspclientsink picks rtph264pay and rtpklvpay for these caps. The KLV
# PES packets must carry a PTS (synchronous KLV does): without one,
# tsdemux emits untimed KLV and every KLV RTP packet gets the same
# timestamp, which the server cannot place on the video timeline.
gst-launch-1.0 filesrc location=in.ts ! tsdemux name=d \
    d. ! queue ! h264parse ! s.sink_0 \
    d. ! queue ! meta/x-klv ! s.sink_1 \
    rtspclientsink name=s location=rtsp://127.0.0.1:8554/demo
```

**KLV caveat.** ffmpeg cannot push KLV over RTSP. Its RTSP muxer announces
elementary tracks only, never MPEG-TS, and it stops with
`Unsupported codec klv` when the input carries a KLV stream. To get KLV into a
publish mount, push MPEG-TS from GStreamer (`rtpmp2tpay`), or push H.264 with a
KLV track (`rtpklvpay`), or send the TS over SRT or UDP instead of RTSP.

---

## Key points

### Accepted shapes

| What the publisher announces | What the application receives |
|---|---|
| One MPEG-TS track (`MP2T/90000`, or static payload type 33) | The publisher's TS bytes, unchanged |
| One H.264 video track (`H264/90000`) | TS re-muxed by the server: video on PID 0x100 |
| H.264 plus one KLV track (`smpte336m/90000`) | TS re-muxed by the server: video on PID 0x100, KLV on PID 0x101 (async, with PTS) |

Anything else answers `415 Unsupported Media Type`: audio, H.265, two video
tracks, KLV without video, or an H.264 or KLV track whose clock rate is not
90000. On the re-muxed shapes the TS starts at the first keyframe; earlier
access units are dropped and counted in `aus_dropped`.

### One publisher per name, and generations

A mount holds at most one publisher. A second ANNOUNCE on a mount that has a
live publisher answers `403 Forbidden` and leaves the first publisher
streaming. When the publisher ends (TEARDOWN, a dropped connection, or the
idle reaper), the mount goes idle and its `generation` counter goes up by one.
The application transport stays open and silent; the next publisher on that
name feeds the same transport, and the demuxer sees ordinary continuity
discontinuities at the change. `PublishMountHandle::publisher()` names the
current publisher (control-connection address, shape, start time, generation)
or returns `None` while the mount is idle.

One connection holds one role: a reader SETUP or PLAY on a publisher's
connection, or an ANNOUNCE on a reader's, answers `455`.

### Per-name credentials and refusing a publisher

By default one credential (the builder's `auth_basic` or `auth_digest_*`)
guards every name, for publishers and readers alike. `server.mount_rules()`
returns a table you edit while the server runs: a rule per mount path, plus a
default for every other path. Each rule holds a publish policy (ANNOUNCE and
RECORD) and a read policy (DESCRIBE, SETUP and PLAY).

```rust,no_run
use secrecy::SecretString;
use tst_rtp::{
    MountCredential, MountPolicy, MountRule, RefuseStatus, RtspServerBuilder,
};

let mut builder = RtspServerBuilder::new("rtsp://0.0.0.0:8554")?;
builder.accept_unregistered_publishers(true);
// The default rule refuses a publisher on every path without its own rule.
// Reads stay `Inherit`: open here, since the server has no credential.
builder.default_mount_rule(MountRule::new(
    MountPolicy::Refuse(RefuseStatus::NotFound),
    MountPolicy::Inherit,
));
let server = builder.build()?;
server.start()?;

let rules = server.mount_rules();
let pw = SecretString::from("amber-secret".to_string());
// "/amber" takes its own publisher credential. Its read side is `Inherit`,
// which means the server-wide setting (open here), not the default rule.
rules.set(
    "/amber",
    MountRule::new(
        MountPolicy::Require(vec![MountCredential::new("amber", pw)]),
        MountPolicy::Inherit,
    ),
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Inherit` is the server-wide behavior (the builder's credential, else open),
`Open` waives a builder credential, `Require` replaces it for that path and
direction (the builder's own credential does not open a `Require` path), and
`Refuse` answers `403`, `404`, `453` or `503`. A `Require` challenge uses the
builder's scheme and realm when it configured a credential, else the table's
(`set_challenge`, Digest MD5 and realm `tstrans` by default).

- **Refusal happens before any mount exists.** The check runs first in every
  gated handler, so a refused or unauthenticated ANNOUNCE is answered before
  an on-demand mount is created or queued.
- **A rule change reaches a streaming publisher at its TEARDOWN.** OPTIONS
  and GET_PARAMETER are never gated, so a publisher that is already streaming
  keeps going until it sends TEARDOWN, which then answers the refuse status
  and frees the slot. `server.remove_mount(path)` ends it now.
- **A session is judged on its role.** Once a session holds a role (publisher
  or reader), SETUP, PLAY, PAUSE, RECORD and TEARDOWN are judged on that
  role's mount and direction; ANNOUNCE and DESCRIBE are judged on the request
  URI. A `Refuse` answered to one of those requests ends the role.
- **The builder's `default_mount_rule` guards the first ANNOUNCE.** A rule set
  after `start()` cannot cover a request that arrived before it; the builder's
  default rule does. A server nobody edits behaves exactly as before.

### KLV timing on the re-muxed shape

KLV units are placed on the video timeline using the publisher's RTCP sender
reports. `PublishMountStats::alignment` reads `Pending` while KLV is held for
alignment, `SenderReport` once reports for both tracks arrived, and
`Provisional` when two seconds passed without them and the server fell back
to first-packet coincidence. MPEG-TS and video-only publishers read
`NotApplicable`.

B-frame publishers get a TS with PTS only. Each access unit that arrives with
a PTS below one already muxed is still muxed and counted in `aus_reordered`;
encode with `-tune zerolatency` (or no B-frames) for a clean stream.

### On-demand bounds

With `accept_unregistered_publishers(true)`, anyone who can reach the port,
and pass the server's auth when it is configured, can create mounts. The
server bounds that:

- At most 64 on-demand handles wait for `next_publisher`. An ANNOUNCE that
  would create one more answers `503 Service Unavailable` and creates nothing.
- At most 256 on-demand mounts live in the mount table, taken or not. An
  on-demand mount stays after its publisher leaves and after its handle is
  dropped, so removing idle names is the application's job:
  `server.remove_mount(path)` sends every session on the mount the
  server-initiated TEARDOWN notice, closes it, ends the application transport
  with `Closed`, and frees the name.
- Mounts registered with `add_publish_mount` never count toward either bound.

Readers and publishers share the server's one credential set
(`auth_basic` / `auth_digest_*` on the builder).

### Ending the application side

`into_recv_transport` can be called once per mount: a second call, from any
clone of the handle, returns `RtspServerError::TransportTaken`. Clone the
handle first if you want `stats()` and `publisher()` after taking the
transport. `PublishMountHandle::cancel()` ends the transport with
`ExplicitClose` and leaves readers and the publisher alone; `remove_mount` and
`stop()` end it with `Closed`, which `DemuxReceiver` reports as end of stream.

---

## Run the example

```bash
cargo run -p tst-examples --example recv_rtsp_publish
# In another terminal:
ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 -c:v libx264 \
    -preset ultrafast -tune zerolatency -f rtsp rtsp://127.0.0.1:8554/demo
```

The example prints video access units as they arrive and each mount's
publisher and stats every five seconds. Press Enter to stop it.
