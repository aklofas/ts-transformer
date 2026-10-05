# MPEG-TS Muxer Guide


> **Who this is for:** You're building the sender side — combining encoded video / audio / KLV / subtitles into an MPEG-TS byte stream that conforms to STANAG 4609 / MISB ST 1402.

> **You will learn:**
> - How `MuxerConfig` declares programs, streams, and PIDs
> - How to push video access units (`push_video`), audio frames (`push_audio`), KLV (`push_klv`), subtitles (`push_subtitle*`)
> - PCR/DTS/PTS timing — what you supply vs what the muxer computes
> - The default PIDs and how to override them
> - How `MuxSender<T>` composes the muxer with a `Transport`
> - When to use the multi-program API for EO + IR + KLV streams

## Introduction

When you have encoded video, KLV metadata, audio, and subtitles and need to
combine them into an MPEG-TS stream that conforms to STANAG 4609 / MISB
ST 1402, `tst_core::mpegts::mux` is the engine. `Muxer` builds PES packets
from the payloads you push, fragments them into 188-byte TS packets, and emits
PAT, PMT (with the appropriate registration / metadata / language /
subtitling descriptors per stream), PCR, and PTS at configured cadences. Each
`push_video` / `push_klv` / `push_audio` / `push_subtitle` call corresponds to
one PES packet — the PES boundary is the AU boundary for video and the
metadata-record boundary for KLV. Output is deterministic: a function of
inputs only, with no wall-clock dependency, so the same input sequence
produces the same output bytes regardless of how the caller paces its calls.

This is the sender-side guide. The symmetric receiver-side guide is
[guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — `mpegts::demux` ships
a Rust-native TS demuxer covering the same wire shape. Consumers can
also feed extracted bytes to FFmpeg / Bento4 / JavaCV / platform
demuxers if they prefer.

## Capabilities

The muxer is multi-program and multi-stream from day one. A single
`Muxer` instance carries:

- **Multi-program TS** — up to 16 programs per muxer; each program
  has its own PMT, PCR pin, and elementary stream set.
- **Video** — H.264 (PMT `stream_type 0x1B`), H.265 (`0x24`), H.266
  (`0x33`), and AV1 (`0x06` + AV01 registration descriptor per the
  AV1-in-MPEG-2-TS binding); up to 16 video streams per program.
- **KLV metadata** — async `PrivateData` (`0x06`) and synchronous
  `SynchronousMetadata` (`0x15`); sync streams auto-prepend the
  5-byte H.222.0 §2.12.4.2 `Metadata_AU_cell` header per the
  MISB ST 1402 pipeline; up to 16 KLV streams per program.
- **Audio** — MPEG-1 Layer II (`0x03`), AAC ADTS (`0x0F`),
  AAC LATM (`0x11`), and AC-3 (`0x81` + AC-3 registration per
  ATSC A/53); up to 16 audio streams per program with optional
  ISO 639 language descriptors.
- **Subtitles / captions** — DVB subtitling, DVB teletext,
  CEA-708 standalone, and WebVTT-in-MPEG-TS, all carried on
  `stream_type 0x06` with auto-emitted PMT descriptors for
  receiver disambiguation; up to 16 subtitle streams per program.

`MuxerConfig::default()` produces a conservative single-program shape
(one H.264 video PID + one async KLV PID) so the simplest "hello
muxer" path stays terse. Multi-program / multi-stream configurations
are constructed via the builder API (see `MuxerConfigBuilder` below)
or by populating the `programs` field directly. See
[reference/compatibility.md](/docs/reference/compatibility.md) for the full per-codec /
per-feature support matrix.

## `MuxerConfig` shape

```rust
pub struct MuxerConfig {
    pub programs: Vec<MuxerProgramConfig>,
    pub pcr_interval_ms: u32,
    pub psi_interval_ms: u32,
    pub buffer_packets: usize,
    pub av1_carriage: Av1CarriageMode,
}
```

Each `MuxerProgramConfig` carries its own `program_number`, `pmt_pid`,
optional `pcr_pid`, program-level descriptors, and a single
`streams: Vec<StreamSpec>` list whose variants distinguish video, KLV,
audio, and subtitle streams.

`MuxerConfig::default()` returns the canonical single-program shape:
program 1 with H.264 video at PID `0x1011`, KLV `PrivateData` (async,
no PTS) at PID `0x1031`, PCR pinned to the video PID,
`pcr_interval_ms: 40`, `psi_interval_ms: 100`, `buffer_packets:
10_000`. `MuxerConfig` and `MuxerProgramConfig` are `#[non_exhaustive]`,
so outside `tst-core` you start from the default (or a builder) and set
fields, rather than writing a struct literal. Two ways to construct from
defaults plus selected overrides:

```rust,no_run
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

// Pure default — H.264 + async KLV.
let cfg_default = MuxerConfig::default();

// Field-update form: replace the single default program with an H.265 +
// synchronous-KLV program; keep cadence defaults (pcr_interval_ms,
// psi_interval_ms, buffer_packets).
let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x1011, VideoCodec::H265);
prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
let mut cfg_h265_sync = MuxerConfig::default();
cfg_h265_sync.programs = vec![prog.build()];
```

## `MuxerConfigBuilder`

When constructing from scratch rather than tweaking the default,
`MuxerProgramConfigBuilder` builds each program block, and
`MuxerConfigBuilder` ties one or more programs together with cadence
overrides. Both expose `&mut self -> &mut Self` mutators for clean
FFI-binding semantics. Methods (all in `tst_core::mpegts::mux`):

`MuxerProgramConfigBuilder`:

- `new(program_number: u16, pmt_pid: u16) -> Self`
- `add_video(&mut self, pid: u16, codec: VideoCodec) -> &mut Self`
- `add_klv(&mut self, pid: u16, stream_type: KlvStreamType, carries_pts: bool) -> &mut Self`
- `add_audio(&mut self, pid: u16, codec: AudioCodec) -> &mut Self`
- `add_audio_with_language(&mut self, pid: u16, codec: AudioCodec, language: [u8; 3]) -> &mut Self`
- `add_subtitle(&mut self, pid: u16, codec: SubtitleCodec) -> &mut Self`
- `add_data(&mut self, pid: u16, stream_type: u8, carries_pts: bool) -> &mut Self`
- `pcr_pid(&mut self, pid: u16) -> &mut Self`
- `program_descriptors(&mut self, descs: Vec<Vec<u8>>) -> &mut Self`
- `stream_descriptors_for_{video,klv,audio,subtitle,data,stream}(&mut self, idx: usize, descs: Vec<Vec<u8>>) -> Result<&mut Self, MuxError>`
- `build(&self) -> MuxerProgramConfig`

`MuxerConfigBuilder`:

- `add_program(&mut self, program: MuxerProgramConfig) -> &mut Self`
- `pcr_interval_ms(&mut self, ms: u32) -> &mut Self`
- `psi_interval_ms(&mut self, ms: u32) -> &mut Self`
- `buffer_packets(&mut self, n: usize) -> &mut Self`
- `av1_carriage(&mut self, mode: Av1CarriageMode) -> &mut Self`
- `build(&self) -> Result<MuxerConfig, MuxError>` — runs
  `MuxerConfig::validate`.

```rust,ignore
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn build() -> Result<MuxerConfig, tst_core::error::MuxError> {
    // Bind-then-step is the canonical shape: it translates cleanly to
    // every supported FFI binding (Kotlin .apply { }, Swift var b, Java
    // step-wise, Python attribute assignment, C opaque-handle).
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_klv(0x1031, KlvStreamType::PrivateData, false);

    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.pcr_interval_ms(40);
    b.psi_interval_ms(100);
    b.buffer_packets(10_000);
    b.build()
}
```

`MuxerConfig::builder()` is a convenience for `MuxerConfigBuilder::default()`.

## Rebuilding a muxer config from a demuxed program

In transmux workflows — receive a live stream, inspect or edit its metadata,
then re-mux byte-faithfully — you need a `MuxerConfig` that matches the
source stream's topology without hand-coding every PID. The demuxer
surfaces `pmt_pid` on every `DemuxEvent::ProgramMap`, so you have everything
you need in one event: program number, PMT PID, PCR PID, stream PIDs, codecs,
and audio language descriptors. `MuxerConfig::from_program_map` turns that
event directly into a muxer config, letting transmux callers skip the builder
entirely.

### Stream-type mapping table

The function maps each demuxed `StreamKind` to the closest representable
muxer stream. Kinds that have no lossless mux representation are strict
offenders — the function fails unless you pass them in `drop` to exclude
them; DVB subtitling/teletext are the only such kinds.

| Demuxed `StreamKind` (PMT `stream_type`) | Rebuilt as | Notes |
|---|---|---|
| `Video(H264)` — `0x1B` | `add_video(pid, VideoCodec::H264)` | Codec-for-codec; Annex-B framing preserved. |
| `Video(H265)` — `0x24` | `add_video(pid, VideoCodec::H265)` | Codec-for-codec; Annex-B framing preserved. |
| `Video(H266)` — `0x33` | `add_video(pid, VideoCodec::H266)` | Codec-for-codec; Annex-B framing preserved. |
| `Video(Av1)` — `0x06` + `AV01` registration | `add_video(pid, VideoCodec::Av1)` | AV01 registration descriptor auto-re-emitted on the rebuilt PMT. |
| `Audio(Mp2)` — `0x03` / `0x04` | `add_audio` | Demuxer recognizes both `0x03` (ISO/IEC 11172-3, MPEG-1 audio) and `0x04` (ISO/IEC 13818-3, MPEG-2 audio) as `Mp2`; rebuilt PMT emits `0x03`. Raw PMT descriptors are preserved verbatim in the rebuilt config: the ISO-639 `0x0A` descriptor passes through byte-for-byte — including uppercase codes, multiple entries, and the `audio_type` byte. The `language` field is not separately recovered; `add_audio` (not `add_audio_with_language`) is always used. Same exact-preservation rule applies to the three audio rows below. |
| `Audio(Aac)` — `0x0F` | `add_audio` | |
| `Audio(AacLatm)` — `0x11` | `add_audio` | |
| `Audio(Ac3)` — `0x81` | `add_audio` | AC-3 registration descriptor auto-re-emitted on the rebuilt PMT. |
| `KlvSync { .. }` — `0x15` | `add_klv(pid, SynchronousMetadata, carries_pts=true)` | `carries_pts` is a PES-level property; the PMT cannot declare it. `true` is the STANAG 4609 norm. |
| `KlvAsync` — `0x06` + `KLVA` registration | `add_klv(pid, PrivateData, carries_pts=true)` | Same `carries_pts` rule. Callers needing `false` build the config by hand. |
| `Subtitle(Cea708Standalone)` — `0x06` + `GA94` registration | `add_subtitle(pid, SubtitleCodec::Cea708Standalone)` | Registration descriptor auto-re-emitted. |
| `Subtitle(WebVttInTs)` — `0x06` + `VTTC` registration | `add_subtitle(pid, SubtitleCodec::WebVttInTs)` | Registration descriptor auto-re-emitted. |
| `Subtitle(DvbSubtitling)` / `Subtitle(DvbTeletext)` | **error (`ConfigInvalid`)** | DVB descriptor params (language, page IDs) are not recoverable from the PMT entry alone. Pass `StreamKindTag::Subtitle` in `drop` to exclude all subtitle streams. |
| `Unknown(stream_type)` | `add_data(pid, stream_type, carries_pts=true)` | PES pass-through keeping the raw PMT `stream_type` byte; the stream's PMT descriptors are preserved verbatim on the rebuilt PMT. Same `carries_pts` rule as KLV. Pass `StreamKindTag::Unknown` in `drop` to exclude private data streams you don't want to carry. |

The `drop` filter is kind-coarse: `StreamKindTag::Subtitle` drops *all* subtitle
streams including the representable CEA-708 / WebVTT kinds, not only the
DVB offenders. If you need finer control, build the config manually using
`MuxerProgramConfigBuilder`.

### Rust

The demuxer emits one `DemuxEvent::ProgramMap(pm)` per program per PMT
discovery (or version bump). Each `ProgramMap` describes exactly one program,
so `from_program_map` also returns exactly one program's worth of config.

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::mux::MuxerConfig;

fn transmux_single_program(bytes: &[u8]) -> Result<MuxerConfig, Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    d.feed(bytes)?;
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = ev {
            // Basic: fail on any unrepresentable stream (DVB sub/teletext);
            // unknown stream types convert to Data pass-through specs.
            let cfg = MuxerConfig::from_program_map(&pm, &[])?;
            return Ok(cfg);
        }
    }
    Err("no ProgramMap in stream".into())
}
```

The `drop` filter is optional — unknown stream types convert by default.
To exclude streams of a kind you don't want to carry (here, the private
data streams), pass that kind in `drop`:

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, StreamKindTag};
use tst_core::mpegts::mux::MuxerConfig;

fn transmux_drop_private_data(bytes: &[u8]) -> Result<MuxerConfig, Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    d.feed(bytes)?;
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = ev {
            let cfg = MuxerConfig::from_program_map(
                &pm,
                &[StreamKindTag::Unknown],
            )?;
            return Ok(cfg);
        }
    }
    Err("no ProgramMap in stream".into())
}
```

For a multi-program source the demuxer emits one `ProgramMap` event per
program. Collect them and combine via `MuxerConfigBuilder::add_program`:

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, StreamKindTag};
use tst_core::mpegts::mux::{MuxerConfig, MuxerConfigBuilder};

fn rebuild_multi_program(bytes: &[u8]) -> Result<MuxerConfig, Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    d.feed(bytes)?;
    let mut b = MuxerConfigBuilder::default();
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = ev {
            let prog_cfg = MuxerConfig::from_program_map(&pm, &[StreamKindTag::Unknown])?;
            for p in prog_cfg.programs {
                b.add_program(p);
            }
        }
    }
    Ok(b.build()?)
}
```

Note: a PSI version bump re-emits a program's `ProgramMap` mid-stream. If your
source may do that, dedupe by `pm.program_number` before `add_program` —
duplicate program numbers fail `build()`.

### Python

The Python binding mirrors the Rust event model: each `DemuxEvent.ProgramMap`
event describes one program (its `programs` tuple always has exactly one
entry); multi-program sources emit one event per program, collected across
the iteration just as in Rust.

```python
from tstrans.mpegts import Demuxer, DemuxEvent, MuxerConfig, StreamKindTag

demux = Demuxer()
demux.feed(ts_bytes)

for ev in demux:
    if isinstance(ev, DemuxEvent.ProgramMap):
        pm = ev.programs[0]  # the event's single program

        # Basic: fail on any unrepresentable stream (DVB sub/teletext);
        # unknown stream types convert to DataStreamSpec entries.
        cfg = MuxerConfig.from_program_map(pm)

        # Optional filter: exclude private data streams you don't
        # want to carry.
        cfg_filtered = MuxerConfig.from_program_map(pm, drop=[StreamKindTag.UNKNOWN])
        break
```

`MuxerConfig.from_program_map` is a `@staticmethod`. `drop` defaults to `None`
(empty — strict mode). Raises `MuxError` (kind `CONFIG_INVALID`) for
unrepresentable streams not covered by `drop` (and for any other conversion
failure, e.g. an empty `pm` whose program has no streams), and `ValueError`
for malformed input — a kind/codec mismatch on a `StreamInfo`, or
non-`StreamKindTag` entries in `drop`. Field values outside their wire range
(e.g. a pid > 0xFFFF) surface the underlying extraction error
(`OverflowError`).

### Key notes

- **Single-program scope.** Each call converts one `ProgramMap` (one program).
  In both languages the demuxer emits one `ProgramMap` event per program;
  for multi-program sources, collect them across events and combine via
  `MuxerConfigBuilder::add_program`.
- **PCR copy rule.** The demuxed `pcr_pid` is copied to the rebuilt config iff
  it equals the PID of a kept video or audio stream. KLV, data, and subtitle
  PIDs are PCR-ineligible — the PCR pin is not copied; `pcr_pid` is left
  `None` and the builder default applies (first video → first audio;
  KLV/data/subtitle are never auto-selected). An explicit PCR pin on any
  ineligible PID is rejected by `validate()`.
- **`klv_links` are ignored.** The muxer re-derives KLV-to-video linkage from
  its own configuration; the PMT-declared `metadata_descriptor` links in the
  `ProgramMap` are not forwarded.
- **Codec coverage.** For a complete picture of which PMT stream types the
  demuxer recognizes — see
  [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md#what-gets-parsed-vs-passed-through)
  and the KLV-specific table in
  [guides/klv.md](/docs/guides/klv.md#wire-format-details-pes-stream_id).

## Codec selection

| `VideoCodec` variant | PMT `stream_type` byte | Notes |
| --- | --- | --- |
| `VideoCodec::H264` | `0x1B` | Annex-B framing on `push_video`. |
| `VideoCodec::H265` | `0x24` | Annex-B framing on `push_video`. |
| `VideoCodec::H266` | `0x33` | Annex-B framing on `push_video`. |
| `VideoCodec::Av1` | `0x06` | OBU-framed (`obu_has_size_field = 1`); auto-emitted AV01 `registration_descriptor` per AV1-in-MPEG-2-TS binding §2.1. |

All four are first-class. Mid-stream codec change is out of scope — destroy
the muxer and create a new one if you need to switch codecs in a single
output file.

The diff between [../examples/muxing/mux_to_file.rs](/examples/muxing/mux_to_file.rs)
(H.264 + async KLV via `MuxerConfig::default()`) and
[../examples/muxing/mux_h265_with_klv.rs](/examples/muxing/mux_h265_with_klv.rs)
(H.265 + sync KLV via `MuxerProgramConfigBuilder`) is exactly the codec and
KLV-mode knobs:

```text
- VideoCodec::H264                         + VideoCodec::H265
- KlvStreamType::PrivateData               + KlvStreamType::SynchronousMetadata
- carries_pts: false                       + carries_pts: true
```

Everything else — PIDs, cadence intervals, push/pull contract — is
identical.

## KLV-in-TS modes

Two independent axes — `KlvStreamType` (the PMT `stream_type` byte)
and `carries_pts` (whether the PES header includes a PTS) — yield
four combinations, three of them valid:

| `stream_type` | `carries_pts` | PMT byte | What it is |
| --- | --- | --- | --- |
| `PrivateData` | `false` | `0x06` | Async KLV — no PTS, broadly recognized |
| `PrivateData` | `true`  | `0x06` | Async-shaped but with PTS (uncommon, advanced) |
| `SynchronousMetadata` | `true`  | `0x15` | Sync KLV per ST 1402 — PTS required |
| `SynchronousMetadata` | `false` | (invalid) | `MuxerConfig::validate` rejects |

`PrivateData` + `false` is the default and matches what most
receivers (FFmpeg, mediamtx, hls.js v1.7+) recognize out of the box —
the registration descriptor `KLVA` on the PMT entry tells the receiver
the private data is KLV. `SynchronousMetadata` + `true` is the strict
ST 1402 form for receivers that conform to it; the muxer emits the
same `KLVA` registration descriptor on the PMT entry.

**AU cell wrapping is muxer-side.** When you configure a KLV stream as
`KlvStreamType::SynchronousMetadata`, `Muxer::push_klv` auto-prepends a
5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 § 2.12.4.2
(Tables 2-155+2-156). Pass raw KLV LS bytes; the muxer does the wrap.
The conventional pipeline is:

```text
encode_to_vec(&ls)                       → inner KLV bytes
mux.push_klv(&inner, pts, service_id)    → muxer prepends 5-byte AU cell
                                           header (with metadata_service_id
                                           = service_id, sequence_number
                                           per-stream-counter, CFI=Complete,
                                           RAI=true), emits PES carrying the
                                           AU cell, PTS in the PES header
                                           (per § 2.12.4.1).
```

PTS lives in the PES header — the AU cell carries no embedded
timestamp. ST 1402.2 § 9.4.1 + Appendix B specializes this generic
H.222.0 AU cell for KLV by mandating `metadata_format_identifier =
"KLVA"` in the PMT metadata_descriptor; the wrapper itself is
H.222.0's. The substrate lives at `mpegts::au_cell` if you ever need
to construct or parse AU cells outside the mux/demux machinery.

For `KlvStreamType::PrivateData` streams, the muxer carries payload
through unchanged.

## PCR cadence

`pcr_interval_ms` defaults to `40` (validated `1..=100`) and applies to
each program's PCR PID independently. PCR is pinned to the program's
first video PID by default; set `MuxerProgramConfig::pcr_pid:
Option<u16>` (builder: `pcr_pid(pid)`) to override (the chosen PID must
equal a configured video or audio stream's PID — `MuxerConfig::validate`
enforces this). A 40 ms interval
gives 25 PCR samples per second, well inside the typical receiver
expectation of "one PCR every 100 ms or better".

The cadence is push-driven, not wall-clock. The muxer compares the
PTS handed to `push_video` (or `push_klv`) against the PTS at the
last PCR emission and emits a fresh PCR when the configured interval
has elapsed in stream time. This is what makes the muxer
deterministic — the same input PTS sequence produces the same output
byte sequence — and means real-time scheduling is the caller's
responsibility, not the muxer's. Drive `push_video` at the encoder's
output cadence and PCR cadence falls out automatically.

## PSI cadence

`psi_interval_ms` defaults to `100` (validated `>= 10`). PAT and PMT
emit together at this interval — they are coupled by construction,
not separately configurable. The 100 ms default matches measured
baselines from real-world STANAG 4609 captures, where five of six
sample files in the test corpus emit PSI at roughly that cadence.

PSI is detected as due using a 33-bit modular comparison against the
last PSI emission PTS, so PTS rollover (every ~26.5 hours at 90 kHz)
does not cause PSI to suppress incorrectly. Backward PTS — common
with B-frames in display order — does not retrigger PSI either.

## Bitrate budgeting

TS overhead is dominated by per-packet header cost. A 188-byte TS
packet has 4 bytes of header (and possibly a few more for adaptation-
field stuffing), leaving 184 bytes for PES payload. A typical 1 KB
video AU therefore needs roughly:

```text
1024 bytes payload + ~14 bytes PES header = 1038 bytes
1038 / 184 = 6 TS packets (~1128 bytes wire)
overhead: (1128 - 1024) / 1024 ≈ 10–12%
```

PSI overhead at the 100 ms default is two 188-byte packets every
100 ms — about 3.8 KB/s. PCR rides on existing video TS packets'
adaptation fields and adds a negligible amount.

A useful back-of-envelope for output bitrate:

```text
output_bps ≈ input_bps * 1.05 + 4 KB/s
```

For a 4 Mbps encoded video plus low-rate KLV that lands around
4.2 Mbps wire.

## `push_video` / `push_klv` contract

```text
push_video(nal: &[u8], pts: Pts90khz, key_frame: bool)                    -> Result<(), MuxError>
push_klv(klv: &[u8], pts: Pts90khz, metadata_service_id: u8)              -> Result<(), MuxError>
```

Required:

- `nal` for `push_video` must be in Annex-B framing — either start
  code `0x000001` or `0x00000001`. Otherwise the call returns
  `MuxError::InvalidNal` and muxer state is unchanged.
- One PES packet per call — the PES boundary equals the AU boundary
  for video and the metadata-record boundary for KLV.
- `pts_90khz` is the MPEG-TS 90 kHz clock value, signed `i64`. A
  30 fps stream ticks PTS by `90_000 / 30 = 3000` per frame.
- `key_frame: true` should correspond to IDR (H.264) or
  `IDR_W_RADL` / `IDR_N_LP` (H.265). The muxer sets
  `random_access_indicator` in the adaptation field of the first TS
  packet of the resulting PES.
- `push_klv`'s `pts_90khz` is honoured only when the KLV stream was
  configured with `carries_pts: true`; otherwise it is ignored.
- `push_klv`'s `metadata_service_id` lands in the AU cell header per
  H.222.0 § 2.12.4.2 / ST 1402.2 App. B Table 2 ONLY when the
  configured `KlvStreamType` is `SynchronousMetadata` (stream_type
  0x15). `PrivateData` (stream_type 0x06) streams pass payload through
  verbatim with no AU cell wrap, so the parameter is silently ignored
  on that path. Spec default is `0x00`. Mirror a non-default
  `metadata_klva(svc)` PMT descriptor's `service_id` if you need
  consistency between the PMT advertisement and the wire AU cell.
- KLV blobs are bounded by `PES_packet_length` (`u16`) — `65532`
  bytes without PTS, `65527` bytes with. Real ST 0601 packs are
  typically under 2 KB so this is a sanity check, not a regular
  failure.

`MuxError` variants (full list in
[../crates/tst-core/src/error.rs](/crates/tst-core/src/error.rs)):

- `InvalidConfig(&'static str)` — `MuxerConfig::validate` rejected the
  configuration; the message names the failed rule.
- `InvalidNal` — `push_video` was handed a buffer without an
  Annex-B start code.
- `BufferFull { capacity_packets: u64 }` — the resulting TS
  packets would exceed `MuxerConfig::buffer_packets`. Drain via `pull`
  and retry. State is unchanged when this variant fires. An access unit
  that alone needs more than `buffer_packets` packets (≈ 184 payload
  bytes each) fails even on an empty queue — raise `buffer_packets`.
- `KlvTooLarge { size: usize, max: usize }` — `push_klv` blob
  exceeds the `PES_packet_length` ceiling.

## `pull` contract

```text
pull(&mut [u8]) -> usize
```

`pull` is infallible — there are no failure modes that don't already
surface at `push_video` / `push_klv` time. It returns the number of
bytes written: `0` if the queue is empty or `out.len() < 188`, otherwise
a positive multiple of 188. The caller drives a drain loop until `pull`
returns 0.

The standard pattern is push-then-drain after every push so the
muxer's internal queue stays bounded:

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, Muxer};

fn drain_pattern() -> Result<(), Box<dyn std::error::Error>> {
    let mut mux = Muxer::new(MuxerConfig::default())?;
    let mut buf = [0u8; 1316]; // 7 TS packets — typical SRT payload size
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0x00];
    mux.push_video(&nal, Pts90khz::new(0), true)?;
    let klv = vec![0xAB; 50];
    mux.push_klv(&klv, Pts90khz::new(0), 0x00)?;
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        // ship buf[..n] downstream
    }
    Ok(())
}
```

## Multi-stream output

The muxer accepts up to **16 video** + **16 KLV** elementary streams in
a single program (one PMT, one PAT). Use this shape when you need:

- **Dual-camera platforms.** EO (visible) + IR (thermal) sensors on a
  stabilized turret, both streamed in the same TS — typical for ISR
  pods.
- **Multi-metadata pods.** Vehicle telemetry on one KLV PID, sensor
  metadata on another (common when the sensor and platform are
  separately instrumented).
- **Combinations of the above.** N video + M KLV in any ratio (N ≥ 1
  unless the program has an audio stream to carry the PCR; N ≤ 16,
  M ≤ 16).

### Building a multi-stream MuxerConfig

Build the program with multiple `add_video` / `add_klv` calls on the
program builder, then hand it to the outer `MuxerConfigBuilder`:

```rust
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x1011, VideoCodec::H264); // EO
prog.add_video(0x1021, VideoCodec::H264); // IR
prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
prog.pcr_pid(0x1011); // pin PCR to the EO stream — see "PCR rule" below

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
```

Validation:

- More than 16 streams of either kind → `MuxError::TooManyVideoStreams`
  / `TooManyKlvStreams` (cap is generous; ask if you need more).
- Duplicate PIDs across any pair of streams in a program →
  `MuxError::InvalidConfig` (across programs:
  `MuxError::DuplicatePidAcrossPrograms`).
- `pcr_pid` (if set) must equal a configured stream's PID, or
  validation rejects; a KLV, subtitle or data PID is rejected as a PCR
  pin.
- Each program needs at least one video or audio stream to carry the
  PCR — otherwise `MuxError::NoPcrEligibleStream`. Video-only output is
  supported; KLV-only output is not.

### Stream handles

After `Muxer::new` succeeds, obtain a handle per configured stream:

```rust
let mut mux = Muxer::new(cfg)?;
let eo = mux.video_stream_handle(0).unwrap();
let ir = mux.video_stream_handle(1).unwrap();
let klv = mux.klv_stream_handle(0).unwrap();
```

Handles are opaque (`VideoStreamHandle` / `KlvStreamHandle` — both
`Copy + Eq + Hash`). They're tied to the muxer that produced them —
passing one to a different muxer surfaces as
`MuxError::InvalidStreamHandle`.

You can also enumerate everything in declaration order:

```rust
for h in mux.video_handles() { /* ... */ }
for h in mux.klv_handles()   { /* ... */ }
```

### Pushing to a specific stream

```rust
mux.push_video_to(eo, &eo_nal, pts, key_frame)?;
mux.push_video_to(ir, &ir_nal, pts, key_frame)?;
mux.push_klv_to(klv, &klv_blob, pts, /*metadata_service_id=*/ 0x00)?;
```

Each push is independent — no cross-stream synchronization is
implied by the API. PSI (PAT/PMT) is re-emitted on a single timeline
driven by whichever push call fired most recently.

### Single-target convenience APIs in the multi-stream world

The no-suffix `Muxer::push_video` and `Muxer::push_klv` (and the
`MuxSender::send_video` / `send_klv` wrappers) only work when **exactly
one** stream of that kind is configured. Otherwise they return
`MuxError::AmbiguousTarget`. This keeps single-stream callers
unchanged while making it impossible to accidentally route bytes to
the wrong stream when N > 1.

### PCR rule

`MuxerProgramConfig::pcr_pid` controls which PID carries each program's PCR:

- If unset, the muxer pins PCR to the first video stream's PID
  (or the first audio stream's PID if there is no video). KLV, data,
  and subtitle streams are never auto-selected.
- If set to a value that matches no configured stream, validation
  rejects with `MuxError::InvalidConfig`; a KLV, subtitle or data PID is
  rejected with `KlvPidUsedAsPcrPid` / `SubtitlePidUsedAsPcrPid` /
  `DataPidUsedAsPcrPid`.

Each program carries its own PCR pin; a multi-program muxer (multiple
PMTs in one PAT) re-emits each program's PCR independently.

### Runnable example

`examples/muxing/mux_dual_camera.rs` builds a 30-frame EO + IR
+ KLV TS file. Run it with `cargo run -p tst-examples --example mux_dual_camera`; the
example prints the output path it wrote (under your system temp dir),
which `ffprobe -show_streams` should report as two video streams and
one data (KLV) stream.

### From the C ABI

The same fan-out is exposed in `tst-c`. Two transparent `uint32_t`
typedefs (`tst_video_stream_handle_t` / `tst_klv_stream_handle_t`)
plus a `TST_INVALID_STREAM_HANDLE` sentinel back the C surface:

```c
tst_mux_config_t* cfg = tst_mux_config_new();   /* empty: no programs yet */
tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
tst_video_stream_handle_t h_eo =
    tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
tst_video_stream_handle_t h_ir =
    tst_mux_config_add_video_stream(cfg, prog, 0x1021, TST_VIDEO_CODEC_H264);
tst_klv_stream_handle_t h_klv =
    tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TST_KLV_STREAM_TYPE_PRIVATE_DATA, false);

tst_muxer_t* mux = tst_muxer_open(cfg);
tst_mux_config_free(cfg);

tst_muxer_push_video_to(mux, h_eo, nal, sizeof(nal), pts, true);
tst_muxer_push_video_to(mux, h_ir, nal, sizeof(nal), pts, true);
tst_muxer_push_klv_to(mux, h_klv, klv, sizeof(klv), pts);
```

Same shape on the network senders: `tst_mux_sender_send_video_to` /
`_send_klv_to` and `tst_managed_mux_sender_send_video_to` /
`_send_klv_to`. The single-target entry points (`tst_*_send_video`,
`tst_*_send_klv`) return `TST_E_INVALID_USAGE` (`MuxError::AmbiguousTarget`)
on multi-stream muxers and work unchanged on single-stream ones.

The `tst_sender_t` and `tst_raw_sender_t` variants do **not**
have handle-aware siblings — they take pre-muxed TS bytes
(`send_ts(bytes)`) or opaque payload bytes (`send(bytes)`), so
multi-stream fan-out doesn't apply.

See `bindings/c/examples/muxing/mux_dual_camera.c` for a worked end-to-end
example mirroring the Rust analogue.

## Per-stream PMT descriptors

Each PMT entry in the per-stream loop can carry caller-supplied
descriptor TLVs alongside the muxer's auto-emitted KLVA Registration.
Use this when:

- You're producing a multi-stream program (EO + IR + KLV) and want
  receivers to render which PID is which without external config.
- You're emitting `KlvStreamType::SynchronousMetadata` and need the
  canonical `metadata_descriptor` (0x26) + `metadata_STD_descriptor`
  (0x27) pair that strict ST 1402 receivers expect.
- You're interoperating with a sender stack that uses tag 0xFF
  (user-private) labels on every PID.

### Building descriptor TLVs

The `mpegts::descriptors` module ships byte-builders for the
descriptor types real-world senders actually emit:

| Helper | Tag | Purpose |
|---|---|---|
| `registration(format_id, additional)` | 0x05 | "KLVA" on KLV PIDs (the muxer auto-emits it on every KLV PID), "HDMV" + trailing bytes on video PIDs |
| `metadata_klva(service_id)` | 0x26 | Canonical KLVA Metadata descriptor for `stream_type=0x15` KLV |
| `metadata_std(in, buf, out)` | 0x27 | STD-buffer dimensions, paired with 0x26 |
| `user_private(payload)` | 0xFF | De-facto label slot used in the wild ("VIDEO-ARS", "KLV_SYNC") |
| `user_private_with_tag(tag, payload)` | 0x40..=0xFF | Vendor-defined label slots |
| `component(content, type, tag, lang, text)` | 0x50 | Textbook DVB free-text label |
| `stream_identifier(component_tag)` | 0x52 | Pairs with Component for routing |
| `iso_639_language(lang, audio_type)` | 0x0A | 3-byte language code, conventional on audio |

Helpers whose payload is statically bounded (`metadata_klva`,
`metadata_std`, `stream_identifier`, `iso_639_language`,
`format_identifier_*`) return a `Vec<u8>` directly. The remaining
helpers (`registration`, `user_private`, `user_private_with_tag`,
`component`, `subtitling_descriptor_multi`,
`teletext_descriptor_multi`) accept caller-sized payloads and return
`Result<Vec<u8>, DescriptorError>` so payloads that would overflow
the 8-bit `descriptor_length` field surface as
`DescriptorError::TooLarge`. Hand the result list to one of the
`MuxerConfigBuilder` methods (use `?` on the fallible builders).

### Setting descriptors on the builder

```rust
use tst_core::mpegts::descriptors as desc;
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x100, VideoCodec::H264);
prog.stream_descriptors_for_video(0, vec![desc::user_private(b"EO 1080p")?])?;
prog.add_video(0x101, VideoCodec::H264);
prog.stream_descriptors_for_video(1, vec![desc::user_private(b"IR 640")?])?;
prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
prog.stream_descriptors_for_klv(
    0,
    vec![
        desc::metadata_klva(0x00),
        desc::metadata_std(0, 0, 0),
        desc::user_private(b"KLV_SYNC")?,
    ],
)?;

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
```

The video / KLV index passed to `stream_descriptors_for_video` /
`stream_descriptors_for_klv` is the ordinal among streams of that
kind (in add-order), not the absolute stream index. For absolute
indexing use `stream_descriptors_for_stream(absolute_idx, ...)`.

### Auto-emit and conflict suppression

The muxer auto-emits Registration `KLVA` (tag 0x05) on every KLV PID,
`PrivateData` and `SynchronousMetadata` alike. If the caller supplies
their own Registration descriptor on the same PID (any
`format_identifier`), the auto-emit is suppressed — TSDuck and
ffprobe both flag duplicate Registration descriptors as malformed.

If the caller's Registration on a KLV PID has a non-`KLVA`
`format_identifier` (e.g., a vendor-specific KLV transport tag),
the muxer emits a `tracing::warn!` since receivers that look for
the standard `KLVA` registration will not recognize the stream as
KLV. The PMT bytes still go out as the caller specified.

### PMT size limits

The muxer emits single-section PMT — the entire PMT must fit in one
188-byte TS packet. After the 17-byte PMT header overhead, the
per-stream ES loop has 166 bytes total to spend (across all streams)
on `5 + descriptor-loop-bytes` per stream. `MuxerConfig::validate`
rejects oversized configurations with `MuxError::PmtTooLarge`.

For typical configurations (3–4 streams with ~30 bytes of
descriptors each), this is plenty. If you hit the limit, drop one
or more user-supplied descriptors or shorten their payloads.
Multi-section PMT output is not supported.

## Multi-program output

A single TS multiplex can carry several independent programs, each with its
own PMT, PCR, and elementary stream set. Use this when a consumer needs to
ship multiple logically separate "channels" through one transport (e.g. two
aircraft each emitting an EO+IR+KLV bundle, aggregated through one SRT socket).

```rust
let mut prog1 = MuxerProgramConfigBuilder::new(1, 0x1000);
prog1.add_video(0x1011, VideoCodec::H264);
prog1.add_klv(0x1031, KlvStreamType::PrivateData, false);

let mut prog2 = MuxerProgramConfigBuilder::new(2, 0x1100);
prog2.add_video(0x1111, VideoCodec::H265);
prog2.add_klv(0x1131, KlvStreamType::PrivateData, false);

let mut b = MuxerConfig::builder();
b.add_program(prog1.build());
b.add_program(prog2.build());
let config = b.build()?;
```

### PID uniqueness

PIDs MUST be unique across programs. Validation rejects with
`MuxError::DuplicatePidAcrossPrograms` if two programs declare the same
stream PID. This is by design — PES packets carry only PID, so PES dispatch
on the demux side can't disambiguate same-PID-different-program. For
repacking workflows that mix sources with overlapping PID ranges, renumber
program 2's streams into a non-conflicting range.

### Per-program PCR

Each program has its own PCR PID. Set explicitly via `.pcr_pid(N)` inside
the `add_program(...)` block, or omit and let it auto-fall-back to the
program's first video PID (or first audio PID if there is no video). PCR
cadence is independent per-program — program 2's PCR doesn't drift when
program 1 is stalled.

### Push routing with handles

Multi-program callers must use `Muxer::push_video_to(handle, ...)` and
`push_klv_to(handle, ...)`. The bare `push_video` / `push_klv` reject with
`MuxError::AmbiguousTarget` when there's more than one stream of that kind
across all programs. Resolve handles per-program via
`video_handles_for_program(N)` / `klv_handles_for_program(N)`.

### Limits

`MAX_PROGRAMS = 16`. Per-program limits are unchanged from single-program:
≤16 video + ≤16 KLV streams per program. Each PMT must individually fit in
one TS packet (the size budget is per-PMT, not per-multiplex).

For a runnable end-to-end repacking example, see `examples/muxing/repack_two_programs.rs`.

## Audio output

`mpegts::mux` carries four audio codecs alongside video and KLV:

| Codec | PMT `stream_type` | Use case |
|---|---|---|
| `AudioCodec::Mp2` | `0x03` | MPEG-1 Layer II (the most common form in ISR captures) |
| `AudioCodec::Aac` | `0x0F` | AAC ADTS (most widespread internet-streaming form) |
| `AudioCodec::AacLatm` | `0x11` | AAC LATM (ETSI / ATSC mandated for HD pipelines) |
| `AudioCodec::Ac3` | `0x81` | ATSC AC-3 |

Add an audio stream to a program via the program builder:

```rust
let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x100, VideoCodec::H264);
prog.add_audio(0x300, AudioCodec::Aac);   // ← NEW

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
```

Push pre-framed audio frames with PTS:

```rust
muxer.push_audio(&adts_frames, pts_90khz)?;
```

The library treats `frames` as opaque bytes — caller is responsible
for the codec-specific framing (ADTS sync words, LATM length prefix,
AC-3 sync, MP2 frame header). One PES per `push_audio` call; bundle
multiple frames per call for tighter PES grouping if your bitrate
warrants it.

For multi-stream programs (≤16 audio streams per program), use
`push_audio_to(handle, pts, frames)` with handles from
`Muxer::audio_handles()` or `audio_handles_for_program()`. The bare
`push_audio` rejects with `MuxError::AmbiguousTarget` when the muxer
has more than one audio stream configured — disambiguating which
stream gets the call is the caller's job.

### Audio descriptors

Audio PMT entries default to bare (just the stream_type byte). Two
auto-emit shortcuts are available, plus the manual route via
`stream_descriptors_for_audio` for richer cases.

#### ISO 639 language: `add_audio_with_language`

Set the language at builder time and the muxer emits an
`iso_639_language_descriptor` (tag `0x0A`, ISO/IEC 13818-1 §2.6.18-19)
with `audio_type = 0x00` (undefined / clean main):

```rust
let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x101, VideoCodec::H264);
prog.add_audio_with_language(0x300, AudioCodec::Aac, *b"eng");
prog.add_audio_with_language(0x301, AudioCodec::Aac, *b"spa");

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
```

The plain `add_audio(pid, codec)` form keeps `language: None` and
emits no descriptor. Suppression: caller-supplied
tag-`0x0A` via `stream_descriptors_for_audio` wins (their language code
overrides; no double-emit).

For multi-language tracks or richer `audio_type` values
(visually-impaired commentary, hearing-impaired, dialogue, etc. per
§2.6.19 Table 2-83), supply the descriptor manually:

```rust
use tst_core::mpegts::descriptors::iso_639_language;

let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
prog.add_video(0x101, VideoCodec::H264);
prog.add_audio(0x300, AudioCodec::Aac);
prog.stream_descriptors_for_audio(0, vec![iso_639_language(*b"eng", 0x03)])?;

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
```

(Codec-specific audio descriptors — AC-3 audio descriptor `0x6A`, AAC
audio descriptor `0x7C` — are not pre-built; assemble via
`user_private_with_tag(tag, payload)` if needed.)

### AC-3 registration descriptor

`AudioCodec::Ac3` streams auto-emit a `registration_descriptor` with
`format_identifier = "AC-3"` per ATSC A/53 Part 3 §5.1 — without it,
strict ATSC consumers (ffmpeg, GStreamer, TSDuck) may fall back to
probing or misclassify the stream. The auto-emit mirrors the KLVA /
AV01 precedent:

- Suppressed when the caller has already supplied a tag-0x05
  Registration with `format_identifier = "AC-3"` via
  `stream_descriptors_for_audio` (no duplicate emit).
- Caller-supplied non-AC-3 Registration on an AC-3 PID logs a
  `tracing::warn!` and is left as-is — the caller's intent
  (e.g. routing to a custom DVB-shaped AC-3 path) wins. This
  differs from AV01, where the AV1-in-MPEG-2-TS binding §2.1
  hard-requires the AV01 marker for receiver classification; AC-3
  on `stream_type 0x81` can be classified without the descriptor.

DVB-shaped AC-3 (`stream_type 0x06` + DVB AC-3 descriptor `0x6A`)
remains a separate path; see `deferred-features.md`.

## Subtitle output

The muxer emits four subtitle / caption codecs as separate
elementary streams. All four share PMT `stream_type = 0x06` (PES
private data) and disambiguate via auto-emitted PMT descriptors:

| Codec | Auto-emitted descriptor | Use case |
|---|---|---|
| `DvbSubtitling { language, subtitling_type, composition_page_id, ancillary_page_id }` | `subtitling_descriptor` (tag 0x59) | ETSI broadcast (Europe) |
| `DvbTeletext { language, teletext_type, magazine_number, page_number }` | `teletext_descriptor` (tag 0x56) | ETSI broadcast (legacy) |
| `Cea708Standalone` | `registration_descriptor` `format_identifier="GA94"` | ATSC standalone-CC carry-out (best-effort) |
| `WebVttInTs` | `registration_descriptor` `format_identifier="VTTC"` | Apple HLS-compatible WebVTT-in-MPEG-TS |

The descriptor is **structurally required** for receiver
classification — without it, a `stream_type 0x06` PID is
indistinguishable from KLV-PrivateData. Caller-supplied descriptors
via `MuxerProgramConfigBuilder::stream_descriptors_for_subtitle` append after
the auto-emitted one (do NOT suppress; contrast with KLV's
`KLVA`-suppression rule).

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec, VideoCodec,
};

let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
prog.add_video(0x101, VideoCodec::H264);
prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let cfg = b.build()?;
let mut mux = Muxer::new(cfg)?;
let h = mux.subtitle_handles()[0];
mux.push_subtitle_to(
    h,
    Pts90khz::new(90_000),
    b"WEBVTT\n\n00:00:01.000 --> 00:00:05.000\nhello\n",
)?;
// Drain TS bytes via `mux.pull(&mut buf)` in a loop until it
// returns 0 (queue empty); see `examples/muxing/mux_with_webvtt_subtitles.rs`
// for a `drain_all` helper.
```

### Codec-specific PES envelope behavior

`Muxer::push_subtitle_to` applies the spec-conformant wire envelope
based on the configured `SubtitleCodec`. Pass raw payload bytes; the
muxer prepends/appends the envelope.

- **`DvbSubtitling`** — wraps caller-supplied subtitling segment bytes
  in `data_identifier(0x20) + subtitle_stream_id(0x00) + segments +
  end_of_PES_data_field_marker(0xFF)` per ETSI EN 300 743 §6.2. Pass
  raw segment bytes (each starting with sync_byte `0x0F`).
- **`DvbTeletext`** — emits the 45-byte stuffed PES header
  (`PES_header_data_length=0x24`) and pads the PES tail with `0xFF`
  stuffing to reach `(N × 184) − 6` total `PES_packet_length` per ETSI
  EN 300 472 §4.2. Pass raw teletext data unit bytes (each starting
  with `data_identifier`).
- **`Cea708Standalone`** and **`WebVttInTs`** — informal industry
  conventions with no spec-defined envelope; the muxer passes payload
  through unchanged.

### Limits and caps

- ≤16 subtitle streams per program (`MAX_SUBTITLE_STREAMS_PER_PROGRAM`).
- Subtitle PIDs cannot serve as the PCR PID — too sparse for PCR pacing.
- `push_subtitle` payload max: 65527 bytes (PES packet length budget).
- DVB-teletext `magazine_number` ∈ 0..=7; `teletext_type` ∈ 0..=0x1F; ISO 639-2 language codes are 3 ASCII alphabetic bytes (per EN 300 468 §6.2.41/§6.2.43; both lowercase and uppercase accepted).

### Multi-stream and multi-program

`push_subtitle_to(handle, pts, bytes)` dispatches by handle. Bare
`push_subtitle(pts, bytes)` rejects with
`MuxError::NoSubtitleStreamsConfigured` when no subtitle streams exist
across any program, or `MuxError::AmbiguousTarget` when ≥2 subtitle
streams exist (caller must pick one via the `_to` form). Mirrors the
shape used for video / KLV / audio.

## Private/application data streams

`StreamSpec::Data` carries arbitrary private PES payloads — serial
telemetry lines, command/control JSON, vendor binary blobs — on a
caller-chosen PMT `stream_type`. The muxer is a pure **pass-through**:
no AU-cell wrap, no payload inspection, no auto-emitted descriptors.
One push becomes exactly one PES packet (`stream_id = 0xBD`,
private_stream_1), and the demuxer round-trips the stream as
`StreamKind::Unknown(stream_type)` with byte-identical payloads.

**Data vs KLV:** use `add_klv` for MISB KLV metadata — the KLV stream
kinds own the sync-AU-cell machinery and demux as typed KLV streams
with their own event family. Use `add_data` for everything else that
should surface from the demuxer as `Unknown`.

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::descriptors::user_private;
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
prog.add_video(0x101, VideoCodec::H264);
prog.add_data(0x200, 0xF0, /*carries_pts=*/ true);
// Optional: a user-private (tag 0xFF) name descriptor so receivers can
// identify the PID. The index is data-relative (zero-indexed among
// `add_data` calls, mirroring the other stream_descriptors_for_* forms).
prog.stream_descriptors_for_data(0, vec![user_private(b"SERIAL01")?])?;

let mut b = MuxerConfig::builder();
b.add_program(prog.build());
let mut mux = Muxer::new(b.build()?)?;

let h = mux.data_handles()[0];
mux.push_data_to(h, b"USR01,line-record-1\r\n", Pts90khz::new(90_000))?;
```

### The classify-Unknown rule

`MuxerConfig::validate()` rejects a data stream whose `stream_type` +
descriptor set the demuxer would classify as anything other than
`Unknown` — both directions of the contract: a Data stream re-demuxes
as `Unknown`, and typed payloads cannot ride a Data spec to lie to
downstream demuxers. Concretely:

- Typed stream_types (`0x1B` H.264, `0x0F` AAC, `0x15` async KLV, …)
  are rejected — use the corresponding `add_video` / `add_audio` /
  `add_klv` / `add_subtitle` form instead.
- `stream_type 0x06` (PES private data) is allowed, but only without a
  classifying descriptor (KLVA registration, DVB subtitling/teletext,
  …) — attaching one flips the demux classification and fails
  validation with a `ConfigInvalid` naming the typed kind it would
  become.

### PCR and PTS

Data PIDs cannot serve as the PCR PID — pushes are caller-paced with
no cadence guarantee, so a data PID cannot promise ETSI TR 101 290
§5.6.1's 100 ms PCR ceiling. Pinning one rejects with
`MuxError::DataPidUsedAsPcrPid`, and a program needs at least one
PCR-eligible stream (video / audio) to validate.

`carries_pts = false` omits the PES PTS field entirely (the `pts`
argument to `push_data_to` is ignored); the demuxer surfaces such
samples with `pts = 0`. The PES_packet_length budget bounds a single
push to 65527 payload bytes with PTS, 65532 without
(`MuxError::DataTooLarge`).

### Limits and multi-stream

- ≤16 data streams per program (`MuxError::TooManyDataStreams`).
- `push_data_to(handle, bytes, pts)` dispatches by handle, from
  `Muxer::data_handles()` / `data_stream_handle(index)` /
  `data_handles_for_program(program_number)`. Bare `push_data` follows
  the family shape: `MuxError::NoDataStreamsConfigured` with zero data
  streams, `MuxError::AmbiguousTarget` with ≥2.
- Pipeline shells delegate one-for-one:
  `MuxSender::{send_data, send_data_to, data_handles}` and
  `MuxPublisher::send_data` — see [guides/pipeline.md](/docs/guides/pipeline.md).
- The Python binding mirrors the whole surface: `add_data` /
  `stream_descriptors_for_data` / `push_data(_to)` / the handle accessors
  on `tstrans.mpegts`, plus `push_data` / `push_data_to` / `data_handle()`
  on the srt/rtp `MuxSender`s. The C binding (`tst_mux_config_add_data_stream`,
  `tst_muxer_push_data` / `_to`) and the JVM binding
  (`MuxerConfig.Builder.addData`, `Muxer.pushData` / `pushDataTo`) expose
  data streams too.

## Examples

Three runnable examples cover the muxer's surface:

- `cargo run -p tst-examples --example mux_to_file` — [examples/muxing/mux_to_file.rs](/examples/muxing/mux_to_file.rs)
  — H.264 + async KLV via `MuxerConfig::default()`, writes a `.ts` file.
- `cargo run -p tst-examples --example mux_h265_with_klv` — [examples/muxing/mux_h265_with_klv.rs](/examples/muxing/mux_h265_with_klv.rs)
  — H.265 + sync KLV via `MuxerProgramConfigBuilder`, illustrating the
  diff against the H.264 default.
- `cargo run -p tst-examples --example send_pipeline_to_socket` — [examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs)
  — the muxer composed inside `pipeline::MuxSender` and connected to an
  SRT socket. See [guides/pipeline.md](/docs/guides/pipeline.md) for the
  sender-shell layer.

## See also

- [guides/klv.md](/docs/guides/klv.md) — encoding the KLV bytes you pass to `push_klv`.
- [guides/pipeline.md](/docs/guides/pipeline.md) — `MuxSender<T>` and the other pipeline shells.
- [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — the symmetric receiver-side guide.

## What's deferred

Each item below maps to an entry in
[project/deferred-features.md](/docs/project/deferred-features.md).

- `private_stream_2` (0xBF) data carriage — `StreamSpec::Data` always
  emits private_stream_1 (0xBD); 0xBF has no PES header (no PTS
  possible) and no observed carriage. See
  [project/deferred-features.md](/docs/project/deferred-features.md).
