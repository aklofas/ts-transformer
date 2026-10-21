# WebRTC Playback Guide

> **Who this is for:** You want a browser to play a live H.264 or H.265
> stream with its MISB KLV telemetry, frame by frame, with no plugin and
> no media server in between.

> **You will learn:**
> - What `tst-webrtc` is: a WHEP server with mounts, in Rust and on the JVM
> - The minimal Rust and Java serving loops, for elementary streams and for MPEG-TS
> - How KLV reaches the page twice, in an SEI message inside the frame and on a data channel, and how the reference player times it
> - The WHEP endpoint, the SDP answer, ICE and TLS as the server does them
> - What a mount does with audio, B-frames and slow viewers
> - Which browser features the player uses and where each browser stands

## What it is

`tst-webrtc` is a WHEP (WebRTC-HTTP Egress Protocol) server. A browser
POSTs an SDP offer to a mount's path and receives the mount's video over
WebRTC, with each KLV record locked to the frame it belongs to. Video is
passed through: an H.264 access unit is packetized per RFC 6184, an H.265
one per RFC 7798, and nothing is decoded or re-encoded. KLV travels in two
carriages at once: as a `user_data_unregistered` SEI message inside the
frame's access unit, and as a message on a `klv` data channel that names
the frame's RTP timestamp.

The server runs ICE-lite on one UDP port shared by every viewer, with DTLS
and SRTP from str0m, and retransmits lost packets on NACK (RTX). It needs
no TURN or STUN server, and offers none: viewers must reach the address
the server advertises. There is no ingest side (no WHIP), no transcoding,
and no simulcast.

The crate is `tst-webrtc` (feature `tls`, on by default, for `wheps://`).
The JVM binding exposes it as `org.tstrans.webrtc`
([JVM guide](/docs/languages/jvm.md#webrtc-playback-orgtstranswebrtc)). The
Python and C bindings do not expose it in this release. The browser side
is a reference player, three plain ES modules under
[`/docs/webrtc/`](/docs/webrtc/README.md).

## Quickstart (Rust)

An elementary mount takes access units, KLV records and audio one at a
time, with 90 kHz PTS values:

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::{MountConfig, WhepServer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // advertise= is the address viewers send media to (see "Network").
    let server = WhepServer::bind("whep://0.0.0.0:8889?advertise=192.0.2.10")?;
    let cam = server.add_mount("/cam1", MountConfig::default())?;

    // A browser POSTs its offer to http://192.0.2.10:8889/cam1.
    loop {
        let (au, pts, key_frame, klv) = next_frame_from_your_encoder()?;
        // A record waits for the first frame whose PTS is at or after its own.
        cam.push_klv(&klv, Pts90khz::new(pts), 0)?;
        cam.push_video(&au, Pts90khz::new(pts), key_frame)?;
    }
}
```

An MP2T mount takes MPEG-TS bytes instead and demultiplexes them itself,
playing the first video stream of the mount's codec and the KLV of that
stream's program:

```rust
use tst_webrtc::{MountConfig, MountKind, WhepServer};

let server = WhepServer::bind("whep://0.0.0.0:8889?advertise=192.0.2.10")?;
let mut cfg = MountConfig::default(); // non_exhaustive: default, then assign
cfg.kind = MountKind::Mp2t;
let live = server.add_mount("/live", cfg)?;
while let Some(chunk) = next_ts_chunk()? {   // any slice size; a packet may span two calls
    live.push_ts(&chunk)?;
}
server.stop();
```

Pushes never block: a push packetizes, queues for the server's media
thread and returns. A full queue drops the frame and counts it. The mount
closes when it is removed (`remove_mount`) or the server stops; later
pushes return `PushError::Closed`.

## Quickstart (Java)

```java
import org.tstrans.webrtc.*;

try (WhepServer server = WhepServer.bind("whep://0.0.0.0:8889?advertise=192.0.2.10");
     MountHandle mount = server.addMount("/cam1", MountConfig.defaults())) {
    mount.pushKlv(record, pts, /*metadataServiceId=*/ 0);
    mount.pushVideo(accessUnit, pts, /*keyFrame=*/ true);
}
```

The `ServeWhep` example (`bindings/jvm/src/examples/java/ServeWhep.java`)
feeds a `.ts` file to an MP2T mount at the speed its PTS says. The
[JVM guide](/docs/languages/jvm.md#webrtc-playback-orgtstranswebrtc) has the
binding's conventions: every setting lives in the URL, `stop()` versus
`close()`, and the exception kinds.

## The page

The reference player plays a mount in a `<video>` and hands every KLV
record to a callback:

```html
<video id="v" autoplay playsinline muted></video>
<script type="module">
  import { playWhep } from "/webrtc/whep-player.js";
  const session = await playWhep({
    video: document.getElementById("v"),
    url: "https://cam.example:8443/cam1",   // the mount's WHEP endpoint
    token: "s3cret",                        // only when the server has token=
    onKlv: ({ pts, bytes, via }) => renderTelemetry(bytes, pts),
  });
  // later: await session.stop();
</script>
```

`onKlv` receives `{ pts, bytes, via, rtpTimestamp }`: the record's 90 kHz
PTS as a BigInt, the KLV bytes as pushed, `"sei"` or `"dc"` for the
carriage, and the RTP timestamp of the frame the record is bound to. The
three files under [`/docs/webrtc/`](/docs/webrtc/README.md) are served as
static files from any web server; the player imports its worker and the
SEI extractor by relative URL, so they stay in one directory. The KLV guide
covers what to do with the bytes ([decoding ST 0601](/docs/guides/klv.md)).

Two rules follow from the browser, not from the server:

- A page served over HTTPS may only POST its offer to an HTTPS endpoint,
  so a public page needs a `wheps://` server (see "Network").
- The page's origin must be allowed: every response carries
  `Access-Control-Allow-Origin`, `*` unless the URL sets `cors_origin=`.

## Mounts

A mount is one video stream, one KLV stream and, optionally, one audio
stream, shared by every viewer of its path. `MountConfig` has:

| Field | Default | Meaning |
|---|---|---|
| `kind` | `Elementary` | `Elementary` takes `push_video`, `push_klv`, `push_audio`; `Mp2t` takes `push_ts`. The other family returns `PushError::WrongMountKind`. |
| `video` | `H264` | `H264` or `H265`. H.266 and AV1 are refused by `validate`. |
| `audio` | `Drop` | `Drop` discards pushed audio; `PassthroughOpus` forwards Opus packets (elementary mounts only). |
| `klv` | `SeiAndDataChannel` | Also `SeiOnly` and `DataChannelOnly`. |
| `sei_budget_bytes` | 64 KiB | The most KLV bytes carried in SEI per access unit (1 to 1 MiB). |
| `viewer_queue_packets` | 2048 | One viewer's queue, see "Viewers, queues and timeouts" (at least 16). |

What the video path does with a frame:

- **Keyframes.** The NAL units decide: an H.264 IDR picture, or an H.265
  IRAP picture (IDR, CRA or BLA), with its parameter sets. The `key_frame`
  argument is advisory; a disagreement is counted in
  `MountStats::keyframe_flag_mismatch`. SPS, PPS and VPS are cached per
  mount and prepended to every keyframe, because a browser takes parameter
  sets from the stream. A viewer that joins mid-GOP waits for the next
  keyframe (`viewers_waiting_keyframe`); the mount never asks the source
  for one, and a viewer's PLI is only counted (`plis_received`).
- **Decode order is presentation order.** WebRTC carries frames in decode
  order with the RTP timestamp as the presentation time, so the source
  must deliver I/P-only video. A frame whose PTS is below the previous
  frame's is counted in `MountStats::frames_pts_reordered` and plays badly.
- **RTP clock.** The RTP timestamp is the PTS on the 90 kHz clock plus a
  random per-mount origin. A PTS jump of more than 10 s back or 1 h ahead
  rebases the clock (`rtp_clock_rebases`) and the timestamp moves on by one
  frame.
- **Payload size.** Each access unit is packetized once, into payloads of
  at most 1180 bytes, and the same payloads go to every viewer with that
  viewer's sequence numbers and SSRC. With the 12-byte RTP header, up to
  16 bytes of header extensions, a retransmission's 2-byte original
  sequence number and up to 16 bytes of SRTP tag the packet is at most
  1226 bytes, which fits the 1280-byte IPv6 minimum MTU with the IP and
  UDP headers.
- **Profile choice.** The answer picks the first H.264 payload the offer
  declares with `packetization-mode=1` whose profile idc matches the
  mount's cached SPS, else the first mode-1 payload. Safari offers
  Constrained High first, and the server lists that profile so Safari
  negotiates it. An offer with no usable payload for the mount's codec is
  answered 406.

An MP2T mount runs one frame behind the bytes pushed: its demuxer emits an
access unit when the next one starts, so a file's last access unit is never
played unless more bytes follow. Demuxed audio is discarded whatever the policy (the demuxer has no
Opus mapping), and damaged units, units of another codec and the streams
the mount does not play are counted in `MountStats::demux_nonconformant`,
never failing the push.

## KLV carriage

A record pushed with PTS `p` waits in the mount for the first frame whose
PTS is at or after `p`, then travels inside that frame and next to it. A
record whose frame has already gone binds to the next frame; a record is
never discarded for timing. At most 1024 records wait; beyond that the
oldest is dropped and counted in `klv_records_dropped_unbound`. A clock
rebase binds every waiting record to the rebasing frame. The record's own
PTS always travels with it, so the page recovers the producer's timing
exactly.

### In the frame: the SEI message

One SEI NAL unit per record, inserted after the parameter sets and before
the first slice of the bound access unit (H.264 NAL type 6; H.265 prefix
SEI type 39). Its single SEI message is:

| Field | Bytes | Value |
|---|---|---|
| `payload_type` | 1 | 5, `user_data_unregistered` |
| `payload_size` | 1 or more | a `0xFF` per full 255, then the remainder |
| UUID | 16 | `74 73 74 4B 4C 56 00 01 8C A0 4E 0D 9B 2F 11 E9` (`tstKLV`, `00 01`, eight fixed bytes) |
| version | 1 | 1 |
| KLV PTS | 8 | the record's 90 kHz PTS, big-endian |
| record | rest | the KLV bytes exactly as pushed (16-byte UL, BER length, value) |

Emulation prevention applies as to any NAL unit. The layout is final: a
client written for a raw-KLV user-data SEI ports by changing the UUID and
skipping the 9 bytes (version and PTS) before the record. `tst_webrtc::sei`
has the Rust builder and parser; `whep-sei-extract.js` is the JavaScript
parser.

Records that would push an access unit's SEI bytes past `sei_budget_bytes`
are left out of the SEI, counted in `klv_sei_dropped_oversize`, and still
sent on the data channel. A record is never split across SEI messages. An
ST 0604 MISP timestamp SEI already in the access unit passes through
untouched.

### Next to the frame: the `klv` data channel

The server opens the channel itself, in-band (DCEP) on the SCTP association
the viewer's offer brings up. The contract for any client: the viewer's
offer must carry a data-channel section (create any data channel before
`createOffer`), and the channel arrives in `ondatachannel` with the label
`klv`. A viewer whose offer has no data-channel section gets video with KLV
in SEI, and no channel, and no drop is counted for it. The channel is
reliable and unordered: a record never waits behind an earlier one, and
each message names its frame's RTP timestamp, so the page orders records
by frame itself (the reference player does).

One binary message per record:

| Field | Bytes | Value |
|---|---|---|
| version | 1 | 1 |
| RTP timestamp | 4 | the bound frame's RTP timestamp, as in that viewer's video RTP header, big-endian |
| KLV PTS | 8 | the record's 90 kHz PTS, big-endian |
| record | rest | the KLV bytes exactly as pushed |

Without its 4-byte RTP timestamp, the message is byte-identical to the
SEI payload after the UUID (version, PTS, record), so a page can use either
carriage or cross-check both.

A record is at most `KLV_DC_LIMIT` (256 KiB) on the data channel;
`push_klv` refuses a larger one with `PushError::KlvTooLarge`. The server
sizes each session's SCTP send buffer (512 KiB) for a record at the limit.
A record is dropped for one viewer, and counted in `klv_dc_dropped_viewer`
and the session's `dc_messages_dropped`, when that viewer's send buffer
has no room for it (a transiently full buffer drops the record rather than
waiting), or when the message of record plus 13-byte header is larger than
the viewer's `max-message-size`: 64 KiB when the viewer states none, 256
KiB in current Chrome and Safari (so their largest record is 262 131
bytes), any size up to the limit in Firefox. The video is never held back
for the data channel.

### Timing in the page

The reference player takes the SEI records from each encoded frame before
it is decoded (an `RTCRtpScriptTransform` worker, or Chrome's
`createEncodedStreams` on the main thread) and the data-channel records as
they arrive. Where the browser exposes the painted frame's RTP timestamp
in `requestVideoFrameCallback` metadata, a record is emitted when its
frame is painted; elsewhere it is emitted on arrival, which is a few frames
ahead of the picture. Frames arrive in presentation order, so when a frame
is painted every record bound to it or to an earlier frame is emitted: a
record whose own frame is not painted (dropped by the decoder, or a paint
callback the browser skipped) goes out with the next painted frame, or at
most 64 pending frames later.

When both carriages deliver one record, the player emits it once: the key
is the KLV PTS plus a hash of the record bytes, and the SEI copy is
preferred (in the paint-locked path the SEI copy is emitted first; on
arrival the first copy wins). Pass `dedupe: false` to receive both copies,
each with its `via`.

## Audio

Under `AudioPolicy::Drop` (the default) `push_audio` discards the frame,
counts it in `audio_frames_dropped`, and the answer rejects the browser's
audio section (port 0). Under `AudioPolicy::PassthroughOpus` each
`push_audio` call is one Opus packet (RFC 6716, up to 3825 bytes), sent
per RFC 7587 on the 48 kHz clock, computed from each PTS on its own so the
rounding never accumulates, to every viewer whose offer asks to receive
Opus; other viewers get video and KLV only. Audio never decides whether an
offer is served, and never waits for a keyframe.

`PassthroughOpus` needs an elementary mount: `MountConfig::validate`
refuses it on an MP2T mount because the TS demuxer has no Opus mapping.
Transcoding (AAC to Opus) and Opus in TS are deferred
([deferred features](/docs/project/deferred-features.md)).

## Viewers, queues and timeouts

Each viewer has a queue the media thread writes out on every pass, so
`viewer_queue_packets` is a budget per pass. Every entry counts against it:
a video RTP payload, a KLV data-channel message or an Opus packet. A frame
that does not fit what is left is dropped whole for that viewer (a partial
frame cannot be decoded), its payloads are counted in
`viewer_packets_dropped_queue_full`, and the viewer waits for the next
keyframe. A keyframe larger than the whole budget is admitted into an empty
queue, so the queue can exceed the budget by one keyframe.

Timeouts, all on the server's media thread:

- A viewer that has not completed ICE and DTLS within 10 s of its answer is
  closed (`sessions_reaped_ice_timeout`).
- A connected viewer that stays disconnected for 30 s is closed
  (`sessions_reaped_disconnected`). ICE-lite drops the candidate pair 15 s
  after the viewer's last connectivity check, so a viewer that stops
  reading, or whose machine went away, is closed about 45 s after its last
  check. A viewer whose queue stays full for 30 s with nothing written is
  closed the same way.
- The signalling endpoint serves one connection at a time and a request
  must arrive whole within 5 s (else 408), so a client that holds a
  connection open delays every other POST and DELETE by up to 5 s.

`max_viewers` (1 to 64, default 10) caps the sessions across all mounts; a
POST over the cap is answered 503 with `Retry-After: 5`. The mount-to-media queue holds 4096
messages; a push that finds it full drops its frame and, until the next
keyframe, the frames that would reference it.

Counters: `WhepServer::stats()` (sessions, reaps, packets, unrouted
datagrams), `MountHandle::stats()` (frames, keyframes, KLV by carriage and
drop reason, audio, PLIs, NACKs, queue drops, clock rebases) and
`MountHandle::sessions()` (one `SessionStats` per viewer: ICE state, remote
address, packets, NACKs, drops). `stats()` on the server and `sessions()`
ask the media thread and can wait up to 2 s for its reply; a mount's
`stats()` never waits.

## The WHEP endpoint

| Request | Response |
|---|---|
| `POST /<mount>`, body `application/sdp` | `201 Created`, the answer as `application/sdp`, `Location: /<mount>/sessions/<id>` |
| `DELETE /<mount>/sessions/<id>` | `200 OK`; `404` for an unknown session |
| `PATCH /<mount>/sessions/<id>` | `204 No Content`; the trickle-ICE fragment is read and ignored (ICE-lite needs no remote candidates); `404` unknown session; `405` on any other target |
| `OPTIONS` on any target | `204` with the CORS headers |
| other methods | `405` with `Allow: POST, PATCH, DELETE, OPTIONS` |
| unknown mount | `404` |
| server at `max_viewers` (all mounts together) | `503`, `Retry-After: 5` |
| body not an SDP offer, or one payload type mapped to two formats | `400` with a plain-text reason |
| no offered video payload matches the mount | `406` with a plain-text reason |
| missing or wrong bearer token when `token=` is set | `401`, `WWW-Authenticate: Bearer` |
| request head over 16 KiB, body over 64 KiB, no complete request within 5 s | `431`, `400`, `408` |
| the server stopping | `503` |

Every response carries `Access-Control-Allow-Origin` (`cors_origin=`,
default `*`), `Access-Control-Allow-Methods: POST, PATCH, DELETE, OPTIONS`,
`Access-Control-Allow-Headers: Authorization, Content-Type, If-Match` and
`Access-Control-Expose-Headers: Location`. HTTP/1.1, one request per
connection, `Connection: close`.

The answer: video `sendonly` from the server on the payload chosen as
above, the audio section rejected or Opus, `a=ice-lite`, host candidates
(below), RTX for the video payload, NACK and PLI accepted, and the offer's
data-channel section accepted; the `klv` channel itself is opened over that
association once it is up.

## Network: ICE, ports, TLS

- **One UDP port.** `media_port=` (default: the signalling port number)
  carries every viewer's media; each datagram goes to the session that
  accepts it, and the ones no session accepts are counted in
  `datagrams_unrouted`.
- **Candidates.** A bind on a specific address offers that address, plus
  `advertise=` when it differs. A bind on `0.0.0.0` or `::` offers only
  loopback unless `advertise=` names the address viewers reach, which
  behind NAT is the public IP. `advertise=` takes one literal IP address.
- **No TURN, no STUN, no ICE over TCP.** The viewer must reach the
  advertised address on `media_port` over UDP.
- **TLS.** `wheps://0.0.0.0:8443?cert=server.crt&key=server.key` serves the
  signalling over HTTPS (rustls; feature `tls`). `cert=` and `key=` are PEM
  file paths; a `wheps://` URL without both, or a plaintext `whep://` URL
  with either, is refused at parse time, and a file that cannot be loaded
  is `WhepServerError::Tls` at bind with no port held. Media stays on UDP
  under DTLS-SRTP regardless.
- **Token.** `token=` makes every POST, PATCH and DELETE carry
  `Authorization: Bearer <token>`; refused requests are counted in
  `sessions_rejected_auth`.

The URL keys are listed in the
[URL query key table](/docs/reference/conventions.md#url-query-keys).

## Browser support

The player picks its path from what the browser offers:

| Capability | Chrome / Edge | Firefox | Safari | The player |
|---|---|---|---|---|
| `RTCRtpScriptTransform` | 141 | 117 | 15.4 | SEI read in a worker, per encoded frame |
| `createEncodedStreams` | 86 | no | no | SEI read on the main thread when the above is absent |
| `RTCEncodedVideoFrame.getMetadata().rtpTimestamp` | 127 | no (the frame's `timestamp` attribute is used) | 26 | binds an SEI record to its frame |
| `requestVideoFrameCallback` | 83 | 132 | 15.4 | the paint callback |
| `rtpTimestamp` in that callback's metadata | yes | no | no | paint-locked emission; elsewhere emission on arrival |
| data channel `max-message-size` | 256 KiB | any | 256 KiB | the largest record a viewer takes |

A browser without either encoded-frame API still plays the video and
receives KLV on the data channel.

The server's test suite answers an offer captured from Chrome and offers
reconstructed for Firefox and Safari, and plays to a WebRTC client in the
same process; the interop matrix's browser cell loads this player in
headless Chromium and checks that every record arrives by both carriages.

## Limits

- No ingest (WHIP), no transcoding, no simulcast, no B-frames, no TURN or
  STUN, no ICE over TCP, no trickle ICE from the server, no ICE restart.
- The Python and C bindings do not expose WebRTC in this release.
- A viewer's PLI is counted, not forwarded; the source decides the GOP.
- `advertise=` takes one address; a dual-homed deployment needs one server
  per address.

The deferred items and their revisit triggers are in
[deferred features](/docs/project/deferred-features.md).

## See also

- [Serve a live stream to a browser over WebRTC (WHEP)](/docs/cookbook/sending/whep-browser-playback.md): the recipe.
- [Reference player](/docs/webrtc/README.md): the files and their API.
- [KLV guide](/docs/guides/klv.md): decoding the records the page receives.
- [HLS guide](/docs/guides/hls.md): the segmented alternative when WebRTC cannot reach the viewer.
- [Compatibility](/docs/reference/compatibility.md#webrtc-playback-tst-webrtc): features, native dependency, browser table.
