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
§10.11 RECORD, `mode=record` SETUP over TCP-interleaved, or over UDP where
the server allows UDP media). A publish
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

The UDP push works against an `rtsp://` server only: an `rtsps://` server
refuses media over UDP unless it allows it (see
[TLS and UDP media](#tls-and-udp-media)). Push to it TCP-interleaved
(`-rtsp_transport tcp`, `protocols=tcp`).

**GStreamer and credentials.** `rtspsrc` and `rtspclientsink` 1.24 answer a
Basic challenge, and a Digest challenge only in the RFC 2069 form, without
`qop`, which the server refuses as a downgrade unless it was built with
`accept_digest_without_qop(true)`; with it, Digest MD5 works. Digest SHA-256
does not work with GStreamer 1.24 either way: it answers a SHA-256 challenge
with an MD5 response. Put the credential in the location
(`rtsp://user:pass@host:8554/demo`).

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
streaming. When the publisher ends (TEARDOWN, a dropped connection, the session
timeout, or the optional publish idle timeout below), the mount goes idle and its `generation` counter goes up by one.
By default the application transport stays open and silent; the next publisher on that
name feeds the same transport. A next publisher with the same tracks shows
as ordinary continuity discontinuities at the change. A next publisher with
other tracks is not seen by a demuxer that has seen the first (no event, no
sample) when both PMTs carry the same version number, which the server's
re-muxer (version 0, like this library's muxer) always does: a demuxer
takes a PMT with the version it already holds as a repeat. Use one mount
name per track layout, or turn end-on-exit on (below) and add the mount
again for each publisher, so that each gets a transport and a demuxer of
its own. `PublishMountHandle::publisher()` names the
current publisher (control-connection address, shape, start time, generation)
or returns `None` while the mount is idle.

To end the mount instead, set the server builder's
`end_publish_mount_on_publisher_exit(true)` for every mount, or register one
with `server.add_publish_mount_with(path, PublishMountConfig::new().end_on_publisher_exit(true))`
(an unset per-mount option inherits the server's; an explicit `false`
overrides it). A publisher that reached RECORD then ends the mount as it
leaves: the application transport reads `Closed`, later ANNOUNCEs on the
name answer `403`, and the application calls `remove_mount` and adds the
mount again to accept a new publisher. A publisher that never reached RECORD
does not end it. An ended on-demand mount stays in the table, and counts
toward the 256-mount bound, until it is removed.

`RtspServerBuilder::publish_idle_timeout(Duration)` is off by default; zero
leaves it unset. After RECORD it ends a publisher's session when no valid
RTP packet (a decodable header with the announced payload type) arrives for
that long, timed from RECORD or the last such packet. RTSP keepalives and
RTCP do not re-arm it, so it ends a publisher that keeps its session alive
while sending no media.

One connection holds one role: a reader SETUP or PLAY on a publisher's
connection, or an ANNOUNCE on a reader's, answers `455`.

### Per-name credentials and refusing a publisher

By default one credential (the builder's `auth_basic` or `auth_digest_*`)
guards every name, for publishers and readers alike. `server.mount_rules()`
returns a table you edit while the server runs: a rule per mount path, plus a
default for every other path. A rule's path must be canonical (`/amber`, never
`/amber/`); `set` refuses anything else. Each rule holds a publish policy
(ANNOUNCE and RECORD) and a read policy (DESCRIBE, SETUP and PLAY).

```rust,no_run
use tst_rtp::{
    MountCredential, MountPolicy, MountRule, RefuseStatus, RtspServerBuilder,
    SecretString,
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
// which takes the default rule's read side, itself `Inherit`: the
// server-wide setting (open here).
rules.set(
    "/amber",
    MountRule::new(
        MountPolicy::Require(vec![MountCredential::new("amber", pw)]),
        MountPolicy::Inherit,
    ),
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

A path rule's `Inherit` side uses the table's default rule; the default's `Inherit` uses the server-wide builder setting (`auth_*`). The server-wide setting
is the builder's credential, else open. `Open` waives a builder credential, `Require` replaces
it for that path and direction (the builder's own credential does not open a
`Require` path), and `Refuse` answers `403`, `404`, `453` or `503`.

Every `401` offers the table's challenge, and every credential is checked
in it, the builder's included. It starts as the builder's scheme and realm
when the builder configured a credential, else Digest MD5 and realm
`tstrans`; `set_challenge(scheme, realm)` changes it while the server runs.
A Digest answer computed for the old challenge is then answered `401` with
the new one. A `set_challenge(AuthScheme::Basic, …)` makes clients send
their password base64-encoded, readable by anyone on the path of an
`rtsp://` connection. A user name that contains `:` cannot authenticate
while the challenge is Basic, the server's own credential included; the
builder refuses one in `auth_basic` at `build()`.

A realm may not contain control bytes (`set_challenge` and the builder refuse
it); quotes and backslashes are escaped in the header. The bind URL carries
no credential: `build()` refuses a bind URL with a user name or a password
(`rtsp://admin:secret@0.0.0.0:8554`), which would otherwise have named a
credential the server does not use.

- **A publisher stays inside its mount.** A SETUP for a path other than the
  mount it announced answers `404`, like a missing mount, so it does not show
  which other mounts exist.
- **Refusal happens before any mount exists.** Every gated request is
  checked once, before its handler runs, so a refused or unauthenticated
  ANNOUNCE is answered before an on-demand mount is created or queued.
- **A rule edit does not reach a reader that is already playing until its
  next gated request.** A reader that keeps alive with OPTIONS or
  GET_PARAMETER sends none, so turning its mount to `Require` or `Refuse`
  does not stop the stream; its next SETUP, PLAY, PAUSE or TEARDOWN is judged
  by the edited rule, and `server.remove_mount(path)` ends it now.
- **A rule change reaches a streaming publisher at its next gated request.**
  OPTIONS and GET_PARAMETER are never gated, so a publisher that is already
  streaming keeps going until it sends SETUP, RECORD, PLAY, PAUSE or (usually)
  TEARDOWN, which then answers the refuse status and frees the slot. `server.remove_mount(path)` ends it now.
- **A session is judged on its role.** Once a session holds a role (publisher
  or reader), SETUP, PLAY, PAUSE, RECORD and TEARDOWN are judged on that
  role's mount and direction; ANNOUNCE and DESCRIBE are judged on the request
  URI. A reader's SETUP is judged on the mount it names, since it can move the
  reader there. A `Refuse` on the role's own mount ends the role at its next
  gated request (SETUP of that mount or one of its tracks, RECORD, PLAY,
  PAUSE, TEARDOWN). A reader's SETUP refused for another mount is answered
  with that mount's status and costs the reader nothing: it keeps its role
  and a playing reader its stream. ANNOUNCE and DESCRIBE are answered but do
  not revoke a role. A playing reader's SETUP of another mount is still
  answered by that mount's rule first (`404`, `401`, ...), and by `455` from
  the handler when the rule admits it.
- **What a `Refuse` rule hides, and what it does not.** On a server whose
  default rule is open, a DESCRIBE or SETUP of a
  `Refuse(RefuseStatus::NotFound)` path reads like one of a missing path.
  Three things still tell them apart for a peer with no credential: an
  ANNOUNCE without a valid SDP body answers `404` on a `Refuse` path, where
  a path with no rule answers `400` (the rule is judged before the body is
  read); a PLAY,
  PAUSE, RECORD or TEARDOWN on a connection with no session answers the
  rule's status on a `Refuse` path, where a path with no mount and no rule
  answers `454`, `454`, `455` and `200`; and where the other paths ask for a
  credential, a `Refuse` path answers its status where they answer `401`.
  `Refuse` is an access rule, not a way to keep a name secret.
- **The builder's `default_mount_rule` guards the first ANNOUNCE.** A rule set
  after `start()` cannot cover a request that arrived before it; the builder's
  default rule does. A server nobody edits behaves exactly as before.

### Refused-credential counts

`rules.auth_failures("/amber")` returns the rule's `MountRuleAuthFailures`
(`publish` and `read`), `None` for a path without a rule of its own, and
`rules.default_auth_failures()` the default rule's. A request counts when it
carried an `Authorization` header and was answered `401` for any reason but
a stale nonce: a wrong password, an unknown user, a malformed header, the
wrong scheme, and an honest client's first answer after `set_challenge`
changed the scheme or the realm. The first challenge every client gets (no
header yet), a stale nonce (a Digest header that does not carry the
connection's current nonce, malformed or not) and a `Refuse` status never
count. A request for a path without a rule counts on the default rule, so a
peer cannot create a counter; `set` on a path keeps its counts, `remove` and
`clear` drop them.

Read the counts as a rate, not as a reason to lock a name: anyone can raise
them (three refused answers per connection under Basic, two under Digest
after the bare first challenge, on as many connections as they like), so a
lock-out built on one name's count can be set off against that name's owner.
A peer that holds one valid credential resets its strikes with it and can try
other credentials without bound on one connection: the count is the only sign
of that.

### KLV timing on the re-muxed shape

KLV units are placed on the video timeline using the publisher's RTCP sender
reports: the two tracks' reports give the offset from the KLV track's RTP
clock to the video's, rounded down to a whole 90 kHz tick (with a guard for
the sender's own rounding of each report's time), so a unit's PTS is never
later than the sender stamped it and is exact for a sender whose reports are
tick-exact, as this library's push client's are.
`PublishMountStats::alignment` reads `Pending` while KLV is held for
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

### Readers and the connection cap

`max_sessions` (64 by default) counts connections, publishers' and
readers' alike, so readers that hold every connection shut publishers out.
`RtspServerBuilder::max_readers(n)` caps the reader sessions: a reader takes
a slot at its SETUP and holds it until its role ends (TEARDOWN, a refusal on
its own mount, the end of its connection), readers of a multicast mount
count, and a reader's SETUP over the cap is answered `453 Not Enough
Bandwidth` and its connection closed. With `n` below `max_sessions`, readers
never hold the difference.

What it does not promise: a connection with no session yet (silent, or
OPTIONS only, authenticated or not) still holds one of the `max_sessions`
until its 30 s admission deadline, so a publisher's connection can still
find the server full for a moment. `max_readers(0)` means no readers at all
(`max_sessions(0)` counts as 1), and a cap at or above `max_sessions`
protects nothing.

### TLS and UDP media

On an `rtsps://` server only the RTSP connection is encrypted: media over
UDP would leave it as plain RTP. So by default an `rtsps://` server allows
no UDP media: it answers `461 Unsupported Transport` to every SETUP whose
transport is not TCP-interleaved, a reader's or a publisher's, and refuses
`add_multicast_mount` with `InvalidConfig`. The `461` comes after the
credential check and the mount lookup, so it tells a peer nothing that a
`401` or `404` would hide. A reader that asks for UDP first, such as
GStreamer's `rtspsrc`, falls back to TCP-interleaved on the same
connection by itself; this library's client asks for TCP-interleaved on
`rtsps://` from the start. A publisher or a reader that forces UDP is
refused. `RtspServerBuilder::allow_udp_media(true)` lets UDP media through
on `rtsps://`, unencrypted; `allow_udp_media(false)` makes a plain
`rtsp://` server TCP-only.

The builder's `auth_basic` / `auth_digest_*` credential is the server-wide
setting that `Inherit` falls back to. Readers and publishers need not share
it: mount rules give each name and direction its own policy (see
[per-name credentials](#per-name-credentials-and-refusing-a-publisher)).

### Ending the application side

`into_recv_transport` can be called once per mount: a second call, from any
clone of the handle, returns `RtspServerError::TransportTaken`. Clone the
handle first if you want `stats()` and `publisher()` after taking the
transport. `PublishMountHandle::cancel()` ends the transport with
`ExplicitClose` and leaves readers and the publisher alone; `remove_mount` and
`stop()` end it with `Closed`, which `DemuxReceiver` reports as end of stream.

With end-on-exit on, a publisher that reached RECORD ends the transport the
same way when it leaves.

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
