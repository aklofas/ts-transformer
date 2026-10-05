# MPEG-TS, KLV, and SRT in plain terms

> **Who this is for:** You're new to MPEG-TS / KLV / SRT and want to understand them well enough to read the rest of these docs — and to talk about your design — without getting lost.

> **You will learn:**
> - What MPEG-TS is and why people keep using a 1990s container
> - What a PID, PES packet, and access unit are
> - What KLV is (the metadata format) and how it gets carried alongside video
> - What SRT is, and why pick it over RTMP / RTP / TCP
> - The glossary you'll hit in the API: PAT, PMT, PCR, DTS, PTS, IDR, GOP, SMPTE UL, BER

This page is conceptual. There's no code here. For code, see [`quickstart.md`](/docs/start/quickstart.md). For deep dives, see [`guides/`](/docs/guides/).

---

## MPEG-TS, the container

**MPEG-TS** (MPEG-2 Transport Stream) is the byte format used to multiplex live video, audio, and metadata into one continuous stream that can survive packet loss and let a viewer tune in mid-stream.

Imagine a satellite TV channel that broadcasts 24/7. There's no "restart" — when you turn on your receiver, the stream is already in flight. Your receiver has to be able to start decoding from whatever byte it lands on. MPEG-TS solves this by chopping the byte stream into uniform **188-byte packets**, each one self-routing, each one carrying a small slice of one elementary stream (video frame, audio frame, or metadata chunk). The receiver scans for sync bytes, locks on, and starts decoding the next complete frame.

Designed in the 1990s, MPEG-TS is everywhere streaming live media goes: digital television (DVB, ATSC), satellite, military / ISR video links, IPTV head-ends. It's the lingua franca of "live linear video over an unreliable channel." Newer container formats (MP4, fMP4, CMAF) handle file-based and on-demand streaming better, but for live + lossy, TS is still the right tool.

### PIDs, programs, and the PAT/PMT/PCR ladder

Inside one MPEG-TS byte stream, multiple **elementary streams** (one video, one audio, one KLV metadata track, etc.) are interleaved. Each elementary stream gets a **PID** (Packet IDentifier — a 13-bit channel number, 0x0000–0x1FFF). The receiver routes incoming packets by PID: PID 0x100 → video decoder, PID 0x101 → audio decoder, PID 0x1031 → KLV parser.

But PIDs alone don't tell you *which* PID carries which stream. That's where the table ladder comes in:

- **PAT** (Program Association Table) — always on PID 0x0000. Tells you "Program 1 is described by PMT on PID 0x1000; Program 2 is described by PMT on PID 0x1010" (and so on).
- **PMT** (Program Map Table) — one per program. Tells you "for Program 1: video is PID 0x101 (H.264), audio is PID 0x102 (AAC), KLV is PID 0x103 (sync metadata)."
- **PCR** (Program Clock Reference) — periodic 27 MHz timestamps embedded in the stream. The receiver uses PCR to keep its internal clock locked to the sender's clock, which is what lets the audio and video play in sync without drift.

You usually don't think about this ladder directly — ts-transformer's `Muxer` builds the PAT and PMT automatically from a `MuxerConfig`; the `Demuxer` parses them automatically. But when you see "PID" in the API, this is what it means.

### PES, access units, presentation time

Inside the 188-byte TS packet layer is another packetization layer: **PES** (Packetized Elementary Stream). PES packets are the unit *one elementary stream's* data is chunked into before being sliced across TS packets. A single H.264 frame might span dozens of TS packets but is one PES packet. The PES header carries the per-frame metadata: timestamps, stream IDs.

**Access unit (AU)** = the smallest decodable unit of an elementary stream. For H.264 video that's one decoded picture (one frame, or one field for interlaced). For audio it's one decoded sample frame. For KLV sync metadata it's one complete KLV record.

**Presentation timestamps:**

- **PTS** (Presentation Time Stamp) — when this AU should be displayed (rendered).
- **DTS** (Decode Time Stamp) — when this AU should be decoded. Differs from PTS for video codecs with B-frames (frames decoded out of display order).

Both PTS and DTS live in the PES header, at 90 kHz tick resolution (so 90,000 ticks = 1 second). PTS lets the decoder play video and audio in sync. PCR (above) keeps the decoder's clock locked to the sender's clock so PTS comparisons are meaningful.

The shorthand: **TS packet → PES packet → access unit → frame.** ts-transformer's API speaks at the access-unit level (`push_video(nal, pts, key_frame)`), so you mostly think in AUs. The library handles the PES + TS layering.

---

## KLV, the metadata format

**KLV** stands for **Key-Length-Value**. It's a binary, self-describing data encoding where every field carries:

- A **Key** — a unique identifier saying what this field is.
- A **Length** — how many bytes the value occupies.
- A **Value** — the actual bytes.

A parser that doesn't recognize a key can read the length, skip ahead exactly that many bytes, and keep parsing the next field. This makes KLV **forward-compatible**: a sender can add new fields without breaking older receivers, because the older receivers can simply skip what they don't understand.

Worked example. An aircraft emits per-frame telemetry: latitude, longitude, altitude, heading, sensor pointing angles, mission ID, timestamp. Each field is one KLV record, all bundled together into one **set** (a KLV record that contains other KLV records). Today's set has 14 fields. Six months later, the platform adds a new field for "wind speed at altitude." Old receivers parse the 14 known fields and silently skip the wind-speed bytes. New receivers parse all 15. No version negotiation, no schema break.

### Keys: SMPTE Universal Labels

A KLV key is a **SMPTE Universal Label (UL)** — a 16-byte unique identifier registered with SMPTE (the Society of Motion Picture and Television Engineers). The first bytes of every UL identify the registration authority; the rest disambiguate the specific field.

The military / ISR community settled on the **MISB** (Motion Imagery Standards Board) standards as the authoritative key set:

- **MISB ST 0601** — Full Motion Video (FMV) metadata. The big one. Defines ~140 keys for aircraft platform position + sensor pointing + mission context.
- **MISB ST 0102** — Security metadata (classification, releasability).
- **MISB ST 0605** — Precision Time Stamp (a time-status byte plus a microsecond timestamp, carried as its own KLV pack).
- **MISB ST 0903** — VMTI (Video Moving Target Indicator) — per-target detection bounding boxes inside the video.

ts-transformer ships typed Rust structs for all four sets — `UasDatalinkLs` (ST 0601), `SecurityLs` (ST 0102), `PrecisionTimeStampPack` (ST 0605), and `VmtiLs` (ST 0903): encode a typed record into KLV bytes, or decode KLV bytes into the typed struct. Sibling typed layers cover items nested inside an ST 0601 record too — ST 0806 (Remote Video Terminal, Tag 73) and ST 1010 (SDCC-FLP error covariance, Tag 102) — plus a one-way ST 0805 KLV→Cursor-on-Target conversion. See [`guides/klv.md`](/docs/guides/klv.md).

### Lengths: BER encoding

KLV uses **BER** (Basic Encoding Rules — borrowed from ASN.1) for the length field. Short form: lengths under 128 fit in one byte. Long form: longer lengths use a multi-byte encoding. The library handles this automatically.

### How KLV gets into MPEG-TS

A KLV record lives on its own PID inside the TS stream, alongside the video and audio PIDs. The KLV elementary stream is described in the PMT as either:

- **Synchronous Metadata** (`stream_type` 0x15) — KLV is bundled with PES packets that carry PTS, so it can be aligned to specific video frames. Per ITU-T H.222.0 §2.12.4.2, each KLV record gets wrapped in a 5-byte **Metadata AU cell** header before going into the PES payload. ts-transformer auto-wraps + auto-unwraps these for you.
- **Private Data** (`stream_type` 0x06) — KLV passes through as raw PES payload without the AU cell wrap. Less common but supported.

The full standard for KLV-in-TS is MISB **ST 1402** (multiplexing) + MISB **ST 1910** (lessons learned + best practices). You don't need to read these to use ts-transformer; the library implements them.

---

## SRT, the transport

**SRT** stands for **Secure Reliable Transport**. It's a UDP-based protocol designed for live media on unreliable networks. Originally developed by Haivision; published as an IETF draft (`draft-sharabayko-srt`); the reference implementation is the open-source `libsrt` C++ library that ts-transformer wraps.

Mental model: **TCP, but for video.** SRT recovers lost packets within a tunable latency budget, then ships forward. The video doesn't stall when the link recovers from a burst loss — the receiver plays through gaps if they exceed the budget, dropped packets become brief glitches rather than a long stall.

Compare:

- **Plain UDP** — fire and forget. Lost packets are lost. Video glitches build up.
- **TCP** — retransmit forever. A 1-second outage becomes a 30-second buffer-fill stall. Head-of-line blocks all subsequent packets behind any missed retransmission.
- **RTMP** — TCP-based, so same head-of-line problem. Industry-standard for non-live ingest, but bad on lossy links.
- **WebRTC** — better for two-way real-time (low latency), but heavyweight for one-way broadcast; depends on the browser ecosystem.
- **RIST** — similar design to SRT, also UDP-based with retransmission. Different protocol; not interoperable.
- **SRT** — UDP-based with selective retransmission inside a tunable latency budget. Perfect for satellite, cellular, mesh, mobile — anywhere bursty loss is the norm.

### Latency budget, encryption, reconnect

**Latency.** SRT trades latency for reliability. You configure how long the receiver waits for missing packets before giving up and playing forward. Default ~120 ms; tune up (seconds) for satellite, tune down (tens of ms) for low-latency local links. ts-transformer's `SocketBuilder` exposes this as `latency_ms`.

**Encryption.** SRT supports AES-128, AES-192, AES-256 with a shared passphrase. ts-transformer enables encryption **by default** — the vendored mbedTLS 3.6.x LTS is statically linked. You set a passphrase via `SocketBuilder::passphrase(Passphrase::new("…")?)` and both sender and receiver use the same one. Disable encryption with `--no-default-features` only if you have a reason.

**Reconnect.** Reconnection is **not** part of the SRT protocol itself — when a socket drops, it stays dropped. ts-transformer adds a `ManagedTransport` wrapper that automatically retries with exponential backoff and emits discontinuity events when a reconnect happens, so the receiver knows the stream had a gap. See [`guides/pipeline.md`](/docs/guides/pipeline.md).

---

## Glossary

Compact reference of terms you'll hit in the API and in these docs. Each links to its deeper home.

| Term | Plain meaning |
|---|---|
| **PID** | Packet identifier — the channel number inside an MPEG-TS stream. ([guide](/docs/guides/mpegts-mux.md)) |
| **PAT / PMT** | Tables that map programs to PIDs. Auto-generated by `Muxer`. |
| **PCR** | Periodic 27 MHz clock reference. Keeps receiver clock locked to sender clock. |
| **PES** | Per-elementary-stream packetization layer inside TS. |
| **AU** | Access Unit — smallest decodable unit of an elementary stream (one video frame, one audio frame, one KLV record). |
| **PTS / DTS** | Presentation / Decode time stamps. 90 kHz ticks in the PES header. |
| **NAL unit** | Network Abstraction Layer unit — H.264/H.265/H.266 elementary stream unit. ([guide](/docs/guides/codec.md)) |
| **OBU** | Open Bitstream Unit — AV1's equivalent of a NAL unit. |
| **IDR / I-frame** | Instantaneous Decoder Refresh — a video frame that decodes without referencing others. Stream cut-in points. |
| **GOP** | Group of Pictures — the chunk of frames between consecutive IDRs. |
| **KLV** | Key-Length-Value binary self-describing format. ([guide](/docs/guides/klv.md)) |
| **UL** | Universal Label — the 16-byte SMPTE-registered key prefix for KLV. |
| **BER** | Basic Encoding Rules — the length-prefix encoding KLV uses. |
| **MISB ST 0601** | Full Motion Video metadata standard. The KLV "main set" for ISR. |
| **MISB ST 0102** | Security metadata standard. |
| **MISB ST 0903** | VMTI — Video Moving Target Indicator metadata. |
| **MISB ST 1402** | KLV-in-MPEG-TS multiplexing standard. |
| **H.222.0 §2.12.4.2** | The ITU-T standard for the 5-byte Metadata AU cell header that wraps synchronous KLV. |
| **SRT** | Secure Reliable Transport. UDP-based, retransmission-within-latency-budget. ([guide](/docs/guides/srt.md)) |
| **libsrt** | The C++ reference implementation; ts-transformer vendors v1.5.7. |
| **mbedTLS** | Encryption library; ts-transformer vendors v3.6.x LTS. |
| **ADTS** | Audio Data Transport Stream — AAC's elementary stream framing format. |
| **LATM** | Low-overhead Audio Transport Multiplex — alternate AAC framing for TS. |

## What to read next

- [`quickstart.md`](/docs/start/quickstart.md) — write your first code (10 minutes).
- [`guides/mpegts-mux.md`](/docs/guides/mpegts-mux.md) — sender-side TS construction.
- [`guides/mpegts-demux.md`](/docs/guides/mpegts-demux.md) — receiver-side TS parsing.
- [`guides/klv.md`](/docs/guides/klv.md) — KLV encode + decode deep dive.
- [`guides/srt.md`](/docs/guides/srt.md) — SRT transport: latency, encryption, cancellation.
- [`guides/codec.md`](/docs/guides/codec.md) — video / audio elementary stream parsers.
- [`guides/pipeline.md`](/docs/guides/pipeline.md) — composing senders + receivers + reconnect wrappers.
