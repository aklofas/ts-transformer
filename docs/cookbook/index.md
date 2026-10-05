# Cookbook

> **Who this is for:** You've read the guides and want to crib working code for a specific task. Most recipes are self-contained Rust snippets (or example pointers); a few binding-specific recipes (e.g. Python + PyAV, noted inline) cover tasks that are inherently language-specific. Paste them into your project.

> **You will learn:**
> - Where to find a recipe by topic
> - How recipes group by reader intent (muxing, sending, receiving, pairing, KLV, codecs, operations)
> - Where to find the full runnable example for each recipe (where one exists)

Recipes are task-named — find them by scanning the section that matches what you're doing. Within each section, recipes run simple → complex. Run any example with `cargo run -p tst-examples --example <name>`.

## 🧱 Muxing — build a TS, no network

Start here if your job is producing MPEG-TS bytes: files, fixtures, multi-stream programs.

- **[Mux to a file (no SRT, no transport)](/docs/cookbook/muxing/mux-to-file.md)** — The muxer's output without any networking — test fixtures, TSDuck/ffprobe validation, offline pipelines.
- **[Mux H.265 + sync KLV](/docs/cookbook/muxing/mux-h265-with-klv.md)** — The encoder produces HEVC, or the receiver requires strict ST 1402 sync metadata (PMT stream_type 0x15).
- **[Mux H.266 / VVC video with synchronous KLV](/docs/cookbook/muxing/mux-h266-with-klv.md)** — The encoder produces H.266 (VVC) and the receiver requires strict ST 1402 sync KLV.
- **[Mux AV1 video with KLV](/docs/cookbook/muxing/mux-av1-with-klv.md)** — The encoder produces AV1 — OBU framing replaces Annex-B NAL framing.
- **[Mux audio + video + KLV in a single program](/docs/cookbook/muxing/mux-audio-video-klv.md)** — A three-stream program where audio PTS-aligns with video and KLV rides the same PCR clock.
- **[Label EO + IR + KLV streams in a multi-stream program](/docs/cookbook/muxing/mux-eo-ir-klv.md)** — Per-stream PMT descriptors let receivers (TSDuck, ffprobe, our `Demuxer`) render which PID is which.
- **[Repack two single-program inputs into one multi-program TS](/docs/cookbook/muxing/repack-multi-program.md)** — Ship two independent feeds through one socket without forcing each onto its own port.

## 🛰 Sending — put a TS on the wire

- **[Send a single TS packet to any `Transport`](/docs/cookbook/sending/send-single-packet.md)** — The simplest possible sender — open a transport, push 188 bytes, drop.
- **[Open a sender from an `srt://...?...` URL](/docs/cookbook/sending/sender-from-url.md)** — Connection target and tuning live in deployment config or an orchestrator.
- **[Send video + KLV with passphrase encryption](/docs/cookbook/sending/send-encrypted.md)** — A secure SRT uplink with passphrase-derived AES-CTR encryption negotiated at handshake.
- **[Send MPEG-TS over UDP](/docs/cookbook/sending/udp.md)** — Raw UDP unicast/multicast — lowest-common-denominator for ffmpeg, VLC, and STANAG 4609 receivers.
- **[Send MPEG-TS over TCP](/docs/cookbook/sending/tcp.md)** — Reliable-bytestream sibling of UDP, optional TLS via rustls.
- **[Send MPEG-TS over RIST](/docs/cookbook/sending/rist.md)** — ARQ-recovered UDP; Simple + Main profiles; AES PSK encryption.
- **[Publish MPEG-TS as an HLS stream](/docs/cookbook/sending/hls.md)** — `.ts` segments + rolling `.m3u8`; LIVE/EVENT/VOD + `finish_serving`; optional built-in HTTP server.
- **[Send KLV over HLS to a browser (hls.js)](/docs/cookbook/sending/hls-klv-to-web.md)** — MISB KLV inside HLS segments; hls.js pulls it via the native `misbklv` path or a UL-anchored fallback.
- **[Relay a captured `.ts` file over SRT](/docs/cookbook/sending/relay-file-to-srt.md)** — Replay a capture: regression-testing receivers, rebroadcasting an archive.
- **[Use a custom (non-SRT) transport](/docs/cookbook/sending/custom-transport.md)** — The sender shells fit but the wire is your own protocol or an in-memory harness.

## 📡 Receiving — take a TS off the wire

- **[Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md)** — Ingest from ffmpeg, VLC, or STANAG 4609 senders; unicast or multicast.
- **[Receive MPEG-TS over TCP](/docs/cookbook/receiving/tcp.md)** — Accept an inbound caller (listener) or connect to a producer; TLS via `tcps://`.
- **[Receive MPEG-TS over RIST](/docs/cookbook/receiving/rist.md)** — Bind URL form (`rist://@host:port`) with optional AES PSK decryption.
- **[Receive into a file](/docs/cookbook/receiving/receive-to-file.md)** — Archive a stream or build a test fixture from a live producer.
- **[Ingest H.264 from an RTSP camera and remux to MPEG-TS](/docs/cookbook/receiving/recv-rtsp-h264-to-ts.md)** — Bare H.264-over-RTP (RFC 6184) gateway pattern. Python-first; Rust example twin.
- **[Extract subtitle PES bytes from a captured `.ts` file](/docs/cookbook/receiving/extract-subtitle-pes.md)** — Discover subtitle codecs in a capture and read the cue text.

## 🔗 Pairing — align KLV with video frames

Receiving a stream and want video + telemetry as one record? This is the section.

- **[Pair sync-KLV with video AUs by nearest PTS](/docs/cookbook/pairing/pair-klv-by-pts.md)** — One KLV per frame with KLV PES PTS = frame PTS; consume frame + telemetry paired.
- **[Sample-and-hold async-KLV against video frames](/docs/cookbook/pairing/sample-hold-klv.md)** — 1–10 Hz async metadata against 25–60 fps video.
- **[EO + IR sensor pair with shared async-KLV](/docs/cookbook/pairing/eo-ir-shared-klv.md)** — Two sensors, one async metadata stream serving both.
- **[Pair sync-KLV with video AUs via `Pairer::with_config` (Realtime)](/docs/cookbook/pairing/pairer-realtime.md)** — The inline PTS pattern expressed through the opt-in `Pairer` helper, with bounded history and telemetry counters.
- **[Pair sync-KLV in batch mode (`PairerMode::Buffered`)](/docs/cookbook/pairing/pairer-batch.md)** — KLV PES interleaves *after* its matching video PES and Realtime mode misses the pairing.
- **[Sample-and-hold async KLV via `Pairer::last_before_pts`](/docs/cookbook/pairing/pairer-last-before-pts.md)** — Attach the most recent KLV at `klv.pts <= video.pts` to each frame.
- **[EO + IR composition with shared async-KLV](/docs/cookbook/pairing/eo-ir-shared-klv-pairer.md)** — Two video PIDs share one async-KLV PID, with typed projections per branch.

## 🔑 KLV — encode and decode metadata directly

- **[Decode ST 0601 from a captured `.klv` blob](/docs/cookbook/klv/decode-st0601-blob.md)** — Validate producer output, build dashboards, debug a receiver.
- **[Encode ST 0601 from typed values](/docs/cookbook/klv/encode-st0601.md)** — Synthesize KLV for tests, fixtures, or gateway translation.
- **[Decode security metadata from an ST 0601 record](/docs/cookbook/klv/decode-security-metadata.md)** — Tag 48 → ST 0102 classification, country codes, version info.
- **[Decode VMTI per-target detections from an ST 0601 stream](/docs/cookbook/klv/decode-vmti.md)** — Surface detected/tracked targets per frame via Tag 74.
- **[Reading waypoint lists, weapons stores, and SDCC covariance](/docs/cookbook/klv/decode-long-tail.md)** — Typed access to the ST 0601 long tail's repeated-record and cross-referenced-pack items.
- **[Converting ST 0601 to Cursor-on-Target](/docs/cookbook/klv/klv-to-cot.md)** — MISB ST 0805.1 KLV→CoT for ATAK/WinTAK/other TAK-family consumers.
- **[Build a STANAG 4609-conformant stream](/docs/cookbook/klv/stanag-4609-stream.md)** — ST 0604 MISP timestamps + ST 0902 MISMMS gate + strict-compliance encode + Tag 94 Core ID.

## 🎞 Codecs — parse video and audio elementary streams

- **[Extract video resolution and profile from a demuxed stream](/docs/cookbook/codecs/extract-resolution-profile.md)** — Typed codec info (width, height, profile, level, frame rate, color) while demuxing.
- **[Reconstitute Annex B parameter sets for decoder replay](/docs/cookbook/codecs/reconstitute-annex-b.md)** — Hand SPS/PPS bytes to a hardware decoder or encoder re-init.
- **[Pull sample rate and channel count out of an audio stream](/docs/cookbook/codecs/extract-audio-format.md)** — Typed audio metadata (sample rate, channels, codec layer/profile) per audio PID.
- **[Decode video frames in-memory with PyAV (Python)](/docs/cookbook/codecs/decode-frames-pyav.md)** — Decode frames straight from `DemuxEvent.Video.raw` in the same pass that yields your KLV.

## ⚙ Operations — lifecycle, stats, shutdown, fixtures

- **[Survive a flaky transport with reconnect + gap buffer](/docs/cookbook/operations/managed-transport-reconnect.md)** — Radio links, NAT timeouts, listener restarts.
- **[Print live `Stats` from a sender](/docs/cookbook/operations/print-live-stats.md)** — Dashboards, production telemetry, field packet-loss debugging.
- **[Graceful shutdown from another thread via `SrtCancelHandle`](/docs/cookbook/operations/graceful-shutdown.md)** — Wake a thread parked in `send_*` / `recv_*` from a signal handler or watchdog.
- **[Inject WebVTT POI cues into a live MPEG-TS uplink](/docs/cookbook/operations/inject-webvtt-cues.md)** — Mark Points of Interest so a downstream HLS player renders captions.
- **[Capture a regression fixture from a corpus `.ts` file](/docs/cookbook/operations/capture-regression-fixture.md)** — Preserve a minimal reproducer as a committed regression test.

## Browse by example program

Only examples explicitly invoked via `cargo run -p tst-examples --example <name>` in a recipe body are listed here. Examples referenced as runnable file paths appear in each recipe's "Related" header — browse the [examples/](/examples/) tree for the full catalog.

| Example | Recipe |
|---|---|
| `decode_vmti_metadata` | [Decode VMTI per-target detections](/docs/cookbook/klv/decode-vmti.md) |
| `demux_subtitle_file` | [Extract subtitle PES bytes](/docs/cookbook/receiving/extract-subtitle-pes.md) |
| `mux_dual_camera` | [Label EO + IR + KLV streams](/docs/cookbook/muxing/mux-eo-ir-klv.md) |
| `mux_with_webvtt_subtitles` | [Inject WebVTT POI cues](/docs/cookbook/operations/inject-webvtt-cues.md) |
| `pair_klv_pipeline` | [Pair sync-KLV via `Pairer` (Realtime)](/docs/cookbook/pairing/pairer-realtime.md) |
| `parse_audio_frames` | [Pull sample rate and channel count](/docs/cookbook/codecs/extract-audio-format.md) |
| `recv_rtsp_h264` | [Ingest H.264 from an RTSP camera](/docs/cookbook/receiving/recv-rtsp-h264-to-ts.md) |
| `send_tcp` | [Send MPEG-TS over TCP](/docs/cookbook/sending/tcp.md) |
