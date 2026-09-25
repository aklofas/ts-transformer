# MPEG-TS Demuxer Guide


> **Who this is for:** You're building the receiver / processing side — parsing a live MPEG-TS stream (or a `.ts` file) into typed `DemuxEvent` items.

> **You will learn:**
> - The bytes-in / events-out model: `Demuxer::push_packet` and `Demuxer::pop_event`
> - The four-tier `StrictMode` ladder (lenient → strict-by-spec)
> - The `DemuxEvent` variants and what payload each carries
> - How synchronous KLV gets auto-unwrapped from H.222.0 §2.12.4.2 AU cells
> - The opt-in `cfi_tolerance` mode and the producer-malformation pattern it handles
> - How `DemuxReceiver<T>` composes the demuxer with a `RecvTransport`
> - How `ManagedDemuxReceiver` adds automatic reconnect with discontinuity events

## Introduction

When you have an MPEG-TS byte stream — live off the wire or out of a `.ts`
file — and need typed events back (program maps, video access units, KLV
records, discontinuity markers, non-conformance diagnostics),
`tst_core::mpegts::demux` is the engine. `Demuxer` recovers TS packet
alignment, parses PSI (PAT / PMT), reassembles PES packets into raw video
access units, peels H.222.0 § 2.12.4.2 `Metadata_AU_cell` headers off
sync KLV, and emits a typed event stream — `DemuxEvent::ProgramMap`,
`Sample`, `Metadata`, `Discontinuity`, `NonConformant`. Bytes need not
be 188-aligned; the demuxer handles sync recovery internally.

This is the symmetric pair to [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md).
The muxer goes from typed inputs (NAL units + KLV blobs) to TS bytes;
the demuxer goes from TS bytes back to typed events. They share the
same vocabulary — `VideoCodec`, `KlvStreamType` ↔ `MetadataKind`, PSI
cadence — but the demuxer's contract is bigger because it has to cope
with the messy reality of real-world captures.

> **Python:** `tstrans` ships `py.typed` type stubs for the core `io`/`codec`/`klv`/`mpegts` modules, so editors and `mypy` resolve these types directly.

**Decoupled pairing.** The demuxer does **NOT** pair sync-KLV records
with video access units. Each KLV record and each video AU surfaces as
an independent stream-tagged event with full timing info; pairing
(nearest-PTS, sample-and-hold, multi-stream routing) is a consumer-domain
decision. See "Pairing is a consumer concern" below and the three
cookbook recipes (12, 13, 14) for the canonical patterns.

**Lenient by default.** Real-world ISR captures routinely omit
`metadata_descriptor`, mix sync/async stream types incorrectly, or jump
PCR. The demuxer tolerates all of these by default, surfacing them as
`DemuxEvent::NonConformant` events so the receive loop keeps running.
Opt into hard-fail behaviour per category via `StrictMode`.

## Quick example

Read a `.ts` file, feed it to a `Demuxer`, drain events:

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoPayload, split_video};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read("input.ts")?;
    let mut d = Demuxer::new();
    d.feed(&bytes)?;
    d.flush(); // recover trailing PES at end-of-file
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::ProgramMap(m) => {
                println!("PMT: {} streams", m.streams.len());
            }
            DemuxEvent::Sample { stream, pts, payload, .. } => {
                if let SamplePayload::Video { codec, raw, av1_carriage, .. } = payload {
                    // Raw-first: `raw` is the exact encoded access unit.
                    // Splitting it into NAL/OBU units is an opt-in call.
                    let (payload, _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default());
                    if let VideoPayload::Nals(nals) = payload {
                        println!("video PID 0x{:04X} pts={pts} nals={}", stream.pid, nals.len());
                    }
                }
            }
            DemuxEvent::Metadata { stream, pts, payload, .. } => {
                println!("klv PID 0x{:04X} pts={pts} bytes={}", stream.pid, payload.len());
            }
            _ => {}
        }
    }
    Ok(())
}
```

Runnable: [../examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs).

## Public surface

| Type / function | What it is |
| --- | --- |
| `Demuxer` | Stateful TS demuxer. `feed` bytes in, `next_event` events out, `flush` at stream end. |
| `DemuxerConfigBuilder` | Fluent builder for the demuxer's options. Obtain via `DemuxerConfig::builder()`. |
| `DemuxerConfig` | Plain struct of options if you'd rather build a config than chain. |
| `DemuxEvent` | Top-level event enum: `ProgramMap`, `Sample`, `Metadata`, `Discontinuity`, `NonConformant`, `ReconnectDiscontinuity` (emitted only by `ManagedDemuxReceiver` after a reconnect; signals a hard byte-stream discontinuity). |
| `StreamId` | `{ pid: u16, kind: StreamKind }` — identifies the source stream of every event. |
| `StreamKind` | `Video(VideoCodec)`, `Audio(AudioCodec)`, `Subtitle(SubtitleCodec)`, `KlvSync { declared_link }`, `KlvAsync`, `Unknown(u8)`. |
| `VideoCodec` | `H264`, `H265`, `H266`, `Av1`. |
| `AudioCodec` | `Mp2`, `Aac` (ADTS), `AacLatm`, `Ac3`. Codec tag for typed dispatch; bitstream bytes ride on `SamplePayload::Audio.frames`. |
| `SubtitleCodec` | `DvbSubtitling`, `DvbTeletext`, `Cea708Standalone` (separate ES, "GA94"), `WebVttInTs` ("VTTC"). |
| `SamplePayload` | `Video { codec, raw, random_access_indicator, av1_carriage }`, `Audio { codec, frames }`, `Subtitle { codec, payload }`, `Unknown { stream_type, raw }`. **Raw-first:** `raw` is the exact encoded access unit (a `SharedBytes`); call `split_video(&raw, codec, av1_carriage.unwrap_or_default())` to split it into a `VideoPayload` — `Nals(Vec<NalUnit>)` for H.264 / H.265 / H.266 or `Obus(Vec<Obu>)` for AV1. |
| `split_video` / `split_video_strict` | `split_video(raw: &SharedBytes, codec: VideoCodec, av1_carriage: Av1CarriageMode) -> (VideoPayload, Vec<NonConformantIssue>)` — opt-in split of a raw video AU into NAL/OBU units (lenient; ES-conformance findings come back in the issue list). `split_video_strict` returns `Err(NonConformantIssue)` on the first issue. |
| `NalUnit` | `H264 { nal_type, ref_idc, payload }` / `H265 { nal_type, layer_id, temporal_id_plus1, payload }` / `H266 { nal_type, layer_id, temporal_id_plus1, payload }`. RBSP bytes; Annex-B start codes stripped. |
| `Obu` | AV1 OBU: `{ obu_type, extension: Option<ObuExtension>, payload }`. Header byte + optional extension byte + LEB128 `obu_size` consumed; `payload` is OBU body bytes. `obu_type` = 1 SequenceHeader / 2 TemporalDelimiter / 3 FrameHeader / 6 Frame / etc. (AV1 §5.3.2). |
| `MetadataKind` | `KlvSyncAuCell { metadata_service_id, sequence_number, cell_fragment_indication, decoder_config_flag, random_access_indicator, was_reassembled, cell_count }` (first 5 fields per H.222.0 § 2.12.4.2 Table 2-156, AU cell unwrapped; `was_reassembled` / `cell_count` describe multi-cell reassembly), `KlvAsync` (bare LS), `Unknown(u8)`. |
| `ProgramMap` | `{ program_number, pcr_pid, pmt_pid, streams: Vec<StreamInfo>, klv_links: Vec<KlvLink> }`. `pmt_pid` is the PAT-declared PID carrying this program's PMT; needed to reconstruct a muxer config via `MuxerConfig::from_program_map`. |
| `StreamInfo` | `{ pid, stream_type, kind, program_number, raw_descriptors: Vec<RawDescriptor> }` — one row per declared stream in the PMT. `raw_descriptors` carries the raw PMT per-stream descriptor TLVs (tag + data bytes), in PMT loop order. |
| `KlvLink` | `{ klv_pid, video_pid, source: LinkSource }`. |
| `LinkSource` | `Declared` (PMT `metadata_descriptor`), `Inferred` (single video + single KLV topology), `Override` (`DemuxerConfigBuilder::link_klv`). |
| `NonConformantIssue` | `StreamTypeMismatchSyncOnAsyncPid`, `StreamTypeMismatchAsyncOnSyncPid`, `MissingMetadataDescriptor`, `PcrAnomaly { delta }`, `PsiChecksumMismatch { pid }`, `PusiMidPes`, `PidReusedAcrossPrograms { pid, programs }`, `SubtitleMissingDescriptor { pid }`, `SubtitleDescriptorMalformed { pid, tag }` (reserved — not currently emitted), `Other(String)`. |
| `DiscontinuityKind` | `ContinuityJump { expected, observed }`, `PesOversize { pid }`, `PesTotalOversize`, `AdaptationFieldFlag`. |
| `StrictMode` | `Off` (default), `TimingOnly`, `DescriptorsOnly`, `Full`. |
| `pts_to_duration(pts: Pts90khz) -> Duration` | Convenience: 90 kHz ticks to `std::time::Duration`. Diagnostic / test use. |

The complete enum / struct definitions live in
[../crates/tst-core/src/mpegts/demux/event.rs](/crates/tst-core/src/mpegts/demux/event.rs).

**Event variants in brief.** `DemuxEvent::ProgramMap` arrives when the
demuxer first sees a PAT/PMT or on any PSI version bump, and carries the
full declared stream topology including `klv_links`. `DemuxEvent::Sample`
carries one reassembled elementary-stream access unit (video, audio,
subtitle, or unknown) tagged by `StreamId` and `SamplePayload` variant.
`DemuxEvent::Metadata` carries KLV — both sync AU-cell
(`MetadataKind::KlvSyncAuCell`) and async bare-LS
(`MetadataKind::KlvAsync`) — tagged with the KLV PTS and unwrapped
payload. In the Python binding, `DemuxEvent.Klv` is a same-object
deprecated alias for `DemuxEvent.Metadata` (removed at 1.0, PR #79);
in Rust the variant is `Metadata` only. `DemuxEvent::Discontinuity` signals a CC
jump or PES overflow on a specific PID. `DemuxEvent::NonConformant`
surfaces a spec violation that the demuxer tolerated in lenient mode.
`DemuxEvent::ReconnectDiscontinuity` is injected only by
`ManagedDemuxReceiver` after a transport reconnect to signal a hard
stream break; plain `Demuxer::next_event` never emits it.

### `Demuxer` methods

```text
Demuxer::new()                                          -> Demuxer
Demuxer::with_config(config: DemuxerConfig)            -> Demuxer
Demuxer::feed(&mut self, bytes: &[u8])                  -> Result<(), DemuxError>
Demuxer::next_event(&mut self)                          -> Option<DemuxEvent>
Demuxer::flush(&mut self)                               -> ()
Demuxer::reset_sync(&mut self)                          -> ()
```

`feed` accepts arbitrary byte slices — the demuxer handles sync
recovery internally. It can return `DemuxError::Unrecoverable` (no TS
sync byte within the search window — ~6 KiB by default — which usually
means the input isn't TS at all), `DemuxError::MalformedPes` (a PES
header that doesn't validate), or `DemuxError::StrictRejection` (a
strict-mode-rejected `NonConformant` issue surfaced as a fatal error).

**Sync-ingress ceiling.** Before the demuxer acquires its first sync
lock, `feed` buffers incoming bytes to scan for the `0x47` sync byte.
This pre-sync buffer is capped at **4 MiB** by default. A single-shot
`feed` call of a large `.ts` file that exceeds this ceiling will return
an error before any events are emitted. The fix is either to raise the
ceiling via `DemuxerConfig::sync_buf_cap`, or to chunk the input and
drain events between chunks. Every `feed` call appends to one internal
buffer (`sync_buf`), parses whole 188-byte packets out of it and compacts
what it consumed — before and after sync lock alike — so the ceiling
bounds the *unparsed backlog*, and a chunked feed keeps that backlog at
one chunk. The chunk-and-drain loop is
the recommended pattern for file replay regardless of file size, because
it avoids allocating the entire file in the demuxer's queue at once:

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use std::fs;

fn replay_file(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    let mut d = Demuxer::new();
    // Feed 188-packet chunks (188 * 1024 = one nominal TS chunk).
    const CHUNK: usize = 188 * 1024;
    for chunk in bytes.chunks(CHUNK) {
        d.feed(chunk)?;
        while let Some(ev) = d.next_event() {
            // process ev ...
            let _ = ev;
        }
    }
    d.flush();
    while let Some(ev) = d.next_event() {
        let _ = ev;
    }
    Ok(())
}
```

To raise the ceiling instead (for callers that have the full file in
memory and want a single `feed` call):

```rust,no_run
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};

let mut d = Demuxer::with_config(
    DemuxerConfig::builder()
        .sync_buf_cap(64 * 1024 * 1024) // 64 MiB
        .build(),
);
```

`next_event` is non-blocking — returns `None` when the queue is empty.
The standard pattern is `feed`-then-drain in a loop. Events accumulate
across `feed` calls, so a streaming source can call `feed` repeatedly
as bytes arrive.

`flush` emits any in-flight events from partial PES still sitting in
reassembly state. The classic case is a video PES with `PES_packet_length=0`
(unbounded length, normal for AUs > 65535 bytes) which only commits when
the next PES arrives — at end-of-file there is no next PES, so without
`flush` the trailing AU vanishes silently. `flush` is idempotent and
safe to call repeatedly. For live SRT receive, `pipeline::Receiver`
auto-flushes on `Closed` — you only call `flush` directly when feeding
finite inputs (file replay, test fixtures).

`reset_sync` discards the 188-byte syncer state and any in-flight PES
reassembly — used by `ManagedDemuxReceiver` on reconnect to force a
fresh `0x47` sync hunt on the first packet from the new transport.
Reassembly tables (PAT/PMT, per-PID CC history, last PTS) are
*preserved* across `reset_sync`; only the byte-level sync rail and any
partial PES are dropped. Most direct callers should not need this —
call it only when the byte stream is known to have a hard discontinuity
that can't be diagnosed from PCR or CC alone.

```rust,no_run
use tst_core::mpegts::demux::Demuxer;
use std::fs;

fn drain_file(path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    let mut d = Demuxer::new();
    d.feed(&bytes)?;
    d.flush();
    let mut count = 0;
    while d.next_event().is_some() {
        count += 1;
    }
    Ok(count)
}
```

## Lenient vs. strict modes

`StrictMode` is the knob that turns `NonConformant` events into hard
errors. The default is `Off` (lenient). Strict mode categorises the
checks because real-world ISR streams routinely violate descriptor /
stream-type rules — a single "strict everything" mode would be unusable
on most live data.

| Mode | Rejects | Use case |
| --- | --- | --- |
| `Off` (default) | nothing | Triage, real-world capture analysis, live receivers. |
| `TimingOnly` | `PcrAnomaly`, `PusiMidPes` (vestigial — not currently emitted), `PsiChecksumMismatch` | Receivers paranoid about clock integrity but tolerant of encoder quirks. |
| `DescriptorsOnly` | `StreamTypeMismatch{Sync,Async}OnPid`, `MissingMetadataDescriptor` | Spec-compliance gating: did the encoder declare its streams correctly? |
| `Full` | every variant including future-added ones | CI-grade compliance test against a known-good encoder. |

In strict mode, the rejected `NonConformant` event is still pushed onto
the event queue *before* the error returns from `feed`. This means a
caller draining events alongside the error gets the full narrative —
`feed` returns `Err(DemuxError::StrictRejection(_))` *and* a subsequent
`next_event()` returns the structured `NonConformant { issue, .. }`.

`StrictMode` is **TS-layer only**: it gates PSI / PES / timing
conformance, not the contents of a video elementary stream. Malformed
NAL/OBU bitstreams are not inspected during demux — that conformance
check is the opt-in `split_video_strict(&raw, codec, av1_carriage.unwrap_or_default())` (or the issue list
returned by `split_video`).

```rust,no_run
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};

let _d = Demuxer::with_config(
    DemuxerConfig::builder()
        .strict(StrictMode::DescriptorsOnly)
        .build(),
);
```

## Override surface

`DemuxerConfigBuilder` exposes four override knobs. Use them when the encoder
lies, when memory pressure matters, or when topology inference is
ambiguous.

| Method | What it does | When to reach for it |
| --- | --- | --- |
| `sync_buf_cap(bytes)` | Maximum bytes the demuxer holds unparsed across `feed` calls (the same buffer serves before and after sync lock). Default 4 MiB. A `feed` whose bytes would push the backlog past the cap returns `DemuxError::SyncBufExhausted` without copying them. | Single-shot `feed` of a large `.ts` file (> 4 MiB). Prefer the chunk-and-drain loop instead; see the "Sync-ingress ceiling" note above. |
| `link_klv(klv_pid, video_pid)` | Force a `KlvLink` between two PIDs regardless of what the PMT declares. Surfaces as `LinkSource::Override` in the `klv_links` table. | The encoder doesn't emit `metadata_descriptor`, your topology has multiple video PIDs, and you know which KLV PID feeds which video. |
| `treat_as(pid, kind)` | Override the demuxer's PMT-derived `StreamKind` for one PID. | Encoder advertises wrong `stream_type`; you know the real shape of the bytes. |
| `pes_cap_per_pid(bytes)` | Maximum PES reassembly buffer per PID. Default 4 MiB. Exceeding this emits `Discontinuity::PesOversize { pid }` and drops the partial PES. | Memory-tight environments, or paranoia against runaway PES from a malformed encoder. |
| `pes_cap_total(bytes)` | Aggregate cap across all PIDs. Default 64 MiB. Exceeding this emits `Discontinuity::PesTotalOversize` and drops. | Same as above but at the workspace level. |

```rust,no_run
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StreamKind, VideoCodec};

let _d = Demuxer::with_config(
    DemuxerConfig::builder()
        .link_klv(0x1031, 0x1011)                                // klv -> video override
        .treat_as(0x1011, StreamKind::Video(VideoCodec::H265))   // PMT lied about codec
        .pes_cap_per_pid(1 << 20)                                // 1 MiB per-PID
        .pes_cap_total(8 << 20)                                  // 8 MiB total
        .build(),
);
```

`DemuxerConfig` is the plain-struct form if you'd rather build a config
once and pass it around:

```rust,no_run
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StreamKind, VideoCodec};
use std::collections::BTreeMap;

let mut overrides = BTreeMap::new();
overrides.insert(0x1011u16, StreamKind::Video(VideoCodec::H265));
let mut config = DemuxerConfig::default();
config.stream_kind_overrides = overrides;
let _d = Demuxer::with_config(config);
```

## Robustness behaviours

The demuxer was designed against real-world captures, not against the
spec. The behaviours below are what makes lenient mode useful.

**Missing `metadata_descriptor` on a sync KLV PID.** Many encoders
declare `stream_type=0x15` on the KLV PID without the
`metadata_descriptor` that links it to the video PID. The demuxer
infers the link when there is exactly one video PID in the PMT
(surfaces in `klv_links` as `LinkSource::Inferred`); if there are
zero or multiple video PIDs it cannot infer and emits
`NonConformantIssue::MissingMetadataDescriptor` instead.

**Wrong `stream_type` on the KLV PID.** A PID declared `0x06` (private
data) that actually carries an H.222.0 § 2.12.4.2 `Metadata_AU_cell`,
or a PID declared `0x15` that carries bare async KLV. The demuxer
detects the actual shape via the leading bytes (5-byte AU cell header
+ inner SMPTE UL vs. bare ST 0601 UL at offset 0), classifies
correctly, and emits
`NonConformantIssue::StreamTypeMismatch{Sync,Async}On*Pid`. To avoid
flooding the event stream when a stream emits the same mismatch on
every record (often thousands), the issue is coalesced to one event
per (PID, PMT version) — re-arms on PMT version bump.

**Continuity-counter jumps (CC discontinuities).** When the per-PID CC
skips a value, the demuxer emits `Discontinuity::ContinuityJump
{ expected, observed }` and continues. PES reassembly state on that PID
is preserved — the CC jump is a signal, not a teardown.

**PUSI mid-PES.** A new PUSI-set TS packet arrives before the previous
PES's `PES_packet_length` has been satisfied (or before a length-zero
PES has had a chance to terminate). The demuxer silently discards the
partial PES and starts fresh on the new PUSI — this is the normal
start-of-PES path, not a separate diagnostic event, so no
`NonConformantIssue` is emitted. This is the recovery shape for live
captures where a few packets dropped upstream split a PES across a gap.
(`NonConformantIssue::PusiMidPes` exists in the enum but is vestigial —
kept for non-exhaustive-enum binary-compatibility parity only.)

**Oversize PES.** A PES grows past the per-PID or total cap. The demuxer
drops the partial bytes, emits `Discontinuity::PesOversize { pid }` (or
`PesTotalOversize`), and resumes on the next PUSI for that PID.

**Garbage prefix bytes (scan / N-of-M confirm / locked).** When a stream
starts mid-flight or recovers from severe loss, the bytes leading the next
TS packet boundary aren't TS-aligned. The demuxer scans for `0x47`; a
candidate reached by scanning (as opposed to the next packet boundary)
must pass an N-of-M stride check — at least 5 of the next 7 positions at
+188, +376, … also carry `0x47` (`SYNC_REACQ_N` / `SYNC_REACQ_M`, ffmpeg's
`mpegts_resync` values) — before the demuxer locks; a stray `0x47` inside a
payload is rejected and the scan continues. If more than
`SYNC_SEARCH_WINDOW` (32 packets, 6016 bytes) pass without a confirmed sync
it returns
`DemuxError::Unrecoverable` — that's the "this isn't TS at all"
signal.

**PSI checksum mismatch.** PAT or PMT section CRC fails. Lenient mode
falls back to the previous PSI version (so the demuxer keeps its known-
good topology) and emits `NonConformantIssue::PsiChecksumMismatch`.
Strict (`TimingOnly` or `Full`) converts to `StrictRejection`.

**PCR jumps.** A PCR-bearing packet's clock jumps more than one second
relative to the previous. The demuxer emits
`NonConformantIssue::PcrAnomaly { delta }` (delta in 27 MHz ticks)
and continues. Reported only on a PMT-declared `PCR_PID`; a PCR on an
undeclared PID is ignored rather than tracked. A per-PID backward PTS
step of more than one second
(90 000 ticks, computed 33-bit-wrap-aware) is a separate variant,
`NonConformantIssue::PtsAnomaly { delta }` (delta in 90 kHz ticks, negative).

## What gets parsed vs. passed through

Some payloads the demuxer fully types; others it surfaces verbatim so
consumers can apply their own decoders.

**Video (H.264 / H.265).** Raw-first: the demuxer reassembles the PES
into the exact encoded access unit and hands it back as
`SamplePayload::Video { codec, raw, .. }` — `raw` is the verbatim AU
(Annex-B start codes intact). The demuxer does **not** split NAL units;
that's an opt-in call. Pass the AU to `split_video(&raw, codec, av1_carriage.unwrap_or_default())` to get a
`VideoPayload::Nals(Vec<NalUnit>)`: the splitter strips the Annex-B start
codes (`0x000001` / `0x00000001`), preserves emulation-prevention bytes
(the consumer's H.264 / H.265 decoder removes them), and returns each NAL
with codec-tagged headers on `NalUnit::H264` / `NalUnit::H265`. Callers
re-emitting to a downstream Annex-B sink that already have `raw` in hand
can forward it verbatim; callers reconstituting from split NALs prepend
`0x00 0x00 0x00 0x01` between them — see
[../examples/codec-parsing/extract_video_au.rs](/examples/codec-parsing/extract_video_au.rs).

**Sync KLV (`stream_type=0x15`).** The demuxer detects the H.222.0
§ 2.12.4.2 `Metadata_AU_cell` shape (5-byte header followed by an
inner KLV LS at offset 5), peels the AU cell, and emits
`MetadataKind::KlvSyncAuCell { metadata_service_id, sequence_number,
cell_fragment_indication, decoder_config_flag,
random_access_indicator }`. The event's `pts` is the PES PTS (per
§ 2.12.4.1 — the AU cell carries no embedded timestamp). The
`payload` is the inner KLV LS bytes — feed directly to
`klv::st0601::decode`.

**Async KLV (`stream_type=0x06` + `KLVA` registration descriptor).**
The PES payload is bare KLV LS bytes. `MetadataKind::KlvAsync`. The
`pts` is the raw PES PTS (or zero if the PES carried no PTS).

**Real-world wrinkle: stream_type vs. shape mismatch.** Some
production ISR encoders emit `stream_type=0x15` (declared sync) but
ship a bare KLV LS payload, with no AU cell wrap. The demuxer detects
the actual shape (no 5-byte header), surfaces the bytes as
`KlvAsync` with the PES PTS preserved on the parent event, and emits a
`StreamTypeMismatchAsyncOnSyncPid` non-conformance event. This is why
pairing recipes (cookbook § 12) match BOTH `KlvSyncAuCell` AND
`KlvAsync` for sync-style consumers — many real captures present as
the latter after wrap-peeling.

**Recognized video stream types.** The demuxer emits each AU's raw bytes
on `SamplePayload::Video.raw`. The "Split shape" column is what
`split_video(&raw, codec, av1_carriage.unwrap_or_default())` returns for that codec:

| PMT `stream_type` byte | `VideoCodec` | `split_video` shape |
| --- | --- | --- |
| `0x1B` | `H264` | `VideoPayload::Nals(Vec<NalUnit::H264>)` (Annex-B stripped). |
| `0x24` | `H265` | `VideoPayload::Nals(Vec<NalUnit::H265>)` (Annex-B stripped). |
| `0x33` | `H266` | `VideoPayload::Nals(Vec<NalUnit::H266>)` (Annex-B stripped). |
| `0x06` + AV01 registration | `Av1` | `VideoPayload::Obus(Vec<Obu>)` (LEB128-framed). |

The AV1 case shares stream_type `0x06` with KLV-async; the demuxer
disambiguates via the `registration_descriptor` `format_identifier`
(`AV01` for AV1, `KLVA` for async KLV).

**Unknown stream types.** PIDs with `stream_type` not in the
`{0x1B, 0x24, 0x33, 0x06+AV01, 0x06+KLVA, 0x15}` set surface as
`SamplePayload::Unknown { stream_type, raw }`. The PES payload is
preserved verbatim. Audio not declared via the recognized stream_type
bytes also falls through here; use `treat_as` to route by-PID. See
[project/deferred-features.md](/docs/project/deferred-features.md).

### Multi-cell AU cell reassembly

The demuxer reassembles fragmented Metadata AU cells per H.222.0 V9
§2.12.4.2. Most MISB sync KLV deployments emit one KLV record per cell
(`cell_fragment_indication = '11'` Complete) and reassembly is a no-op
— each cell emits its own event.

Some recording pipelines split a single KLV record across multiple AU
cells (`First` → 0..n `Middle` → `Last`). The demuxer accumulates the
fragments in a per-PID buffer and emits one event when `Last`
arrives, with `MetadataKind::KlvSyncAuCell::was_reassembled = true`
and `cell_count = N`.

If reassembly fails (orphan continuation, sequence-number gap, a new
`First` arrives mid-buffer, or the per-PID 1 MiB cap is exceeded) the
demuxer drops the partial buffer and emits
`NonConformantIssue::MultiCellAu` with a typed
`MultiCellAuReason` naming the failure mode. Tune the cap via
`DemuxerConfigBuilder::au_cell_cap_per_pid(bytes)`.

#### Malformed `cell_fragment_indication` tolerance (default on)

Real-world encoders pervasively mis-set the H.222.0 V9 §2.12.4.2
Table 2-157 `cell_fragment_indication` bits — emitting `0b00` (Middle)
or `0b01` (Last) for what are actually single complete KLV records.
Empirically, this is the dominant industry mode: corpus-wide
validation across multiple gimbaled-platform vendors (251 captures,
37 GB) found ~99% of demuxer `NonConformant` events are
`MalformedAuCellCfiTolerated`. No other public reference decoder
enforces CFI either — MISB ST 1402.2 Appendix B lists the four bit
patterns without semantic explanation, FFmpeg's `mpegtsenc.c` does
not generate the 5-byte AU cell header at all, and GStreamer's
`tsdemux.c::parse_pes_metadata_frame` reads the flags byte but
discards the CFI bits. Producers ship malformed CFI and nothing
catches it.

`ts-transformer` defaults to **tolerance on** — pragmatic for any
consumer of real-world STANAG 4609 traffic. With tolerance enabled,
the demuxer validates the orphan cell's inner payload as a single
complete KLV unit (SMPTE 336M UL prefix `06 0e 2b 34` followed by a
BER length that describes exactly the available bytes). If validation
passes, the demuxer emits:

1. A `MetadataKind::KlvSyncAuCell` event with
   `cell_fragment_indication = Complete` (the substituted value),
   `was_reassembled = false`, `cell_count = 1`, and the verbatim KLV
   payload.
2. A `NonConformantIssue::CfiTolerated { pid, observed_cfi,
   treated_as }` diagnostic so callers can quantify the
   malformation, log it, or surface it to telemetry.

When validation fails (no recognized UL prefix, or BER length
mismatch suggesting a real fragment), the existing strict path runs
and only the `Orphan` diagnostic fires — tolerance does not "rescue"
payloads that look truly fragmentary.

For spec-strict conformance testing (e.g. validating a producer
against the wire spec rather than consuming real-world traffic),
disable tolerance explicitly:

```rust,ignore
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
let demuxer = Demuxer::with_config(
    DemuxerConfig::builder()
        .cfi_tolerance(false)
        .build(),
);
```

Strict mode then surfaces orphan Middle/Last cells as
`NonConformantIssue::MultiCellAu { reason: MultiCellAuReason::Orphan }`
and emits no metadata event.

## Reading per-stream descriptors

Every `DemuxEvent::ProgramMap`'s `streams: Vec<StreamInfo>` carries
the parsed PMT descriptor list for each PID in
`StreamInfo::raw_descriptors: Vec<RawDescriptor>`. Use this to
decode vendor-specific or stack-shape descriptors that the standard
label decoder can't generalize over. The raw descriptors are also the
source `MuxerConfig::from_program_map` uses to recover ISO 639 audio
language codes — see the transmux guide in
[guides/mpegts-mux.md](/docs/guides/mpegts-mux.md#rebuilding-a-muxer-config-from-a-demuxed-program).

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::demux::psi::extract_user_label;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    // ... feed bytes ...
    while let Some(event) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = event {
            for stream in &pm.streams {
                // Quick label decode — picks up Component, Stream Identifier,
                // Metadata, ISO 639, and tag 0xFF (user-private) UTF-8.
                let label = extract_user_label(&stream.raw_descriptors)
                    .unwrap_or_else(|| "(unlabeled)".into());
                println!("PID 0x{:04X} ({}): {} descriptors", stream.pid, label,
                    stream.raw_descriptors.len());

                // Custom decoding for vendor-specific descriptors (e.g.,
                // ARS-shape senders that use tag 0xFF as the de-facto label slot,
                // or senders with HDMV trailing bytes on video PIDs).
                for d in &stream.raw_descriptors {
                    if d.tag == 0x05 && d.data.starts_with(b"HDMV") {
                        println!("  HDMV trailing bytes: {:02X?}", &d.data[4..]);
                    }
                }
            }
        }
    }
    Ok(())
}
```

### What `extract_user_label` reads

In priority order:

1. Component descriptor (tag 0x50) — UTF-8 text after the 6-byte header.
2. Stream Identifier descriptor (tag 0x52) — formatted as `tag=N`.
3. Metadata descriptor (tag 0x26) — generic `"KLV"` label.
4. ISO 639 Language (tag 0x0A) — 3-byte language code.
5. User-private (tag 0xFF) — best-effort UTF-8 (added so labels round-trip
   with ARS-shape senders that use tag 0xFF as the de-facto label slot).

Conformant descriptors win when present. If none of these match,
returns `None` — the demuxer-side stats label stays unset for that
PID.

## Pairing is a consumer concern

The demuxer surfaces every video AU and every KLV record as an
independent event with full timing. It does **not** pair sync KLV with
video AUs. This is a deliberate decision — pairing tolerance, sample-
and-hold semantics, and multi-stream routing are domain choices the
library can't make correctly for everyone.

The three canonical pairing patterns are documented as cookbook recipes
with runnable examples:

- **[Pair sync-KLV with video AUs by nearest PTS](/docs/cookbook/pairing/pair-klv-by-pts.md).**
  The "frame and metadata are the same wall-clock event" workflow.
  Match on both `MetadataKind::KlvSyncAuCell` AND `MetadataKind::KlvAsync`
  (because of the AU-cell wrap-peeling case described above). Tolerance
  window is consumer domain knowledge.
- **[Sample-and-hold async-KLV against video frames](/docs/cookbook/pairing/sample-hold-klv.md).**
  KLV at 1–10 Hz, video at 25–60 fps. Each frame uses the most recent
  KLV record where `klv.pts <= frame.pts`. Optional staleness drop.
- **[EO + IR sensor pair with shared async-KLV](/docs/cookbook/pairing/eo-ir-shared-klv.md).**
  Two video PIDs, one metadata PID; both videos attach the same KLV
  state, no per-stream pairing logic.

A potential `pipeline::pairing` opt-in helper module is captured in
[project/deferred-features.md](/docs/project/deferred-features.md) — not part of this ship.

## Common pitfalls

**Forgetting to drain `next_event` after `feed`.** `feed` accumulates
events into an internal queue but does not block on draining. If you
only call `feed` repeatedly and never call `next_event`, the queue
grows unbounded. The pattern is `feed`-then-drain.

**Forgetting `flush()` on finite inputs.** Video PES with
`PES_packet_length=0` only commits when the next PES arrives. At
end-of-file there is no next PES, so the trailing AU sits in the
reassembler. Call `flush()` once you know no more bytes are coming
(file replay, test fixture, end of `cargo run`'s `main`).

**`flush()` not needed for live SRT receive.** `pipeline::Receiver`
auto-flushes on `TransportError::Closed`. You only call `flush()`
yourself when feeding the demuxer directly.

**Assuming the NAL units from `split_video` are Annex-B framed.** They aren't.
`SamplePayload::Video.raw` *is* the Annex-B access unit (start codes intact) —
but once you split it with `split_video(&raw, codec, av1_carriage.unwrap_or_default())`, each
`NalUnit::H264` / `NalUnit::H265` carries the RBSP bytes only: the Annex-B
start codes have been stripped. Re-emit start codes between NALs yourself if
writing split NALs back to an Annex-B sink (or just forward `raw`, which is
already framed). Pattern shown in
[../examples/codec-parsing/extract_video_au.rs](/examples/codec-parsing/extract_video_au.rs).

**Treating `Closed` as an error.** It isn't. `pipeline::Receiver` turns
`TransportError::Closed` into iterator termination — the `for` loop
simply ends after `Demuxer::flush` runs. `Broken` is the peer-disconnect
error variant; `Closed` is clean EOF. See `srt_recv_typed.rs`'s
"stream-end contract" doc-comment for the full discussion.

**Matching only `KlvSyncAuCell` for sync pairing.** Production ISR
captures often surface sync KLV as `KlvAsync` (encoder declares the
PID `stream_type=0x15` but emits bare KLV without the 5-byte AU cell
header) — the PES PTS is still attached to the event, so the bytes
remain PTS-aligned with video. The cookbook
nearest-PTS pairing recipe matches both. Matching only `KlvSyncAuCell` silently drops
the most common shape we see in the field.

**Reaching for strict mode by default.** Strict mode is not the
"correct" mode — it's the compliance mode. Real-world captures
violate descriptor and stream-type rules routinely. Use lenient by
default; reach for strict only for a CI gate against a known-good
encoder, or for triage of a specific non-conformance category.

## Reading audio frames

The demuxer surfaces audio via `SamplePayload::Audio { codec, frames }`:

```rust
for event in receiver {
    if let DemuxEvent::Sample {
        stream,
        pts,
        payload: SamplePayload::Audio { codec, frames },
        ..
    } = event {
        match codec {
            AudioCodec::Mp2 => decode_mp2(&frames, pts),
            AudioCodec::Aac => decode_aac_adts(&frames, pts),
            AudioCodec::AacLatm => decode_aac_latm(&frames, pts),
            AudioCodec::Ac3 => decode_ac3(&frames, pts),
        }
    }
}
```

`frames` holds the raw PES payload bytes — one or more codec frames
concatenated. Per-frame splitting (sync-word scanning) is the
caller's job today; future `codec::aac` / `codec::ac3` parsers will
add typed split helpers (deferred).

`pts` is in 90 kHz ticks (the MPEG-TS standard). `dts` is always
`None` for audio (no B-frame reorder).

### Non-conformant captures

Real-world captures sometimes carry audio on non-conformant PMT
stream_types — for example, the Shotover-ARS encoder family puts
MP3 audio on user-private stream_type `0xF1` alongside KLV. The
demuxer's default classification surfaces these PIDs as
`StreamKind::Unknown(0xF1)`.

To route them to typed audio, set `DemuxerConfig::stream_kind_overrides`
(or use the `DemuxerConfigBuilder::treat_as` one-liner):

```rust
let mut config = DemuxerConfig::default();
config.stream_kind_overrides.insert(0x101, StreamKind::Audio(AudioCodec::Mp2));
let demuxer = Demuxer::with_config(config);
```

The override fires before the PMT-driven classification, so overridden
PIDs always get the caller-specified `StreamKind`.
The bitstream-vs-stream_type mismatch isn't validated at the
carriage layer — caller's decoder handles whatever bytes come
through.

## Subtitle parsing

Subtitle / caption PIDs carry one of four codecs identified by the
demuxer's classification cascade on `stream_type = 0x06`:

```
Priority on stream_type 0x06 (first match wins):
  1. subtitling_descriptor (tag 0x59) → Subtitle(DvbSubtitling)
  2. teletext_descriptor (tag 0x56) or VBI_teletext (0x46) → Subtitle(DvbTeletext)
  3. registration_descriptor format_identifier "VTTC" → Subtitle(WebVttInTs)
  4. registration_descriptor format_identifier "GA94" → Subtitle(Cea708Standalone)
  5. registration_descriptor format_identifier "KLVA" → KlvAsync (existing)
  6. metadata_descriptor (tag 0x26) → KlvSync (existing)
  7. fallback → existing behavior
```

Subtitle classification inserts above KLV cases. KLV
classification is unchanged when no subtitle descriptor is
present.

The receiver-side `SubtitleCodec` enum is `Copy` and
parameter-less. Per-stream descriptor params (language, page IDs,
magazine/page) surface on `StreamInfo::raw_descriptors`; callers
decode lazily via `mpegts::descriptors::parse_subtitling_descriptor`
or `parse_teletext_descriptor`.

```rust
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};

let mut demux = Demuxer::new();
demux.feed(&bytes)?;
demux.flush();
while let Some(e) = demux.next_event() {
    if let DemuxEvent::Sample {
        stream,
        pts,
        payload: SamplePayload::Subtitle { codec, payload },
        ..
    } = e
    {
        // payload is the raw PES payload bytes — for WebVTT-in-TS
        // this is UTF-8 cue text per Apple's HLS draft; for
        // DVB-sub it's a subtitle_data_segment per ETSI EN 300 743;
        // for DVB-teletext it's a teletext_data_unit per ETSI EN
        // 300 706; for CEA-708 standalone it's cc_data_pkt
        // structures per CEA-708-D.
        println!(
            "subtitle PID 0x{:04x} codec={:?} pts={} bytes={}",
            stream.pid,
            codec,
            pts,
            payload.len()
        );
    }
}
```

### Treating non-conformant captures

`DemuxerConfig::stream_kind_overrides: BTreeMap<u16, StreamKind>`
lets you force a specific PID to a specific subtitle codec when an
upstream encoder emits WebVTT-shaped (or other subtitle-shaped)
bytes without the disambiguating descriptor:

```rust
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StreamKind, SubtitleCodec};

let mut config = DemuxerConfig::default();
config.stream_kind_overrides
    .insert(0x300, StreamKind::Subtitle(SubtitleCodec::WebVttInTs));
let mut demux = Demuxer::with_config(config);
```

Equivalently, `DemuxerConfigBuilder::treat_as(pid, kind)` is a one-liner
on the builder form.

When the override routes a PID with no recognized subtitle
descriptor in the PMT, the demuxer also emits
`NonConformantIssue::SubtitleMissingDescriptor` so consumers can
log the override.

## Examples

Four runnable examples cover the demuxer's surface:

- `cargo run -p tst-examples --example demux_to_events` — [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs)
  — file in, full event stream out. Triage-grade diagnostic.
- `cargo run -p tst-examples --example srt_recv_typed` — [examples/receiving/srt_recv_typed.rs](/examples/receiving/srt_recv_typed.rs)
  — bind a listener, wrap with `pipeline::Receiver`, drain typed events
  from a live SRT peer.
- `cargo run -p tst-examples --example pair_sync_klv` — [examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs)
  — nearest-PTS pairing of KLV records with video AUs (Cookbook §12).
- `cargo run -p tst-examples --example tee_disk_and_demux` — [examples/operations/tee_disk_and_demux.rs](/examples/operations/tee_disk_and_demux.rs)
  — `add_byte_sink` fan-out: write `.ts` to disk while consuming typed
  events, all in one pass.

Two existing examples were also retrofitted to use `Demuxer` internally:

- `cargo run -p tst-examples --example extract_klv` — [examples/klv-metadata/extract_klv.rs](/examples/klv-metadata/extract_klv.rs)
  — extract KLV records from a `.ts` capture (now `Demuxer`-driven).
- `cargo run -p tst-examples --example extract_video_au` — [examples/codec-parsing/extract_video_au.rs](/examples/codec-parsing/extract_video_au.rs)
  — extract video access units, re-emit Annex-B framing.

## Multi-program parsing

A multi-program TS is parsed transparently — `Demuxer` tracks each program in
its PAT and emits one `ProgramMap` event per program (plus on PMT version
bumps and PAT version bumps that add programs).

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};

fn main() {
    let mut d = Demuxer::new();
    // feed bytes via d.feed(...) in a loop, then:
    while let Some(event) = d.next_event() {
        match event {
            DemuxEvent::ProgramMap(pm) => {
                // pm.pmt_pid is the PAT-declared PMT PID — use it with
                // MuxerConfig::from_program_map for transmux workflows.
                println!("program {} on PMT PID 0x{:04X} carries {} streams",
                    pm.program_number, pm.pmt_pid, pm.streams.len());
            }
            DemuxEvent::Sample { stream, .. } => {
                // stream.program_number tells you which program owns this PID.
            }
            DemuxEvent::Metadata { stream, .. } => {
                // Same — stream.program_number disambiguates.
            }
            _ => {}
        }
    }
}
```

### PAT version diffing

When the PAT version bumps, the demuxer compares the new program list against
the previous one:

- Programs that disappeared have their tracker dropped and their PIDs cleaned
  from the per-PID stream-kind cache. Subsequent PES on those PIDs is silently
  dropped (the demuxer no longer knows what to do with them).
- New programs get an empty tracker; the next PMT to arrive on the new PMT PID
  populates it and triggers a `ProgramMap` event.

### PID collision across programs

ISO 13818-1 technically allows the same PID to appear in multiple programs
(different programs, different meanings). The demuxer requires uniqueness —
when a PMT introduces a PID already bound to another program, it emits
`NonConformantIssue::PidReusedAcrossPrograms` and keeps the first-program-wins
binding. The second program's tracker records the colliding stream's
`StreamInfo` entry, but PES packets on that PID continue to dispatch to the
first program.

## What's deferred

Each item below maps to an entry in
[project/deferred-features.md](/docs/project/deferred-features.md).

- **`tst_pipeline::ext::pairing` opt-in helper** — pairing stays consumer-side
  via cookbook recipes; library-level helper is deferred.
- **AV1 full Frame Header parser** — current `codec::av1::parse_frame_header_light`
  surfaces type / show flags only; per-frame size + reference management
  is decoder-scope and not in this slice.
- **H.266 APS / Picture Header NAL parsers** — APS NALs (types 17 / 18)
  and Picture Header NALs (type 19) pass through unparsed today.

See [reference/compatibility.md](/docs/reference/compatibility.md)'s `mpegts::demux` block for
the full feature-by-feature status.

## See also

- **Runnable example:** `cargo run -p tst-examples --example demux_to_events` — [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs)
- [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — the symmetric sender-side guide.
- [guides/klv.md](/docs/guides/klv.md) — decoding the KLV bytes the demuxer surfaces.
- [guides/pipeline.md](/docs/guides/pipeline.md) — `DemuxReceiver<T>` and `ManagedDemuxReceiver`.
