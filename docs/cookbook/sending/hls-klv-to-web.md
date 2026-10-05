# Send KLV over HLS to a browser (hls.js)

> **When to use this:** A browser-based map client should render live MISB KLV telemetry alongside the video, delivered as HLS — no native player, no plugin.

> **Related:**
> - [Publish MPEG-TS as an HLS stream](/docs/cookbook/sending/hls.md) — the base sender loop
> - [HLS guide](/docs/guides/hls.md) — serving modes, KLV carriage, latency tuning

KLV rides **inside** the `.ts` segments as a private-data elementary stream;
a JavaScript player pulls the KLV out of the fragment metadata and schedules
it against playback time.

This recipe covers both the producer side (a Python `MuxPublisher` with a
`PrivateData` KLV stream) and the client side (an hls.js snippet that handles
both the native `misbklv` path and a UL-anchored fallback for stock hls.js).

## Producer (Python)

Configure the KLV stream as `PrivateData` (PMT `stream_type = 0x06` +
`KLVA` registration descriptor) — the web-friendly carriage that hls.js can
surface. Pass **raw KLV LS bytes** to `send_klv`.

```python
from tstrans.hls import HlsPublisher, MuxPublisher
from tstrans.mpegts import (
    KlvStreamType, MuxerProgramConfigBuilder, Pts90khz, VideoCodec,
)

publisher = (
    HlsPublisher.builder()
    .bind("127.0.0.1:8080")            # loopback; front with a reverse proxy for exposure
    .output_dir("/var/cache/hls")
    .segment_duration_ms(4000)
    .build()
)

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    # PrivateData KLV: stream_type 0x06 + KLVA — the hls.js-native path.
    .add_klv(0x1031, KlvStreamType.PRIVATE_DATA, carries_pts=True)
    .build()
)

mp = MuxPublisher.with_config_hls(publisher, program)

# Push one keyframe-aligned AU + its KLV record at a shared PTS.
# key_frame=True auto-cuts a segment at the keyframe.
mp.send_video(nal, pts=Pts90khz.from_raw(pts), key_frame=True)
mp.send_klv(klv_ls_bytes, pts=Pts90khz.from_raw(pts))

# ... push the rest of the stream, then finalize ...
pub = mp.finish_into_publisher()
pub.finish()
```

In production put a reverse proxy or CDN in front of the built-in server:
during the run the playlist is served from memory, and `playlist.m3u8` is
written to `/var/cache/hls` only when the publisher finishes — see the
[HLS guide](/docs/guides/hls.md#serving-in-production).

## Client (hls.js)

hls.js parses the private-data PES out of each fragment and delivers KLV
samples on `FRAG_PARSING_METADATA`. Here "native `misbklv` path" means hls.js
reads the KLV directly off the MPEG-TS private-data PID — `tst-hls` carries KLV
in-band on that PID and does **not** emit CMAF `emsg` boxes (fMP4/emsg carriage
per MISB ST 1910.1 is a separate deferred feature). The snippet below handles
**both** that native path (hls.js ≥ 1.7 with `enableEmsgKLVMetadata: true`) and
older/stock hls.js (anchor on the 16-byte SMPTE UL to slice the KLV out of
the sample bytes):

```js
const hls = new Hls({ enableEmsgKLVMetadata: true }); // hls.js >= 1.7: native KLV path

hls.on(Hls.Events.FRAG_PARSING_METADATA, (_e, data) => {
  for (const s of data.samples ?? []) {
    const bytes = s.data instanceof Uint8Array ? s.data : new Uint8Array(s.data);
    // Native path: s.type === 'urn:misb:KLV:bin:1910.1' delivers the raw
    // KLV packet (16-byte SMPTE UL + BER length + value) directly.
    // Older hls.js (<= 1.6): KLV on the private-data PID rides the metadata
    // event path; anchor on the UL to slice the KLV out of the sample bytes:
    const klv = s.type === 'urn:misb:KLV:bin:1910.1' ? bytes : sliceAtUL(bytes);
    if (klv) onKlv(klv, s.pts); // s.pts is seconds on the media timeline
  }
});

hls.loadSource('https://example.com/hls/playlist.m3u8');
hls.attachMedia(video);
```

### The `sliceAtUL` helper

The 16-byte MISB ST 0601 SMPTE Universal Label that prefixes a UAS Datalink
LS is:

```
06 0E 2B 34 02 0B 01 01 0E 01 03 01 01 00 00 00
```

`sliceAtUL` scans the sample bytes for that prefix and returns the KLV from
there to the end (the UL + BER length + value):

```js
const KLV_UL = new Uint8Array([
  0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
  0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
]);

function sliceAtUL(bytes) {
  outer:
  for (let i = 0; i + KLV_UL.length <= bytes.length; i++) {
    for (let j = 0; j < KLV_UL.length; j++) {
      if (bytes[i + j] !== KLV_UL[j]) continue outer;
    }
    return bytes.subarray(i); // UL + BER length + value
  }
  return null; // no KLV in this sample
}
```

### Scheduling against playback

KLV samples arrive **per fragment, ahead of playback** — when hls.js parses
a fragment it hands you every KLV sample in it at once, seconds before those
frames are shown. `s.pts` is the sample's presentation time in seconds on
the media timeline. Do not render on arrival; schedule against
`video.currentTime`. Two common patterns:

- **Poll `video.currentTime`** on each animation frame / timer, and apply
  the most recent KLV whose `pts <= video.currentTime`.
- **Use a hidden metadata `TextTrack`.** Add each KLV as a `VTTCue` with the
  sample's start time and let the browser fire `cuechange` at the right
  moment:

```js
const track = video.addTextTrack('metadata', 'klv');
track.mode = 'hidden';

function onKlv(klvBytes, ptsSeconds) {
  const cue = new VTTCue(ptsSeconds, ptsSeconds + 0.1, '');
  cue.value = klvBytes;        // stash the raw KLV on the cue
  track.addCue(cue);
}
track.addEventListener('cuechange', () => {
  for (const cue of track.activeCues) renderTelemetry(cue.value);
});
```

## Carriage guidance

- **Web clients → `PrivateData`** (stream_type 0x06 + KLVA). This is the
  path hls.js surfaces.
- **STANAG toolchains → `SynchronousMetadata`** (stream_type 0x15). Strict
  STANAG 4609 / MISB receivers expect 0x15; the muxer prepends the 5-byte
  `Metadata_AU_cell` header (ITU-T H.222.0 §2.12.4.2) for you — you still
  pass raw KLV LS bytes.

Pick the mode when you build the program; the two are different PMT
`stream_type` values and different players expect different ones.

## See also

- [HLS guide](/docs/guides/hls.md) — modes, serving guidance, latency tuning.
- [Publish MPEG-TS as an HLS stream](/docs/cookbook/sending/hls.md) — the base sender loop.
- [KLV guide](/docs/guides/klv.md) — building the ST 0601 records you push.
