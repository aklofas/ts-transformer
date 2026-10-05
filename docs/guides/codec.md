# Codec Parameter Set Guide


> **Who this is for:** You're parsing video or audio elementary streams — H.264 / H.265 / H.266 / AV1 NAL units; AAC ADTS / MPEG-2 audio frames — and need typed access to parameter sets, slice headers, sample-rate / channel info.

> **You will learn:**
> - The NAL-unit model for H.264 / H.265 / H.266 (and the OBU model for AV1)
> - How to extract SPS / PPS / VPS parameter sets for decoder warm-start
> - How audio frame parsers expose sample rate, channel layout, frame length
> - The AV1 `Mpeg2TsBinding` vs `InteropRawObu` carriage modes

## What this module is

When you have raw NAL units from the demuxer and need typed fields out of
them — width, height, profile, level, color space, frame rate, slice type —
`tst_core::codec` is the parser layer. It provides stateless functions that
operate on raw NAL unit bytes (or AV1 OBUs, or audio frame bytes).

When `mpegts::demux` surfaces a `DemuxEvent::Sample`, the video payload is
the exact encoded access unit — `SamplePayload::Video.raw`, with TS framing
and PES reassembly stripped but the elementary-stream bytes intact. The
demuxer does **not** split NAL units or parse content; both are opt-in. Split
the AU into NAL/OBU units with `split_video(&raw, codec, av1_carriage.unwrap_or_default())`, then call into the
`codec::h264` / `codec::h265` parsers explicitly for typed fields.

This design keeps the demuxer surface minimal and dependency-free. You only
pay for splitting + codec parsing when you need them, and the codec parsers
have no coupling to the transport or container layers.

## Architecture overview

```
mpegts::demux::Demuxer
  ↓ DemuxEvent::Sample { payload: SamplePayload::Video { codec, raw, av1_carriage, .. }, .. }
  ↓ raw: SharedBytes   — the exact encoded access unit (Annex-B / OBU framed)

tst_core::mpegts::demux
  split_video(&raw, codec, av1_carriage.unwrap_or_default()) → (VideoPayload, Vec<NonConformantIssue>)
  ↓ VideoPayload::Nals(nals)   — raw RBSP bytes; NAL type in the header

tst_core::codec::h264
  parse_sps(rbsp)         → Result<H264Sps, CodecParseError>
  parse_pps(rbsp)         → Result<H264Pps, CodecParseError>
  parse_parameter_sets(nals) → Result<H264ParameterSets, CodecParseError>

tst_core::codec::h265
  parse_vps(rbsp)         → Result<H265Vps, CodecParseError>
  parse_sps(rbsp)         → Result<H265Sps, CodecParseError>
  parse_pps(rbsp)         → Result<H265Pps, CodecParseError>
  parse_parameter_sets(nals) → Result<H265ParameterSets, CodecParseError>
```

The demuxer event surface is raw-first: `DemuxEvent::Sample` carries the
encoded AU on `SamplePayload::Video.raw` regardless of whether you intend to
parse parameter sets. Consumers call `split_video` + the parsers explicitly
when they need typed fields.

## H.264 quick start

```rust,no_run
use tst_core::codec::h264;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video};

let mut dx = Demuxer::new();
// ... feed bytes ...

while let Some(ev) = dx.next_event() {
    if let DemuxEvent::Sample {
        payload: SamplePayload::Video { codec: codec @ VideoCodec::H264, raw, av1_carriage, .. },
        ..
    } = ev
    {
        // Raw-first: split the AU into NAL units (opt-in). The issue list
        // carries any ES-conformance findings; dropped here.
        let (VideoPayload::Nals(nals), _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default()) else {
            continue;
        };
        // parse_parameter_sets is partial-success-tolerant: bad NALs emit
        // tracing::warn! and are skipped. Returns Err only if every
        // parameter-set NAL failed. Non-SPS/PPS NALs are silently skipped,
        // so calling this on a P-frame returns Ok with empty maps.
        if let Ok(ps) = h264::parse_parameter_sets(&nals) {
            if let Some(sps) = ps.sps_by_id.values().next() {
                println!(
                    "H.264 {}x{} profile={} level={} {}-bit {:?}",
                    sps.width,
                    sps.height,
                    sps.profile_idc,
                    sps.level_idc,   // x10 — level 4.0 is stored as 40
                    sps.bit_depth_luma,
                    sps.chroma_format,
                );
                if let Some(c) = &sps.color {
                    println!("  primaries={:?} transfer={:?}", c.primaries, c.transfer);
                }
                if let Some(fps) = sps.frame_rate {
                    println!("  fps={}/{}", fps.num, fps.den);
                }
            }
        }
    }
}
```

**`H264Sps` key fields:**

| Field | Type | Notes |
| --- | --- | --- |
| `width` / `height` | `u32` | Decoded from `pic_width_in_mbs_minus1` / `pic_height_in_map_units_minus1` minus the SPS frame-cropping offsets (`crop_left` / `crop_right` / `crop_top` / `crop_bottom`). |
| `profile_idc` | `u8` | 66 = Baseline, 77 = Main, 100 = High, 110 = High 10, etc. |
| `level_idc` | `u8` | `level × 10` — level 4.0 is stored as 40. |
| `bit_depth_luma` / `bit_depth_chroma` | `u8` | 8 for most streams; 10 for HDR. |
| `chroma_format` | `ChromaFormat` | `Yuv420` / `Yuv422` / `Yuv444` / `Monochrome`. |
| `color` | `Option<ColorInfo>` | VUI color information (`primaries`, `transfer`, `matrix`, `full_range`, `chroma_loc`, `sample_aspect_ratio`). `None` when the VUI signals none of video signal type, chroma location or aspect ratio. |
| `frame_rate` | `Option<Rational>` | Derived from VUI `timing_info` (`num_units_in_tick` + `time_scale`). `None` when absent. |
| `raw_rbsp` | `Vec<u8>` | The input bytes verbatim (emulation-prevention bytes included). |

**`H264Pps` key fields:** `pic_parameter_set_id`, `seq_parameter_set_id`, `entropy_coding_mode` (`Cavlc` or `Cabac`), `raw_rbsp`.

## H.265 quick start

```rust,no_run
use tst_core::codec::h265;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video};

let mut dx = Demuxer::new();
// ... feed bytes ...

while let Some(ev) = dx.next_event() {
    if let DemuxEvent::Sample {
        payload: SamplePayload::Video { codec: codec @ VideoCodec::H265, raw, av1_carriage, .. },
        ..
    } = ev
    {
        let (VideoPayload::Nals(nals), _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default()) else {
            continue;
        };
        if let Ok(ps) = h265::parse_parameter_sets(&nals) {
            if let Some(sps) = ps.sps_by_id.values().next() {
                println!(
                    "H.265 {}x{} profile_idc={} level_idc={} {}-bit {:?}",
                    sps.width,
                    sps.height,
                    sps.general_profile_idc,
                    sps.general_level_idc,   // x30 — level 4.0 is stored as 120
                    sps.bit_depth_luma,
                    sps.chroma_format,
                );
            }
        }
    }
}
```

**`H265Vps` key fields:** `vps_video_parameter_set_id`, `general_profile_idc`,
`general_tier_flag`, `general_level_idc`.

**`H265Sps` key fields:** same width / height / bit_depth / chroma_format /
color / frame_rate shape as `H264Sps`, plus `general_profile_idc`,
`general_tier_flag`, `general_level_idc`, `max_sub_layers_minus1`.
`general_level_idc` is `level × 30` — level 4.0 is stored as 120.

**`H265Pps` key fields:** `pps_pic_parameter_set_id`, `pps_seq_parameter_set_id`.

## H.266 / VVC quick start

H.266 (Versatile Video Coding) parses identically in shape to H.265 — VPS, SPS,
PPS — with a different bitstream syntax. PMT `stream_type = 0x33`.

```rust,no_run
use tst_core::codec::h266;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video};

let mut dx = Demuxer::new();
// ... feed bytes ...

while let Some(ev) = dx.next_event() {
    if let DemuxEvent::Sample {
        payload: SamplePayload::Video { codec: codec @ VideoCodec::H266, raw, av1_carriage, .. },
        ..
    } = ev
    {
        let (VideoPayload::Nals(nals), _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default()) else {
            continue;
        };
        if let Ok(ps) = h266::parse_parameter_sets(&nals) {
            if let Some(sps) = ps.spses.first() {
                println!(
                    "H.266 {}x{} profile_idc={} tier={} level_idc={} {}-bit {:?}",
                    sps.width,
                    sps.height,
                    sps.profile_tier_level.general_profile_idc,
                    sps.profile_tier_level.general_tier_flag as u8,
                    sps.profile_tier_level.general_level_idc,
                    sps.bit_depth_luma,
                    sps.chroma_format,
                );
            }
        }
    }
}
```

**`H266Vps` key fields:** `vps_id`, `max_layers`, `max_sub_layers`.

**`H266Sps` key fields:** `sps_id`, `vps_id`, `profile_tier_level`
(`general_profile_idc` / `general_tier_flag` / `general_level_idc`),
`width`, `height`, `chroma_format`, `bit_depth_luma`, `bit_depth_chroma`,
`color_info: Option<ColorInfo>`, `frame_rate: Option<Rational>`, `raw_rbsp`.

**`H266Pps` key fields:** `pps_id`, `sps_id`.

### Known limitations

- VPS + SPS + PPS only; APS NALs (types 17 / 18), Picture Header NALs
  (type 19), and multi-layer streams (`nuh_layer_id != 0`) pass through
  unparsed.
- Bails `CodecParseError::UnsupportedProfile` on `sps_subpic_info_present_flag = 1`
  and `sps_explicit_scaling_list_enabled_flag = 1` (rare; not in reference
  encoder defaults).
- `color_info` comes from the VUI and is `None` when
  `sps_vui_parameters_present_flag` is unset; `frame_rate` comes from
  `general_timing_hrd_parameters()` (H.266 moved timing out of the VUI) and
  is `None` when no timing/HRD parameters are present.
- See [project/deferred-features.md](/docs/project/deferred-features.md).

## AV1 quick start

AV1 has different bitstream framing — OBU (Open Bitstream Unit)
length-prefixed via LEB128, no Annex-B start codes. PMT `stream_type = 0x06`
with auto-emitted AV01 `registration_descriptor` per the AV1-in-MPEG-2-TS
binding §2.1. For an AV1 AU, `split_video(&raw, codec, av1_carriage.unwrap_or_default())` returns
`VideoPayload::Obus(Vec<Obu>)` rather than `Nals(_)` (and reverses the
`ts_open_bitstream_unit()` binding framing along the way).

The sample's `av1_carriage` field names the carriage the demuxer was
configured for (`DemuxerConfig::av1_carriage`; `None` for non-AV1 streams). `Av1CarriageMode::Mpeg2TsBinding`
(the default) is the conformant binding: PES `stream_id = 0xBD` and
`ts_open_bitstream_unit()` framing. `Av1CarriageMode::InteropRawObu` matches
the ffmpeg / hls.js / mediamtx toolchain: PES `stream_id = 0xE0` and raw OBUs
with no framing. Muxer and demuxer must be configured with the same mode.

```rust,no_run
use tst_core::codec::av1;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video};

let mut dx = Demuxer::new();
// ... feed bytes ...

while let Some(ev) = dx.next_event() {
    if let DemuxEvent::Sample {
        payload: SamplePayload::Video { codec: codec @ VideoCodec::Av1, raw, av1_carriage, .. },
        ..
    } = ev
    {
        let (VideoPayload::Obus(obus), _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default()) else {
            continue;
        };
        let stream = av1::parse_obu_stream(&obus);
        if let Some(seq) = stream.sequence_headers.first() {
            println!(
                "AV1 {}x{} profile={} level={} tier={} {}-bit {:?}",
                seq.max_frame_width,
                seq.max_frame_height,
                seq.profile,
                seq.level,
                seq.tier,
                seq.bit_depth,
                seq.chroma_format,
            );
        }
    }
}
```

**`Av1SequenceHeader` key fields:** `profile`, `level`, `tier`,
`max_frame_width`, `max_frame_height`, `bit_depth`, `monochrome`,
`chroma_format`, `still_picture`, `reduced_still_picture_header`,
`color_info: Option<ColorInfo>`, `frame_rate: Option<Rational>`, `raw`.

**`Av1FrameHeaderLight` key fields:** `frame_type` (0 = KEY,
1 = INTER, 2 = INTRA_ONLY, 3 = SWITCH), `show_frame`,
`show_existing_frame`, `frame_size: Option<(u32, u32)>`. The size field
is always `None` in this slice — full per-frame decode would require
reference-frame management beyond the parser's scope.

**`Av1ObuStream` (returned by `parse_obu_stream`)** holds the
`sequence_headers` and `frame_headers` collected from the input OBUs, plus
`unparseable` (`(obu_type, CodecParseError)` for each OBU that failed). Use it
when you want a single call against an AU's OBU list rather than walking
individual OBUs.

### Known limitations

- Sequence Header + Frame Header light scope; full Frame Header parsing
  crosses into "you want a decoder."
- Operating points beyond 0 are walked past but not surfaced.
- Temporal Delimiter / Tile Group / Metadata / Padding OBUs stay plain
  `Obu { obu_type, payload, .. }` values; `parse_obu_stream` skips them.
- See [project/deferred-features.md](/docs/project/deferred-features.md).

## Error handling

All parse functions return `Result<T, CodecParseError>`. The two tiers:

- **`parse_parameter_sets`** is partial-success-tolerant. If some NALs parse
  correctly and some don't, the correctly-parsed ones fill the output maps and
  bad NALs emit `tracing::warn!` and are skipped. The function only returns
  `Err` if every parameter-set NAL in the input failed.
- **`parse_sps` / `parse_pps` / `parse_vps`** are strict: they return `Err` on
  the first parsing failure.

`CodecParseError` carries a human-readable description. The most common variant
encountered for HDR/Main-10 SPSes is `CodecParseError::EngineError` (the
H.265 SPS parser bails when it reaches `scaling_list_data()`, H.265 §7.3.4,
which is not yet walked — a parser gap, not a profile-level rejection;
the short-term reference-picture-set walker does handle
`num_short_term_ref_pic_sets > 0`). Other variants: `TruncatedRbsp`,
`InvalidGolomb`, `ReservedValue`, `Forbidden`.

## Calling on non-IDR samples

`parse_parameter_sets` is safe to call on every `Sample` event, including
P-frames and B-frames. Non-SPS/PPS NALs are silently skipped. The return
value for a P-frame is `Ok(H264ParameterSets { sps_by_id: {}, pps_by_id: {} })`.
No warnings are emitted.

## Cross-frame tracking

Encoders typically emit SPS + PPS on every IDR. To log only when the stream
configuration changes, maintain a per-PID snapshot and compare:

```rust,no_run
use std::collections::HashMap;
use tst_core::codec::h264;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video};

let mut last_summary: HashMap<u16, String> = HashMap::new();
let mut dx = Demuxer::new();

// (feed bytes and call drain_events in your real loop)

fn drain_events(dx: &mut Demuxer, last: &mut HashMap<u16, String>) {
    while let Some(ev) = dx.next_event() {
        let DemuxEvent::Sample {
            stream,
            payload: SamplePayload::Video { codec: codec @ VideoCodec::H264, raw, av1_carriage, .. },
            ..
        } = ev
        else {
            continue;
        };
        let (VideoPayload::Nals(nals), _issues) = split_video(&raw, codec, av1_carriage.unwrap_or_default()) else {
            continue;
        };
        if let Ok(ps) = h264::parse_parameter_sets(&nals) {
            if let Some(sps) = ps.sps_by_id.values().next() {
                let summary = format!(
                    "{}x{} profile={} level={}",
                    sps.width, sps.height, sps.profile_idc, sps.level_idc
                );
                let pid = stream.pid;
                if last.get(&pid) != Some(&summary) {
                    println!("[PID 0x{pid:04X}] {summary}");
                    last.insert(pid, summary);
                }
            }
        }
    }
}
```

See the full runnable form at
[`examples/codec-parsing/parse_video_parameters.rs`](/examples/codec-parsing/parse_video_parameters.rs).

## Decoder replay — reconstituting Annex B bytes

The `raw_rbsp` field on each parsed struct preserves the RBSP body bytes
verbatim, including emulation-prevention bytes, exactly as handed to the
parser. **It does not include the NAL header byte(s)** — the demuxer strips
those before parsing (H.264: 1 header byte; H.265/H.266: 2 header bytes),
same as `NalUnit::{H264,H265,H266}.payload`. To reconstitute Annex-B-framed
parameter set bytes for a decoder initialization sequence, the header must be
rebuilt from the NAL's type/ref_idc (H.264) or type/layer_id/temporal_id_plus1
(H.265) fields before the RBSP is appended:

**H.264 SPS / PPS:**

```rust,no_run
fn to_annex_b_h264(nal_type: u8, ref_idc: u8, rbsp: &[u8]) -> Vec<u8> {
    // H.264 NAL units use a 4-byte start code, then a 1-byte header
    // rebuilt from nal_type/ref_idc (H.264 §7.3.1), then the RBSP.
    // The demuxer strips this header byte before handing back
    // `NalUnit::H264.payload` / `raw_rbsp` — it is not part of either.
    let header = ((ref_idc & 0x03) << 5) | (nal_type & 0x1F);
    let mut out = vec![0x00, 0x00, 0x00, 0x01, header];
    out.extend_from_slice(rbsp);
    out
}
```

**H.265 VPS / SPS / PPS:**

```rust,no_run
fn to_annex_b_h265(nal_type: u8, layer_id: u8, temporal_id_plus1: u8, rbsp: &[u8]) -> Vec<u8> {
    // H.265 NAL units have a 2-byte header (H.265 §7.3.1.2), also
    // stripped by the demuxer before handing back `NalUnit::H265.payload`
    // / `raw_rbsp`.
    let h0 = ((nal_type & 0x3F) << 1) | ((layer_id >> 5) & 0x01);
    let h1 = ((layer_id & 0x1F) << 3) | (temporal_id_plus1 & 0x07);
    let mut out = vec![0x00, 0x00, 0x00, 0x01, h0, h1];
    out.extend_from_slice(rbsp);
    out
}
```

Both prepend a 4-byte start code, but H.264 rebuilds a 1-byte header while
H.265 rebuilds a 2-byte header (nal_unit_type + layer_id + temporal_id_plus1).
See the full worked version — reconstructing Annex B for every `NalUnit`
variant (H.264/H.265/H.266) coming out of `split_video` — at
[`examples/codec-parsing/extract_video_au.rs`](/examples/codec-parsing/extract_video_au.rs).

**Skip the manual reconstruction:** if all you need is parameter-set NALs for
a decoder init call (e.g. Apple VideoToolbox's
`CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets`),
`tst_core::codec::nal_framing::extract_parameter_sets(annexb: &[u8], codec: VideoCodec) -> ParameterSets`
scans a `Sample`'s raw Annex-B bytes directly — the same `raw` field passed to
`split_video`, no need to call it first — and returns VPS/SPS/PPS as complete
NALs with the header byte(s) already included.

## Shared types

`tst_core::codec` re-exports several types used by both the H.264 and H.265
modules:

| Type | Description |
| --- | --- |
| `ChromaFormat` | `Monochrome` / `Yuv420` / `Yuv422` / `Yuv444` |
| `Rational` | `{ num: u32, den: u32 }` — frame rate numerator / denominator |
| `ColorInfo` | `{ primaries, transfer, matrix, full_range, chroma_loc, sample_aspect_ratio }` — H.273-faithful decoded enums plus range, chroma location and pixel aspect ratio |
| `ColourPrimaries` | BT.709, BT.2020, DCI-P3, Unspecified, … (full ITU-T H.273 table) |
| `TransferCharacteristics` | BT.709, SMPTE ST 2084 (PQ), HLG, IEC 61966-2-1 (sRGB), … |
| `MatrixCoefficients` | BT.601, BT.709, BT.2020 NCL/CL, Identity, … |
| `CodecParseError` | Shared error type for all codec parsers |

The color enum decoders are verified against BT.2020 + PQ HDR fixtures to
ensure the numeric code points match the ITU-T H.273 Table 2 / Table 3 /
Table 4 assignments.

## Roadmap

`tst_core::codec` is an umbrella for typed payload parsing across codec and
stream types. H.264, H.265, H.266, and AV1 parameter-set parsers ship today.
Future slices in the same umbrella (each landing additively when a consumer
asks):

- **H.266 APS / Picture Header** — APS NALs (types 17 / 18) and Picture
  Header NALs (type 19) pass through unparsed today; typed surfaces follow
  on consumer ask.
- **AV1 full Frame Header** — current `parse_frame_header_light` extracts
  type / show flags only; per-frame size + reference-frame management is a
  decoder-scope expansion.
- **SEI parsing** for H.264 and H.265 — HDR mastering display info, content
  light level, picture timing, etc.
- **AAC-LATM full decode** (`codec::aac::latm`) — the LOAS sync + length
  validator ships today (see "AAC LATM + AC-3" below); typed per-frame
  decode is deferred. (MP2, AAC ADTS, and AC-3 frame parsing all ship.)
- **Heuristic payload-kind detection** (`codec::detect`) — looks-like-ADTS /
  looks-like-UL+BER / looks-like-H.264 helpers for `Unknown` / private streams.

## Audio frame parsing

The MPEG-TS demuxer surfaces `SamplePayload::Audio { codec, frames }`
events carrying the raw PES payload bytes (`frames` is a `SharedBytes`, which
derefs to `&[u8]`). The `codec::mpegaudio` and `codec::aac`
modules parse those bytes into typed per-frame metadata (sample rate, channel
count, layer/profile, frame size) without decoding audio content.

Both modules expose the same shape: `fn frames(bytes) -> impl Iterator<Item =
Result<Frame, CodecParseError>>`. The iterator advances by header-decoded
`frame_length_bytes` and ends on first error or buffer end.

### `codec::mpegaudio` — MPEG-1 / MPEG-2 / MPEG-2.5 Layer I/II/III

```rust
use tst_core::codec;
use tst_core::mpegts::demux::{AudioCodec, SamplePayload};

if let SamplePayload::Audio { codec: AudioCodec::Mp2, frames, .. } = payload {
    for frame in codec::mpegaudio::frames(&frames) {
        let f = frame?;
        println!("layer={:?} version={:?} sample_rate={} channels={} bitrate_kbps={}",
            f.layer, f.version, f.sample_rate_hz, f.channels, f.bitrate_kbps);
    }
}
```

`AudioCodec::Mp2` covers stream_type `0x03` and `0x04`, which spans Layer I,
II, and III at all version × sample_rate combinations. The frame iterator
recovers the actual layer and version from each header.

### `codec::aac` — AAC ADTS

```rust
if let SamplePayload::Audio { codec: AudioCodec::Aac, frames, .. } = payload {
    for frame in codec::aac::frames(&frames) {
        let f = frame?;
        // `channels()` is `None` for a PCE-defined layout (channel_configuration 0).
        println!("profile={:?} sample_rate={} channels={:?} blocks={} samples_per_frame={}",
            f.profile, f.sample_rate_hz, f.channels(),
            f.num_raw_data_blocks, f.samples_per_frame);
    }
}
```

Note: the ADTS `profile` field is a legacy MPEG-2 AAC concept. Most real-world
ADTS streams encode AAC-LC (`profile = Lc`) regardless of which MPEG-4 audio
object type the encoder used. Streams using HE-AAC, HE-AACv2, or other MPEG-4
AOTs are usually carried in LATM, not ADTS.

### Resync — `frames_with_resync()`

Every audio module ships two iterator entry points:

```rust,ignore
codec::mpegaudio::frames(&bytes)             // strict: first parse error ends iteration
codec::mpegaudio::frames_with_resync(&bytes) // best-effort: skip garbage, find the next valid frame
codec::aac::frames(&bytes)                   // (same pair on aac, ADTS framing)
codec::aac::frames_with_resync(&bytes)       //
```

`frames_with_resync` walks past unparsable bytes until it finds the next
valid header. Use it whenever you're populating stats from possibly-
corrupted PES payloads (the demuxer's per-stream audio frame counters use
it, so one malformed frame mid-PES does not drop the rest of the count). Use the strict `frames()` form when feeding
known-good test fixtures or when any parse error should abort the loop.

`CodecParseError::UnsupportedFreeFormat { layer }` is distinct from
`ReservedValue` — `bitrate_index == 0` (free-format) is *legal* per
ISO/IEC 11172-3 but not supported by this parser; the resync iterator
treats it as a recoverable error and continues past the affected frame.

### What the parsers don't do

- **No CRC verification.** `has_crc` is surfaced; the CRC bytes are consumed
  for offset accounting but not validated. Callers wanting verification can
  use `frame.bytes()` to access the full frame slice and run their own CRC.
- **No raw_data_block enumeration for multi-block AAC frames.** The frame is
  surfaced as one `AdtsFrame` with `samples_per_frame = 1024 *
  num_raw_data_blocks`. Splitting into individual blocks is decoder territory.

### AAC LATM + AC-3

`codec::aac::latm` validates LOAS syncword (`0x2B7` 11-bit pattern) and
`audioMuxLengthBytes` per ISO/IEC 14496-3 §1.7. `codec::ac3::parse_syncframe`
parses A/52 §5.4.1 syncframes (sync word `0x0B77`, bsid, frame size,
sample rate, channel layout).

See `docs/project/deferred-features.md` for any remaining audio surface that
hasn't shipped yet.

## See also

- **Runnable example:** `cargo run -p tst-examples --example parse_video_parameters` — [examples/codec-parsing/parse_video_parameters.rs](/examples/codec-parsing/parse_video_parameters.rs)
- **Runnable example:** `cargo run -p tst-examples --example parse_audio_frames` — [examples/codec-parsing/parse_audio_frames.rs](/examples/codec-parsing/parse_audio_frames.rs)
- [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — where the NAL units come from in the first place.
- [reference/architecture.md](/docs/reference/architecture.md) — how `tst-core::codec` fits into the wider crate graph.
