# MPEG-TS, KLV, and SRT in plain terms

> **Who this is for:** You're new to MPEG-TS / KLV / SRT and want to understand them well enough to read the rest of these docs — and to talk about your design — without getting lost.

> **You will learn:**
> - How MPEG-TS carries video, audio, and metadata together
> - What a PID, PES packet, and access unit are
> - What KLV is (the metadata format) and how it gets carried alongside video
> - How SRT handles packet loss on a live connection
> - The glossary you'll hit in the API: PAT, PMT, PCR, DTS, PTS, IDR, GOP, SMPTE UL, BER

This page is conceptual. There's no code here. For code, see [`quickstart.md`](/docs/start/quickstart.md). For deep dives, see [`guides/`](/docs/guides/).

---

## MPEG-TS, the container

**MPEG-TS** (MPEG-2 Transport Stream) is the byte format used to multiplex live video, audio, and metadata into one continuous stream that can survive packet loss and let a viewer tune in mid-stream.

Imagine joining a TV broadcast that's already running. The receiver first
has to find packet boundaries and learn which packets carry video, audio,
and metadata. MPEG-TS uses fixed **188-byte packets**, with an identifier in
each packet's header, and repeats tables describing the streams. Finding
these boundaries lets the receiver start extracting data. Displaying video
also requires the decoder's setup data and a suitable random-access frame.

MPEG-TS is used in digital television, IPTV, and sensor video links. Its
packet structure helps a receiver recover alignment after a gap, but the
container itself does not recover lost data. That is a separate job for
the transport protocol.

### PIDs, programs, and the PAT/PMT/PCR ladder

Inside one MPEG-TS byte stream, multiple **elementary streams** (one video,
one audio, one KLV metadata track, etc.) are interleaved. A **PID** (Packet
Identifier) is the 13-bit number used to route packets. For example, a
receiver might route PID 0x100 to a video decoder and PID 0x101 to a KLV
parser. Some PIDs are reserved, so the full range 0x0000–0x1FFF is not
available for assigning media streams.

A **program** groups related streams, such as a camera's video, audio, and
telemetry. One MPEG-TS byte stream can contain several programs.

But PIDs alone don't tell you *which* PID carries which stream. That's where the table ladder comes in:

- **PAT** (Program Association Table) — always on PID 0x0000. Tells you "Program 1 is described by PMT on PID 0x1000; Program 2 is described by PMT on PID 0x1010" (and so on).
- **PMT** (Program Map Table) — one per program. Tells you "for Program 1: video is PID 0x101 (H.264), audio is PID 0x102 (AAC), KLV is PID 0x103 (sync metadata)."
- **PCR** (Program Clock Reference) — periodic 27 MHz timestamps embedded in the stream. The receiver uses PCR to keep its internal clock locked to the sender's clock, which is what lets the audio and video play in sync without drift.

You usually don't think about this ladder directly — ts-transformer's `Muxer` builds the PAT and PMT automatically from a `MuxerConfig`; the `Demuxer` parses them automatically. But when you see "PID" in the API, this is what it means.

### PES, access units, presentation time

Media payloads are grouped into **PES** (Packetized Elementary Stream)
packets, which are then split across TS packets. A PES header can carry
timestamps for its payload. In ts-transformer's muxer, one video push
becomes one PES packet, which may span many TS packets.

An **access unit (AU)** is encoded data for one presentation unit. For
H.264 video, it represents one picture (a frame or field) and can contain
several NAL units. An AU is not necessarily independently decodable: it may
refer to earlier pictures. Pass the whole AU to `push_video`, rather than
calling it separately for each NAL unit.

**Presentation timestamps:**

- **PTS** (Presentation Time Stamp) — when this AU should be displayed (rendered).
- **DTS** (Decode Time Stamp) — when this AU should be decoded. Differs from PTS for video codecs with B-frames (frames decoded out of display order).

Both PTS and DTS live in the PES header, at 90 kHz tick resolution (so 90,000 ticks = 1 second). PTS lets the decoder play video and audio in sync. PCR (above) keeps the decoder's clock locked to the sender's clock so PTS comparisons are meaningful.

For video, the data flows like this:

```text
encoder → encoded access unit → muxer → PES split across TS packets
decoder ← encoded access unit ← demuxer ← TS packets
```

ts-transformer handles muxing and demuxing. Your encoder and decoder handle
the conversion between encoded access units and pictures.

---

## KLV, the metadata format

**KLV** stands for **Key-Length-Value**. It's a binary, self-describing data encoding where every field carries:

- A **Key** — a unique identifier saying what this field is.
- A **Length** — how many bytes the value occupies.
- A **Value** — the actual bytes.

A parser that doesn't recognize a key can read the length, skip ahead exactly that many bytes, and keep parsing the next field. This makes KLV **forward-compatible**: a sender can add new fields without breaking older receivers, because the older receivers can simply skip what they don't understand.

Worked example. An aircraft emits per-frame telemetry: latitude, longitude, altitude, heading, sensor pointing angles, mission ID, timestamp. Each field is one KLV record, all bundled together into one **set** (a KLV record that contains other KLV records). Today's set has 14 fields. Six months later, the platform adds a new field for "wind speed at altitude." Old receivers parse the 14 known fields and silently skip the wind-speed bytes. New receivers parse all 15. No version negotiation, no schema break.

### Keys: SMPTE Universal Labels

A top-level MISB KLV record starts with a **SMPTE Universal Label (UL)**, a
16-byte identifier that tells the reader what kind of record follows.
Inside a **local set**, fields use compact numeric tags instead of
repeating a 16-byte label. For example, ST 0601 uses Tag 2 for its timestamp.
The outer UL identifies the set; the set's standard defines its inner tags.

The military / ISR community settled on the **MISB** (Motion Imagery Standards Board) standards as the authoritative key set:

- **MISB ST 0601** — Aircraft platform position, sensor pointing, timing, and mission context.
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

SRT can request retransmission of missing packets. In live mode, the
receiver waits within a configured latency budget; packets that arrive too
late can be dropped. This trades some delay for a better chance of recovering
loss. It does not guarantee uninterrupted pictures: a gap can still affect
decoding until the next suitable frame.

Compare:

- **Plain UDP** sends datagrams without recovering losses or restoring their order.
- **TCP** provides an ordered byte stream. Missing bytes delay delivery of
  later bytes while retransmission is attempted.
- **SRT** sends messages over UDP and adds retransmission and a configurable
  delivery delay for live media.
- **RIST** also uses UDP with retransmission, but is a different protocol.
  An SRT sender needs an SRT receiver; it cannot connect directly to a RIST receiver.

### Latency budget, encryption, reconnect

**Latency.** SRT trades latency for reliability. You configure how long the receiver waits for missing packets before giving up and playing forward. Default ~120 ms; tune up (seconds) for satellite, tune down (tens of ms) for low-latency local links. ts-transformer's `SocketBuilder` exposes this as `latency_ms`.

**Encryption.** SRT supports AES-128, AES-192, and AES-256. Encryption
support is built into `tst-srt` by default, but a connection is encrypted
only when you configure a passphrase. Set the same passphrase on both peers;
in Rust, use `SocketBuilder::passphrase(Passphrase::new("…")?)` and the
corresponding listener setter. See the
[encrypted-send recipe](/docs/cookbook/sending/send-encrypted.md).

**Reconnect.** A broken SRT connection must be replaced with a new one.
ts-transformer's managed wrappers can retry automatically with a configurable
delay between attempts. On the receive side, `ManagedDemuxReceiver` also
emits a discontinuity event after reconnect so your application can handle
the gap. See the [pipeline guide](/docs/guides/pipeline.md).

---

## Glossary

Compact reference of terms you'll hit in the API and in these docs. Each links to its deeper home.

| Term | Plain meaning |
|---|---|
| **PID** | Packet identifier — the channel number inside an MPEG-TS stream. ([guide](/docs/guides/mpegts-mux.md)) |
| **PAT / PMT** | Tables that map programs to PIDs. Auto-generated by `Muxer`. |
| **PCR** | Periodic 27 MHz clock reference. Keeps receiver clock locked to sender clock. |
| **PES** | Per-elementary-stream packetization layer inside TS. |
| **AU** | Access unit — encoded data for one presentation unit, such as a video picture; it may depend on other pictures. |
| **PTS / DTS** | Presentation / Decode time stamps. 90 kHz ticks in the PES header. |
| **NAL unit** | Network Abstraction Layer unit — H.264/H.265/H.266 elementary stream unit. ([guide](/docs/guides/codec.md)) |
| **OBU** | Open Bitstream Unit — AV1's equivalent of a NAL unit. |
| **I-frame / IDR** | An I-frame uses no other picture to decode itself. An IDR (Instantaneous Decoder Refresh) also prevents later pictures from referring to pictures before it, making it a useful place to join a stream. |
| **GOP** | Group of Pictures — a sequence of coded pictures organized around an intra-coded picture; its structure affects compression and where playback can start. |
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
