# Publish MPEG-TS as an HLS stream

> **When to use this:** Browsers and CDNs pull the stream over plain HTTP — the segmenter writes `.ts` segments + a rolling `.m3u8`, with an optional built-in HTTP server (Basic auth + HTTPS).

> **Related:**
> - [HLS guide](/docs/guides/hls.md) — serving modes, KLV carriage, latency tuning
> - [Send KLV over HLS to a browser (hls.js)](/docs/cookbook/sending/hls-klv-to-web.md)
> - [Send MPEG-TS over TCP](/docs/cookbook/sending/tcp.md) — same crate family, raw TCP variant
> - [Open a sender from an `srt://...?...` URL](/docs/cookbook/sending/sender-from-url.md) — reliable point-to-point

> **Conformance notes (RFC 8216):** `#EXTINF` reflects media-presentation
> (PTS) duration when driven through `MuxPublisher`; raw `push_ts` callers
> fall back to wall-clock. `#EXT-X-TARGETDURATION` is fixed at the ceiling
> of `segment_duration` and never changes after the playlist is opened. Live
> mode requires `playlist_window × segment_duration ≥ 3 × ceil(segment_duration)`;
> `HlsPublisherBuilder::build()` rejects configs that cannot meet this floor.

The `tst-hls` publisher segments your stream to `.ts` files on disk and
serves them (plus a rolling `.m3u8`) over a built-in HTTP server. Each
segment opens on a decodable boundary (PAT → PMT → IDR), so a joining player
can decode the first segment it fetches. KLV metadata stays inside the
segments — STANAG 4609-aware players continue to decode telemetry.

See the [HLS guide](/docs/guides/hls.md) for serving guidance, the KLV
ride-along carriage modes, and latency tuning. For the browser + hls.js
client contract, see
[Send KLV over HLS to a browser (hls.js)](/docs/cookbook/sending/hls-klv-to-web.md).

## Code

```rust
use std::time::Duration;
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

let publisher = HlsPublisherBuilder::new()
    // Default bind is 127.0.0.1:8080 (loopback). Bind all interfaces only
    // behind a reverse proxy, or with auth + TLS.
    .output_dir("/var/cache/hls")
    .segment_duration(Duration::from_secs(4))
    // Force-cut cap for an overdue keyframe (stalled / very long GOP).
    // Defaults to 2 × segment_duration; must be ≥ segment_duration.
    .max_segment_duration(Duration::from_secs(8))
    .playlist_window(6)
    .mode(HlsMode::Live)
    .build()?;
println!("serving http://{}/playlist.m3u8", publisher.local_addr().unwrap());

let mut shell = MuxPublisher::with_config(publisher, mux_config)?;

// send_video with key_frame=true automatically cuts a new segment.
shell.send_video(&nal_bytes, pts, key_frame)?;
shell.send_klv(&klv_bytes, pts, 0)?;

// Optional Basic auth: .basic_auth("user", "secret") on the builder.
// Optional HTTPS:      .enable_tls("server.crt", "server.key") on the builder.

let publisher = shell.finish()?;
publisher.finish()?;  // writes final playlist + #EXT-X-ENDLIST (Event/Vod modes)
```

### Java

```java
HlsPublisher pub = HlsPublisher.builder().outputDir("/var/cache/hls")
    .segmentDurationMs(4000).maxSegmentDurationMs(8000).playlistWindow(6).mode(HlsMode.LIVE).build();

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x100)
    .addVideo(0x101, VideoCodec.H264)
    .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true)
    .build();

// withConfigHls, sendVideo, sendKlv, sendAudio, sendSubtitle and cutSegment
// declare checked MuxException and HlsException; finishIntoPublisher and
// finish declare HlsException — catch both in real code.
MuxPublisher shell = MuxPublisher.withConfigHls(pub, program); // consumes `pub`

shell.sendVideo(nalBytes, pts, keyFrame);  // keyFrame=true cuts a new segment
shell.sendKlv(klvBytes, pts, 0);

HlsPublisher back = shell.finishIntoPublisher();
back.finish();  // writes final playlist + #EXT-X-ENDLIST (Event/Vod modes)
```

## Keep a completed VOD / EVENT playlist watchable

`Publisher::finish` writes the terminal playlist and tears the server down —
so a VOD stops being fetchable the moment it ends. Use `finish_serving`
instead to finalize the playlist but keep the built-in server up, returning
an `HlsServerHandle`:

```rust
let publisher = shell.finish()?;
let handle = publisher.finish_serving()?;   // #EXT-X-ENDLIST written; server stays up
println!("VOD at http://{}/playlist.m3u8", handle.local_addr());
// ... hold the handle for as long as clients should be able to fetch ...
handle.shutdown();
```

### Java

```java
HlsPublisher back = shell.finishIntoPublisher();
HlsServerHandle handle = back.finishServing();  // #EXT-X-ENDLIST written; server stays up
System.out.println("VOD at http://" + handle.localAddr() + "/playlist.m3u8");
// ... hold the handle for as long as clients should be able to fetch ...
handle.shutdown();
```

`HlsStats.forced_cuts` (from `publisher.hls_stats()`) counts how often
`max_segment_duration` force-cut an overdue keyframe — a persistently non-zero
value means your GOP cadence and `segment_duration` are mismatched.

## Verify with ffmpeg / VLC / mpv

```bash
ffplay 'http://localhost:8080/playlist.m3u8'
vlc    'http://localhost:8080/playlist.m3u8'
mpv    'http://localhost:8080/playlist.m3u8'
```

## Modes

| Mode | Playlist behavior | ENDLIST written? | Disk eviction? |
|---|---|---|---|
| `HlsMode::Live` | Rolling window (`playlist_window` newest segments visible) | Never | Yes — segments rolled out of window are deleted |
| `HlsMode::Event` | Monotone-growing | On `finish()` | No |
| `HlsMode::Vod` | Same as Event | On `finish()` | No |

## URL parameters (when constructed via `from_url`)

| Parameter | Default | Meaning |
|---|---|---|
| `output_dir` | `<temp_dir>/tstrans-hls` | Filesystem dir for `.ts` + `.m3u8` (portable temp dir) |
| `segment_duration` | `4` (seconds) | Target segment duration (keyframe-aligned cuts aim for this; `max_segment_duration` is the hard force-cut cap) |
| `playlist_window` | `6` | Rolling window size (LIVE mode only) |
| `mode` | `live` | One of `live`/`event`/`vod` |
| `auth_user`/`auth_pass` | none | Basic auth credentials |
| `cert`/`key` | none | HTTPS cert + key paths (PEM) |
