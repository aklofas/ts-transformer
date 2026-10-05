# What ts-transformer is

> **Who this is for:** You've landed on the project and want to know in five minutes what it does, what it doesn't do, and whether to keep reading.

> **You will learn:**
> - What ts-transformer does, and where its scope ends
> - The three places this library sits in a system: source, middle, display
> - What protocols and metadata standards are involved (in plain terms)
> - What's in the box today (Rust + C + Python + JVM)
> - The scope boundaries, and which companion tool covers each
> - Where to read next based on what you're doing

## What it does

ts-transformer is an MPEG-TS + KLV toolkit. It **builds** transport streams
(mux encoded video, audio, subtitles, and typed MISB KLV into conformant
MPEG-TS), **reads** them (demux any TS byte source into typed events), and
**transforms** them (repack programs, pair KLV to frames, re-serve) — offline
against files, or live over a network.

Live is where it earns its keep: **video and metadata over an unreliable
network**. The classic shape:

> A camera on a sensor platform — a drone, an aircraft, a helicopter with an EO/IR turret, a ground vehicle, or a fixed installation — encodes H.264 or H.265 video. Alongside each frame, the platform's sensors emit telemetry: latitude, longitude, altitude, heading, sensor pointing angles. All of it gets multiplexed into one byte stream, encrypted, and pushed across a flaky network — satellite, cellular, mesh radio — to a ground station or cloud ingest. On the other end, a viewer or a processing pipeline pulls it apart: video to a decoder, metadata to a database, both stamped with the same timestamp so they line up on a map.

That's the shape. ts-transformer handles the metadata multiplexing, the wire format, the reconnect, and the encryption. **Your code is the camera, the metadata source, the viewer, or the processing pipeline.** The library is the plumbing between them.

## Where it sits

The library lives in one of three places in your system:

- **At the source** — you're producing the stream. Encoder on a drone, a gimbal platform, a ground encoder. You feed in encoded video access units + typed KLV records; the library mux's them into MPEG-TS and sends over SRT.
- **In the middle** — you're processing, indexing, or relaying. Cloud ingest that extracts KLV into a database, a service that transcodes for HLS, a relay that re-broadcasts to multiple consumers, an archive that records to disk.
- **At the display** — you're showing the stream to a human. Desktop player, mobile viewer, situational-awareness UI that overlays the KLV-derived lat/lon on a map.

Each placement uses the same primitives differently. The [`guides/`](/docs/guides/) describe the primitives; the [cookbook](/docs/cookbook/index.md) shows the placement-specific recipes.

## What's in the box

- **MPEG-TS mux + demux** — the Rust core (`tst-core`, `tst-pipeline`, `tst-srt`). Single-program or multi-program TS; auto-PCR insertion; PAT/PMT generation; the full ST 1402 KLV-in-TS multiplexing pipeline.
- **Transports** — SRT (vendored libsrt 1.5.7; mbedTLS 3.6.x LTS encryption ON by default with AES-128/192/256), RTP (incl. RTSP client + server), raw TCP / TLS, UDP, and RIST (VideoLAN librist). A supported HLS publisher (segmenter + optional built-in HTTP server) ships too — see the [HLS guide](/docs/guides/hls.md).
- **MISB KLV** — typed encode + decode for ST 0601 (Full Motion Video FMV), ST 0102 (Security Metadata), ST 0605 (Precision Time Stamp), ST 0903 (VMTI per-target detections), ST 0806 (RVT), ST 1010 (SDCC error covariance); plus a one-way ST 0805 KLV→Cursor-on-Target conversion layer. H.222.0 §2.12.4.2 Metadata AU cell wrapping for synchronous KLV streams.
- **Video codecs** — H.264, H.265, H.266/VVC, AV1. NAL/OBU parsers; SPS/PPS/VPS extraction; slice-header-light parsers for resolution + profile.
- **Audio codecs** — AAC (ADTS full; LATM carriage + sync validation, full decode deferred), MPEG-2 Audio (MP2/MP3), AC-3. Frame-level parsers expose sample rate / channel count.
- **Subtitles** — DVB subtitling, DVB teletext, CEA-708, WebVTT-in-TS.
- **C bindings** (`tst-c`) — `cdylib` + `staticlib`, `tstrans.h` via cbindgen, `tstrans.pc` for pkg-config. Stable ABI versioned `TST_ABI_VERSION_MAJOR/MINOR`.
- **Python bindings** (`tst-py`, on PyPI as `tstrans`) — offline `.ts` inspection/construction plus live UDP / TCP / RTP (incl. RTSP) / SRT / RIST and the HLS publisher; typed KLV decode/encode; raw-first `DemuxEvent.Video` / `DemuxEvent.Audio` (each carries the raw access-unit / frame bytes); optional pandas + NumPy adapters.
- **JVM bindings** — `tst-jni`, distributed as `tstrans-jvm` (`org.tstrans`) on Maven Central. Mirrors the Python surface package for package for the offline, RTP and SRT parts (`org.tstrans.{io,codec,klv,mpegts,rtp,srt,pipeline}`); UDP, TCP, RIST and HLS have no JVM binding.

## Scope boundaries — and what to pair it with

ts-transformer is deliberately narrow: MPEG-TS as the container, MISB KLV as
the metadata language, the wire format as the job. Each boundary has a
well-worn companion tool:

- **Container: MPEG-TS.** Single- and multi-program; the HLS publisher
  segments MPEG-TS natively. For MP4 / MKV / fMP4 / DASH delivery, repackage
  downstream with FFmpeg or GStreamer.
- **Transports: UDP, raw TCP / TLS, RTP (incl. RTSP client + server), SRT 1.5
  (Haivision libsrt, vendored), RIST (VideoLAN librist), and the HLS publisher**
  (segmenter + optional built-in HTTP server — see the
  [HLS guide](/docs/guides/hls.md)). RTMP and WebRTC aren't planned — front
  with a media server (e.g. MediaMTX) where you need those endpoints.
- **Metadata: MISB KLV** (ST 0601 / 0102 / 0605 / 0903 / 1204 / 0806 / 1010),
  plus a ST 0805 KLV→CoT conversion layer. Other schemas
  ride as opaque private data; see
  [`project/deferred-features.md`](/docs/project/deferred-features.md).
- **Codec work stays outside.** Bring encoded NAL units / OBU frames from
  x264 / x265 / FFmpeg / NVENC / GStreamer; on the display side, pair with
  PyAV / FFmpeg / a hardware decoder.
- **Display stays outside.** Any MPEG-TS-capable player works: VLC, mpv,
  MPV.js, or your own decoder + UI.

For the full feature-by-feature support matrix, see [`reference/compatibility.md`](/docs/reference/compatibility.md). For things deferred with a rationale + revisit trigger, see [`project/deferred-features.md`](/docs/project/deferred-features.md).

## What to read next

- **Brand new to MPEG-TS / KLV / SRT?** → [`start/concepts.md`](/docs/start/concepts.md) explains the domain vocabulary.
- **Ready to write code?** → [`start/quickstart.md`](/docs/start/quickstart.md).
- **Picking a language?** → [Language decision table](/docs/index.md#which-language-should-i-pick) on the landing page.
- **Want a deep dive?** → [`guides/`](/docs/guides/) — one per topic (mpegts-mux, mpegts-demux, srt, klv, codec, pipeline).
- **Need a code recipe?** → [Cookbook](/docs/cookbook/index.md).
