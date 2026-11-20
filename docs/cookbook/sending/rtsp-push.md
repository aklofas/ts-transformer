# Push to an RTSP server (ANNOUNCE / RECORD)

> **When to use this:** The server receives and you publish: a media server
> such as mediamtx relays your stream to its readers, a recorder takes it, or
> another application runs this library's `RtspServer` with a publish mount.
> You open one TCP (or TLS) connection to it and send video and KLV, either as
> MPEG-TS or as elementary H.264 / H.265 with a KLV track.

> **Related:**
> - [`examples/sending/send_rtsp_push.rs`](/examples/sending/send_rtsp_push.rs): runnable Rust twin, both forms, with commands for this library's server, mediamtx and ffmpeg
> - [Accept RTSP publishers (ANNOUNCE / RECORD ingest)](/docs/cookbook/receiving/rtsp-publish-ingest.md): the receiving side, when your application is the server
> - [`/docs/languages/rust.md#rtsp-push-client`](/docs/languages/rust.md#rtsp-push-client), [Python](/docs/languages/python.md#rtsp-push-client), [JVM](/docs/languages/jvm.md#rtsp-push-client-orgtstransrtp): the API lists
> - [Cross-thread cancellation](/docs/reference/srt-cancel-handle.md#rtsp-push-what-a-cancel-and-each-close-do): what a cancel, each close and a drop do to a push
> - [Troubleshooting: RTSP push](/docs/troubleshooting.md#rtsp-push)
> - [Deferred features](/docs/project/deferred-features.md): UDP RECORD, audio tracks, the push in the C API

The RTSP client publishes with the steps of RFC 2326 §10.3 and §10.11: it
connects, sends ANNOUNCE with an SDP description of its tracks, sets each
track up with SETUP (`RTP/AVP/TCP;unicast;interleaved=…;mode=record`) and
sends RECORD. From then on it sends RTP interleaved on the same connection,
with RTCP sender reports beside it. `RtspPush::connect` (Rust) and
`RtspClient.push` (Python and the JVM) run all four steps and return the
recording session.

| What you announce | You send | The server receives |
|---|---|---|
| One MPEG-TS track (RFC 2250, payload type 33) | whole TS packets, at most 1 316 bytes (seven packets) per call, or a `MuxSender` / `Sender` over the push | your TS bytes |
| One H.264 track (RFC 6184, packetization mode 1) or one H.265 track (RFC 7798), and optionally one KLV track (RFC 6597) | one access unit per `send_video`, one KLV unit per `send_klv` | the video track, and the KLV track when announced, placed on the video's clock by the sender reports |

---

## Rust

The MPEG-TS form: the push is a `Transport`, so a `MuxSender` owns it.

```rust,no_run
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::MuxSender;
use tst_rtp::{PushDescription, RtspClientBuilder, RtspPush};

# fn frames() -> Vec<(Vec<u8>, Vec<u8>, Pts90khz, bool)> { Vec::new() }
let builder = RtspClientBuilder::new("rtsp://127.0.0.1:8554/cam")?
    // Pings keep a session the server would otherwise end for silence
    // (mediamtx: 10 s without a byte). Only a builder-made client pings.
    .keepalive_interval(Duration::from_secs(5));
let push = RtspPush::connect(builder, &PushDescription::Mp2t)?; // ANNOUNCE, SETUP, RECORD
// H.264 and KLV, each KLV record with a PTS so a receiver can pair it.
let mut program = MuxerProgramConfigBuilder::new(1, 0x1000);
program.add_video(0x1011, VideoCodec::H264);
program.add_klv(0x1031, KlvStreamType::PrivateData, true);
let mut config = MuxerConfig::builder();
config.add_program(program.build());
let sender = MuxSender::new(push, config.build()?)?;
for (au, klv, pts, key_frame) in frames() {
    sender.send_video(&au, pts, key_frame)?;
    sender.send_klv(&klv, pts, 0)?;
}
sender.finish()?; // no cancel; cannot report a lost tail (see "Ending a push")
# Ok::<(), Box<dyn std::error::Error>>(())
```

The elementary form: H.264 and KLV through the push itself.

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_rtp::{PushDescription, RtspClientBuilder, RtspPush, VideoTrack};

# fn frames() -> Vec<(Vec<u8>, Vec<u8>, Pts90khz)> { Vec::new() }
# let first_key_frame: Vec<u8> = Vec::new();
// The SDP carries the SPS and PPS: take them from a key frame (Annex B).
let video = VideoTrack::h264_from_key_frame(&first_key_frame)?;
let desc = PushDescription::Elementary { video, klv: true };
let builder = RtspClientBuilder::new("rtsp://127.0.0.1:8554/cam")?;
let mut push = RtspPush::connect(builder, &desc)?;
for (access_unit, klv, pts) in frames() {
    push.send_video(&access_unit, pts)?; // Annex B, decoding order
    push.send_klv(&klv, pts)?;           // one whole KLV unit, same timeline
}
push.close()?; // writes everything queued, TEARDOWN, reports the answer
# Ok::<(), Box<dyn std::error::Error>>(())
```

`RtspClient::announce`, `setup_record` and `record` are the same steps one at
a time, on a client from `RtspClientBuilder::connect`. Credentials go in the
URL (`rtsp://user:pass@host/path`) or through the builder's `auth`;
`rtsps://` needs `tst-rtp`'s `tls` feature.

## Python

```python
from tstrans.mpegts import KlvStreamType, MuxerProgramConfigBuilder, Pts90khz, VideoCodec
from tstrans.rtp import PushDescription, RtspClient, RtspClientConfig

cfg = RtspClientConfig("rtsp://127.0.0.1:8554/cam")  # keepalive=True by default

# MPEG-TS through a MuxSender.
program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .add_klv(0x1031, KlvStreamType.PRIVATE_DATA, carries_pts=True)
    .build()
)
push = RtspClient.push(cfg, PushDescription.mp2t())
with push.into_mux_sender(program) as sender:  # the sender now owns the push
    for i, (au, klv) in enumerate(frames):
        pts = Pts90khz.from_raw(i * 3000)
        sender.send_video(au, pts=pts, key_frame=(i % 30 == 0))
        sender.send_klv(klv, pts=pts)
    sender.finish()  # no cancel; cannot report a lost tail (see "Ending a push")
# The with exit frees the sender; without finish() it would cancel first.

# H.264 + KLV through the session itself.
desc = PushDescription.h264_from_key_frame(first_key_frame, klv=True)
with RtspClient.push(cfg, desc) as push:
    for i, (au, klv) in enumerate(frames):
        pts = Pts90khz.from_raw(i * 3000)
        push.send_video(au, pts=pts)
        push.send_klv(klv, pts=pts)
    push.teardown()  # raises RtspError if the server refuses the TEARDOWN
```

The `with` exit calls `close()`, which writes what is queued and sends
TEARDOWN but does not report the answer; `teardown()` does the same and
raises the refusal. Data calls raise `RtpError`, control calls `RtspError`.

## Java

```java
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.rtp.*;
import org.tstrans.shells.MuxSender;

var cfg = RtspClientConfig.builder("rtsp://127.0.0.1:8554/cam").build(); // keepalive on

// MPEG-TS through a MuxSender.
var program = MuxerConfig.builder()
    .addVideo(0x1011, VideoCodec.H264)
    .addKlv(0x1031, KlvStreamType.PRIVATE_DATA, true)
    .build();
// The session is a resource too: a refused config leaves it open, and the
// try closes it then; once the conversion succeeds its close does nothing.
try (RtspPushSession push = RtspClient.push(cfg, PushDescription.mp2t());
     MuxSender tx = push.intoMuxSender(program)) { // consumes the session
    for (int i = 0; i < frames.size(); i++) {
        long pts = i * 3000L;
        tx.sendVideo(frames.get(i), pts, i % 30 == 0);
        tx.sendKlv(klv.get(i), pts, 0);
    }
    tx.finish(); // no cancel, unlike close(); cannot report a lost tail
}

// H.264 + KLV through the session itself.
var desc = PushDescription.h264FromKeyFrame(firstKeyFrame, true);
try (RtspPushSession es = RtspClient.push(cfg, desc)) {
    for (int i = 0; i < frames.size(); i++) {
        long pts = i * 3000L;
        es.sendVideo(frames.get(i), pts);
        es.sendKlv(klv.get(i), pts);
    }
    es.teardown(); // throws RtspException if the server refuses the TEARDOWN
}
```

The RTSP steps and the control calls declare `RtspException`, the session's
data calls `RtpException` and `RtspException` (thrown `CLOSED` only by a
session a panic poisoned), and the `MuxSender` calls `MuxException` and
`BindingException`; `close()` is silent.

---

## Key points

### A send waits only for the queue

A data call (`send_ts`, `send_video`, `send_klv`, and a shell's send over a
push) returns once its packets are queued for the client's I/O thread, not
once they are on the wire. The queue is bounded, so against a server that
reads slowly a send waits for room. The wait ends when room frees, within
about 100 ms of a cancel from another thread (`cancel_handle()`), when the
session ends, or when this machine's kernel has taken no byte of the
connection for 5 s, which ends the connection. The 5 s start when the operating system stops taking bytes,
not when the server stops reading: against a server that had stopped
reading, one measured send ended about 9.7 s after it first waited. A
server that keeps taking a few bytes within every 5 s holds a send for as
long as it does so; the cancel handle is the way out.

Keepalive pings go out one at a time: while one still waits for the
connection to take its bytes, the next is not queued behind it but falls
due one interval later. So however long a server that reads slowly holds
the connection, what waits on it stays bounded: at most 64 KiB of media and
one frame, one request and one ping; the rest waits in the bounded queue.

Pace your sends as an encoder does. mediamtx in its default configuration
times what it relays by arrival (measured on 1.21.4), so a push that sends
ten seconds of media in a burst reaches its readers as a burst.

### Ending a push

| Call | Waits for | Writes | Reports |
|---|---|---|---|
| `RtspPush::close()` (Rust), `teardown()` (Python, JVM) | the queued media to be written, then up to 500 ms for TEARDOWN and its reply, then up to 2 s for the I/O thread | a last sender report per track, TEARDOWN | success only when the queued media was written and the server accepted the TEARDOWN; otherwise the first of: the cancel, once it has fired (`LocalCancel`; `CLOSED` in the bindings); `Protocol` with status `454` when the server had forgotten the session and queued media was dropped, whatever the TEARDOWN is answered; the TEARDOWN's own result (a refusal is `Protocol` with its status) |
| `close()` (Python, JVM), the `with` exit, try-with-resources | the same | the same | nothing |
| `Transport::close` (Rust; what a shell's close and `finish()` call) | the queued media, then up to 500 ms for TEARDOWN and its reply; does not wait for the thread | the same | nothing; `stats().frames_dropped_at_close` and `end_reason()` say what happened |
| Drop of an `RtspPush`, collection of a Python session | nothing | TEARDOWN, without waiting for its reply | nothing; what was queued is discarded |

Over a sender shell, end a stream with `finish()` (`MuxSender::finish()`,
`Sender::finish()`, and `finish()` on Python's and the JVM's `MuxSender` and
`Sender`): it cancels nothing, sends what the shell holds and closes the push, which, while the session is up,
writes everything queued and then sends TEARDOWN. `finish()` cannot tell you
whether that happened. The push's own close reports nothing through the
shell, so a session that ended first (the server closed it, the connection
failed or took no byte for 5 s, a keepalive ping was answered `454`) drops
what was still queued, up to 1 024 frames, and `finish()` still returns
without an error. When you must know, send through the session itself
(`send_ts`) and read the result of its close: `RtspPush::close()` returns
`Ok(())` only when the queued media was written and the server accepted the
TEARDOWN; in Python and on the JVM, `teardown()` raises otherwise. The
session object's `stats()` follows the push into the shell, so after a
shell's end its `frames_dropped_at_close` (`framesDroppedAtClose()`) counts
what was dropped (on the JVM until the session object is closed). A plain
`Sender` does not deliver a stream of one or two aligned TS packets at all:
it forwards bytes only once three aligned packets have confirmed the
alignment.
`MuxSender::close()` fires the push's cancel first, and in Python and on the
JVM so does the `close()` of either shell (`MuxSender` or `Sender`), so it
can drop packets queued a moment before. In Rust, `Sender::close()` and a dropped `MuxSender` or
`Sender` close the push without a cancel, with the same limit as `finish()`.

`close()` and `teardown()` in Python and on the JVM, called while a send,
`pause()` or `resume()` is running on another thread, fire the cancel first:
that call ends with `CLOSED` and what was queued is dropped. A call that is
only waiting for the session is not cancelled: it runs nothing and ends with
`CLOSED`. Called while nothing is in progress, the close writes the queue.
`stats()`, `end_reason()` (`endReason()`) and `end_detail()` (`endDetail()`)
never wait and never raise: they read the push's counters while a send waits
for room, and after `close()`, `teardown()` and a conversion.
`is_session_alive()` (`isSessionAlive()`) never waits: a call in progress
counts as live, and it reads false once the session is closed or converted. The conversions
(`into_mux_sender` / `into_sender`, `intoMuxSender` / `intoSender`) do not
wait for a send, `pause()` or `resume()` running on another thread: they
raise `RuntimeError` (throw `IllegalStateException`) and consume nothing,
and the session stays usable. After a panic inside a call, every call that
takes the session raises `RtspError(CLOSED)` (throws
`RtspException(CLOSED)`), and `close()` and `teardown()` still end the push.

### Keepalive and silence

A server may end a publisher it hears nothing from. mediamtx 1.21.4 closes a
recording publisher after 10 s without a byte, whatever it advertises (its
replies carry `timeout=55`). Media resets that clock, so a push that sends
continuously needs nothing. A push that can go quiet needs pings more often
than every 10 s: in Rust, `RtspClientBuilder::keepalive_interval`. The
default cadence is half the advertised session timeout (27.5 s against
mediamtx), too late. Only a client made by `RtspClientBuilder` pings; one
made by `RtspClient::connect` does not. Python's and the JVM's configs keep
pings on by default and take the interval as `keepalive_interval_ms` /
`keepaliveIntervalMs` (5000 against mediamtx's 10 s); without one, keep media
flowing, or expect the end described in
[Troubleshooting](/docs/troubleshooting.md#rtsp-push).

### Pause and resume

`pause()` queues a last sender report per track, waits until the queue is
written, then sends PAUSE. Data calls still queue and write their packets
while paused, and some servers end the session for that: mediamtx 1.21.4
closes the connection on any frame after PAUSE, and at PAUSE it takes the
path offline for its readers, who must reconnect after `resume()`. Stop
sending before `pause()` and start again after `resume()` returns.

### Timestamps and KLV

`pts` is taken as given: pass one that keeps counting. A PTS read from
MPEG-TS wraps at 33 bits and must be unwrapped first; the sender reports
follow the PTS, and a value that jumps back moves their clock back. Video and
KLV share one timeline, and each track's first sender report goes out before
its first RTP packet.

A KLV unit up to 65 523 bytes goes out as one RTP packet. mediamtx 1.21.4
keeps only the last packet of a KLV unit sent as several. This library's
server places KLV on the video from the sender reports
(`PublishMountStats::alignment` reads `SenderReport`); mediamtx 1.21.4 uses
them only on a path with `useAbsoluteTimestamp: true` and otherwise times
both tracks by arrival.

### Servers

| | This library's `RtspServer` | mediamtx 1.21.4 | ffmpeg 6.1, `-rtsp_flags listen` |
|---|---|---|---|
| Takes | MPEG-TS, H.264, H.264 + KLV; H.265 and KLV without video answer `415` | MPEG-TS; H.264 or H.265, with or without KLV | MPEG-TS, H.264, H.265; KLV as a data stream it has no codec for |
| A second publisher on a name | `403` to its ANNOUNCE | replaces the first at its RECORD (default) | one connection per run |
| Credentials | Basic, Digest MD5 or SHA-256; per mount through mount rules | Basic by default; Digest (MD5) when `rtspAuthMethods` lists it | none |
| `rtsps://` | with the `rtsp-server-tls` feature | with `rtspEncryption: optional` or `strict` | no (measured on a GnuTLS build) |
| Receiver reports to a TCP publisher | none | none | none |
| TEARDOWN | accepted; refused `403` after a mount rule withdrew the publish right | `200` | `501`: `close()` reports a refused TEARDOWN; the listener keeps recording until the connection closes, then writes its output |
| A silent publisher | ended by its session timeout, and by `publish_idle_timeout` when set | ended after 10 s without a byte | never ended |
| Notes | the [ingest recipe](/docs/cookbook/receiving/rtsp-publish-ingest.md) | drops an MPEG-TS RTP payload above 1 440 bytes (the push sends 1 316) | its MPEG-TS output misses the last access unit |

The mediamtx and ffmpeg columns were measured on Linux on loopback with a
default configuration unless the row names a setting. A mediamtx reader
gets what arrives after its PLAY and can attach only once the push is
recording. A credential given as `.auth(...)` (Rust) or `BasicAuth`
(Python, JVM) answers a Basic or a Digest challenge; one restricted to Digest
(`AuthSchemes::DigestOnly`, `DigestAuth`) refuses a Basic-only challenge,
which is mediamtx's default, without sending the password.

The interop matrix pushes to mediamtx (MPEG-TS, and H.264 + KLV) and to
ffmpeg's listener (MPEG-TS). GStreamer reads each mediamtx stream back: the
MPEG-TS capture must be byte-identical to what was sent, and the elementary
one must hold every pushed NAL unit in order and the pushed KLV bytes.
ffmpeg writes what it receives as its own re-mux, which drops the KLV; that
cell checks the video access units and that the push's close found its
queue written. See
[validation evidence](/docs/project/validation-evidence.md).

### Not built

UDP RECORD (`?transport=udp`, `transport_pref=TransportPref.UDP` in Python or
`TransportPref.UDP` on the JVM is refused before any SETUP is written), audio
tracks, RTSP 2.0 (it has no ANNOUNCE or RECORD: refused before the ANNOUNCE),
answering requests the server sends, and a C API for the push. The C binding
still receives from and serves RTSP. See
[deferred features](/docs/project/deferred-features.md).

---

## Run the example

```bash
cargo run -p tst-examples --example recv_rtsp_publish   # this library's server, port 8554
# In another terminal:
cargo run -p tst-examples --example send_rtsp_push -- rtsp://127.0.0.1:8554/cam-ts ts
cargo run -p tst-examples --example send_rtsp_push -- rtsp://127.0.0.1:8554/cam-es es
```

The example sends 10 s of synthetic 30 fps H.264 with one ST 0601 record per
frame, prints a line per second, its counters, and how the session ended.
Run it against mediamtx the same way (`./mediamtx` with its bundled
configuration listens on 8554); the file's header has the ffprobe command
that reads the stream back while the push runs, and the ffmpeg listen
command.
