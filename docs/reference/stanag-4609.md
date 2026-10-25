# STANAG 4609 / MISP conformance

## What STANAG 4609 is

STANAG 4609 is the NATO standardization agreement that formally adopts the
Motion Imagery Standards Profile (MISP) for allied-nation interoperability.
Rather than defining a new wire format, the STANAG cites a specific frozen
MISP snapshot as the conformance baseline. **Edition 5 (2020) binds to
MISP-2019.1**, which in turn requires a particular subset of MISB standards:
MPEG-TS carriage per ST 1402, UAS Datalink metadata per ST 0601, security
metadata per ST 0102, time-stamping per ST 0603/0604/0605, and motion imagery
sensor minimum metadata per ST 0902, among others.

The library is a muxer, demuxer and KLV codec, not a complete Motion Imagery
system, so STANAG 4609 conformance belongs to the stream you build with it, not
to the library alone. The sections below list the requirements the library
meets and the ones that remain the caller's. The library tracks the MISB
standard editions named in the [conformance matrix](#conformance-matrix), which
can be newer than the editions MISP-2019.1 cites. Its output has not been run
through ISO/IEC 13818-4 conformance testing, which MISP-2019.1 requires of
Class 1 Motion Imagery in an MPEG-2 Transport Stream (MISP-2015.1-50).

### What the library does

- Carries H.264 / H.265 video and synchronous or asynchronous KLV in MPEG-TS
  per ST 1402, with the PMT descriptors and AU cell wrapping listed in the
  matrix.
- Writes the ST 0604 Precision Time Stamp (or, on H.265, the Nano Precision
  Time Stamp) as a `user_data_unregistered` SEI in front of the first VCL NAL
  when you push through `Muxer::push_video_misp_to`,
  `MuxSender::send_video_misp_to` or `MuxPublisher::push_video_misp`.
- Encodes and decodes the KLV sets in the matrix, and checks one ST 0601
  record against the ST 0902 Minimum Metadata Set with `validate_mismms`.

### What the caller must supply

- **An absolute-time timestamp in every frame.** MISP-2019.1 makes this
  mandatory (MISP-2018.3-116, and MISP-2018.1-104 for Class 1 via ST 0604).
  The ordinary `push_video*` / `send_video*` calls pass the access unit through
  unchanged, so either your encoder already writes the ST 0604 SEI or every
  frame goes through one of the MISP variants above. A PES PTS or a KLV Tag 2
  does not timestamp the imagery.
- **A valid ST 0603 Time Status byte.** The library writes the byte you pass
  unchanged. Bits 4–0 are reserved and must be `11111`, and bit 7 is `0` when
  the clock is locked: `0x1F` = locked, `0x9F` = lock unknown (both with
  normal, forward time).
- **One timestamp representation per stream** (MISP-2018.1-98): micro or nano,
  never both.
- **The Minimum Metadata Set** (MISP-2015.1-75): every
  ST 0902 item at least once every 30 seconds, including the Security Local
  Set (Tag 48) and the Core Identifier (Tag 94, which MISP-2015.1-68 also
  requires on its own). `validate_mismms` checks one record; the cadence
  across records is yours to keep.
- **Synchronous KLV declarations that match the payload.** Put all
  synchronous metadata in one synchronous metadata stream per program
  (ST 1402-13; the muxer accepts a second one), and keep each AU cell's
  `metadata_service_id` equal to a service the PMT declares (ST 1402-15).
  The default service 0 matches the `metadata_descriptor` the muxer emits;
  the muxer does not cross-check a service id you choose.
- **A regular push cadence.** PAT/PMT and PCR are written only as part of a
  push, so a source that pushes less often than every 100 ms cannot meet the
  H.222.0 PCR interval.

### Limits of the claim

- The PCR is derived from the PTS being pushed, so a decoder gets no lead
  between a PCR and the presentation time that follows it.
- The H.265 MISP SEI is written with `nuh_temporal_id_plus1 = 1` (TemporalId
  0) on every frame, which an encoder using temporal sub-layers above 0 does
  not match.
- The auto-emitted `metadata_std_descriptor` declares zero leak rates and a
  zero buffer size; supply your own descriptor (tag 0x27) if your receiver
  needs real STD values.

---

## Conformance matrix

| Requirement area | Spec | Library support | Notes |
|---|---|---|---|
| MPEG-TS carriage | MISB ST 1402.2 | ✅ Supported | Async KLV (`stream_type 0x06` + KLVA descriptor) and synchronous KLV (`stream_type 0x15`); `metadata_descriptor` and `metadata_std_descriptor` auto-emitted in the PMT per §9.4.1; H.222.0 §2.12.4.2 AU cell wrapping. See [KLV guide](/docs/guides/klv.md). |
| KLV metadata sets | ST 0601.19 · ST 0102.12 · ST 0903.6 · ST 0605.10 · ST 0806.4 · ST 1010.3 | ✅ Supported | 142 of 143 ST 0601.19 items typed or structured (Item 66 deprecated stays unknown-passthrough by design); nested-set items carried byte-faithfully with sibling typed decoders (ST 0102, ST 0903, ST 0806, ST 1204, ST 1010 SDCC). See [KLV guide](/docs/guides/klv.md). |
| Time system — ST 0603 Time Status byte | MISB ST 0603.5 | ✅ Supported | `TimeStatus` newtype with `is_locked` / `has_discontinuity` / `is_reverse_jump` accessors; surfaced via the ST 0605 Precision Time Stamp Pack (`klv::st0605`). |
| Video timestamps (MISP SEI) | MISB ST 0604.6 | ✅ Supported | Required in every frame. `Muxer::push_video_misp_to` / `MuxSender::send_video_misp_to` / `MuxPublisher::push_video_misp` splice a `user_data_unregistered` SEI NAL into the access unit; the ordinary push calls do not, so use these unless your encoder already writes the SEI. `codec::misp_time::extract` recovers the timestamp on the receive side. All four language bindings. See [per-language snippets](#per-language-snippets) below. |
| Minimum Metadata Set compliance validator | MISB ST 0902.8 | ✅ Supported | `klv::st0601::validate_mismms` checks one record against the 23 items of ST 0902.8 Table 1 (with the alternation groups and the Security Local Set sub-items) and returns a `Vec<MismmsViolation>` naming every unmet item. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). See [per-language snippets](#per-language-snippets) below. |
| Core Identifier (MIIS) | MISB ST 1204.3 | ✅ Supported | `klv::st1204::{decode, encode_to_vec}` encode/decode the Core ID: a version byte, a usage byte and one to three 16-byte UUIDs (18, 34 or 50 bytes). ST 0601 Tag 94 (`miis_core_id`) carries it in the KLV stream, and the ST 0903 VMTI Local Set carries it in Tag 13 (`VmtiLs::miis_id`). `klv::st1204::CoreId` / `IdType` are the typed model. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| RVT (Remote Video Terminal) | MISB ST 0806.4 | ✅ Supported | `klv::st0806` — RVT LS + POI/AOI/User-Defined nested LSs; embedded via ST 0601 Tag 73 and standalone UL form. CRC-32/MPEG-2 (Tag 1): the standalone form carries it (verified on decode); the embedded form is not required to carry it. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| KLV → Cursor-on-Target (CoT) | MISB ST 0805.1 | ✅ Supported | `klv::st0805` conversion layer — Platform Position and Sensor Point of Interest CoT events generated from a decoded ST 0601 LS. The caller passes `generated_us` explicitly and attribute order is fixed, so output is byte-stable for replay/testing. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| SDCC error covariance (SDCC-FLP) | MISB ST 1010.3 | ✅ Supported | `klv::st1010::{decode_sdcc_flp, encode_sdcc_flp_mode2}` — Mode 1 (foreign/defensive parse) and Mode 2 (encode) + sparse bit-vector; general-purpose, also used by ST 0601 Tag 102 (`sdcc_flps`). Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| Video coding — H.264 | MISB RP 0802.2 | ✅ Carriage supported | Annex-B NAL push (`push_video_to`); SPS/PPS parameter-set parsing (`codec::h264`). Encoding is the caller's responsibility (out of library scope). |
| Video coding — H.265 | H.265 / HEVC | ✅ Carriage supported | Annex-B NAL push; VPS/SPS/PPS parsing (`codec::h265`). Encoding is the caller's responsibility. |
| Commercial Time Stamp (HH:MM:SS:FF timecode) | MISB ST 0604.6 §9, §11.2, §12.3 | ⏳ Deferred | No consumer has requested the timecode carried in the H.264 `pic_timing` SEI (payloadType 1) or the H.265 `time_code` SEI (payloadType 136). See [deferred-features.md](/docs/project/deferred-features.md). |
| H.262 timestamps | MISB ST 0604.6 §10 | ❌ Deferred | H.262 (MPEG-2 Video) carriage is not planned. See [deferred-features.md](/docs/project/deferred-features.md). |
| AV1 / H.266 MISP SEI splice | ST 0604.6 (future) | ⏳ Deferred | The MISP SEI splice is implemented for H.264 and H.265 only; AV1 and H.266 do not yet have a standardized MISP timestamp SEI payload. See [deferred-features.md](/docs/project/deferred-features.md). |
| Legacy EG 0104 metadata | MISB EG 0104 | ❌ Deferred | The EG 0104 "Predator" metadata format predates ST 0601 and is not in any planned consumer's workflow. See [deferred-features.md](/docs/project/deferred-features.md). |
| MISMMS cadence tracker | ST 0902.8 | ⏳ Deferred | `validate_mismms` is a per-record snapshot check. A stream-level tracker checking the 30-second MISMMS reporting cadence across records is not yet implemented; the separate ST 1204 Core ID stream requirement is also outside the per-record validator. See [deferred-features.md](/docs/project/deferred-features.md). |

---

## Per-language snippets

The four short examples below show the MISP timestamp push and the MISMMS
validator call. Each snippet uses the real function signatures — see the linked
guides for full context on muxer setup and handle acquisition.

### Rust

```rust
use tst_core::codec::misp_time::MispTimestamp;
use tst_core::klv::st0601::validate_mismms;
use tst_core::mpegts::mux::VideoStreamHandle;

// Build a microsecond-precision MISP timestamp (H.264 or H.265).
// time_status carries the ST 0603 byte, written unchanged:
// 0x1F = locked, 0x9F = lock unknown (normal, forward time; bits 4-0 reserved = 11111).
let misp = MispTimestamp::micros(system_time_us, /*time_status=*/ 0x1F);

// Splice the MISP SEI into the access unit and push to the muxer.
muxer.push_video_misp_to(handle, &nal, pts, /*key_frame=*/ true, &misp)?;

// On the receive side, validate that a decoded KLV record satisfies ST 0902.
let violations = validate_mismms(&uas_datalink_ls);
if !violations.is_empty() {
    eprintln!("MISMMS violations: {:?}", violations);
}
```

### Python

```python
from tstrans.codec import MispTimestamp, extract_misp_timestamp
from tstrans.mpegts import VideoCodec
from tstrans.klv import validate_mismms

# Build a microsecond-precision MISP timestamp (H.264 or H.265).
# time_status is the ST 0603 byte (0x1F = locked, 0x9F = lock unknown).
misp = MispTimestamp.micros(system_time_us, time_status=0x1F)

# Splice the MISP SEI and push to the muxer.
# dts=None uses pts as DTS (no B-frame reordering).
muxer.push_video_misp_to(handle, nal, pts=pts, dts=None, key_frame=True, misp=misp)

# Extract a MISP timestamp from a received access unit (returns None when absent).
ts = extract_misp_timestamp(au_bytes, VideoCodec.H264)

# Validate a decoded KLV record against ST 0902.
violations = validate_mismms(uas_datalink_ls)
for v in violations:
    print("MISMMS violation:", v)
```

### Java (JVM)

```java
import org.tstrans.codec.MispTimestamp;
import org.tstrans.klv.Klv;
import org.tstrans.klv.UasDatalinkLs;
import org.tstrans.klv.MismmsViolation;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.mpegts.VideoStreamHandle;

// Build a microsecond-precision MISP timestamp (H.264 or H.265).
// timeStatus is the ST 0603 byte as an int (0x1F = locked, 0x9F = lock unknown).
MispTimestamp misp = MispTimestamp.micros(systemTimeUs, /*timeStatus=*/ 0x1F);

// Splice the MISP SEI into the access unit and push to the muxer.
muxer.pushVideoMispTo(handle, nal, pts, /*keyFrame=*/ true, misp);

// Extract a MISP timestamp from a received access unit (returns null when absent).
MispTimestamp ts = MispTimestamp.extract(auBytes, VideoCodec.H264);

// Validate a decoded KLV record against ST 0902.
List<MismmsViolation> violations = Klv.validateMismms(uasDatalinkLs);
if (!violations.isEmpty()) {
    System.err.println("MISMMS violations: " + violations);
}
```

### C

```c
#include "tstrans.h"

/* Splice a MISP SEI into the access unit and push to the muxer.
   misp_kind=0 → Precision Time Stamp (microseconds).
   misp_kind=1 → nanosecond precision (H.265-only per ST 0604.6 §12.2; returns TST_E_MISP_TIME on H.264).
   time_status is the ST 0603 byte (0x1F = locked, 0x9F = lock unknown).
   Returns 0 on success; call tst_get_last_error() on failure. */
int rc = tst_muxer_push_video_misp_to(
    muxer, handle,
    nal, nal_len,
    pts_90khz,  /* 90 kHz PTS ticks */
    /*key_frame=*/ 1,
    /*misp_kind=*/ 0,      /* 0 = microsecond */
    /*time_status=*/ 0x1F, /* ST 0603 locked */
    /*value=*/ system_time_us);

/* Extract a MISP timestamp from a received access unit.
   Returns 0 when found (out_* fields populated), 1 when absent,
   negative error code on malformed input. */
uint8_t out_kind, out_time_status;
uint64_t out_value;
int found = tst_misp_time_extract(
    au, au_len, TST_VIDEO_CODEC_H264,
    &out_kind, &out_time_status, &out_value);
```

---

## See also

- [Feature matrix](/docs/reference/compatibility.md) — full spec-by-spec implementation status
- [KLV guide](/docs/guides/klv.md) — encode and decode MISB typed metadata
- [MPEG-TS mux guide](/docs/guides/mpegts-mux.md) — muxer setup and stream handles
- [Deferred features](/docs/project/deferred-features.md) — items planned but not yet implemented
