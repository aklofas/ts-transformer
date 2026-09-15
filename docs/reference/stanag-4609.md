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

The `ts-transformer` library targets **current MISP** — the living profile
rather than the frozen 2019.1 snapshot — which is a superset of the STANAG
4609 Ed 5 requirements. This means every STANAG 4609 Ed 5 requirement that is
within the library's scope is satisfied, and additional features (such as ST
1204 Core Identifier support) from later MISP revisions are also available.

Consumers whose ground-station or recorder toolchain is contractually pinned to
STANAG 4609 Edition 5 (= MISP-2019.1) should note that the library's output satisfies
the in-scope requirements of that baseline. The extra MISP-post-2019.1 features the
library exposes — ST 0604 video timestamps, the ST 0902 MISMMS validator, and
ST 1204 Core Identifier — are **opt-in**: a stream produced with only the
base APIs meets the library's obligations under STANAG 4609 Ed 5 — end-to-end
conformance still depends on the caller supplying correct timestamps and required metadata.

---

## Conformance matrix

| Requirement area | Spec | Library support | Notes |
|---|---|---|---|
| MPEG-TS carriage | MISB ST 1402.2 | ✅ Supported | Async KLV (`stream_type 0x06` + KLVA descriptor) and synchronous KLV (`stream_type 0x15`); `metadata_descriptor` and `metadata_std_descriptor` auto-emitted in the PMT per §9.4.1; H.222.0 §2.12.4.2 AU cell wrapping. See [KLV guide](/docs/guides/klv.md). |
| KLV metadata sets | ST 0601.19 · ST 0102.12 · ST 0903.6 · ST 0605.10 · ST 0806.4 · ST 1010.3 | ✅ Supported | 142 of 143 ST 0601.19 items typed or structured (Item 66 deprecated stays unknown-passthrough by design); nested-set items carried byte-faithfully with sibling typed decoders (ST 0102, ST 0903, ST 0806, ST 1204, ST 1010 SDCC). See [KLV guide](/docs/guides/klv.md). |
| Time system — ST 0603 Time Status byte | MISB ST 0603.5 | ✅ Supported | `TimeStatus` newtype with `is_locked` / `has_discontinuity` / `is_reverse_jump` accessors; surfaced via the ST 0605 Precision Time Stamp Pack (`klv::st0605`). |
| Video timestamps (MISP SEI) | MISB ST 0604.6 | ⚙️ Opt-in | `Muxer::push_video_misp_to` / `MuxSender::send_video_misp_to` / `MuxPublisher::send_video_misp` splice a SMPTE-registered SEI NAL into the access unit. `codec::misp_time::extract` recovers the timestamp on the receive side. All four language bindings. See [per-language snippets](#per-language-snippets) below. |
| Minimum Metadata Set compliance validator | MISB ST 0902.8 | ⚙️ Opt-in | `klv::st0601::validate_mismms` returns a `Vec<MismmsViolation>` identifying which required ST 0902 fields are absent or out of range. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). See [per-language snippets](#per-language-snippets) below. |
| Core Identifier (MIIS) | MISB ST 1204.3 | ✅ Supported | `klv::st1204::{decode, encode_to_vec}` encode/decode the 16-byte UUID-backed Core ID; ST 0601 Tag 94 (`miis_core_id`) carries it in the KLV stream. `klv::st1204::CoreId` / `IdType` are the typed model. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| RVT (Remote Video Terminal) | MISB ST 0806.4 | ✅ Supported | `klv::st0806` — RVT LS + POI/AOI/User-Defined nested LSs; embedded via ST 0601 Tag 73 and standalone UL form. CRC-32/MPEG-2 (Tag 1): the standalone form carries it (verified on decode); the embedded form is not required to carry it. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| KLV → Cursor-on-Target (CoT) | MISB ST 0805.1 | ✅ Supported | `klv::st0805` conversion layer — Platform Position and Sensor Point of Interest CoT events generated from a decoded ST 0601 LS. The caller passes `generated_us` explicitly and attribute order is fixed, so output is byte-stable for replay/testing. Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| SDCC error covariance (SDCC-FLP) | MISB ST 1010.3 | ✅ Supported | `klv::st1010::{decode_sdcc_flp, encode_sdcc_flp_mode2}` — Mode 1 (foreign/defensive parse) and Mode 2 (encode) + sparse bit-vector; general-purpose, also used by ST 0601 Tag 102 (`sdcc_flps`). Rust, Python, and JVM (the C ABI types ST 0601 only — the `tst_st0601_*` family, ABI 0.21 — so this set is Rust / Python / JVM). |
| Video coding — H.264 | MISB RP 0802.2 | ✅ Carriage supported | Annex-B NAL push (`push_video_to`); SPS/PPS parameter-set parsing (`codec::h264`). Encoding is the caller's responsibility (out of library scope). |
| Video coding — H.265 | H.265 / HEVC | ✅ Carriage supported | Annex-B NAL push; VPS/SPS/PPS parsing (`codec::h265`). Encoding is the caller's responsibility. |
| Commercial Time Stamp (UTC wall-clock SEI) | MISB ST 0604.6 §7.3 | ⏳ Deferred | No consumer has requested the `payloadType=21` UTC wall-clock SEI family. See [deferred-features.md](/docs/project/deferred-features.md). |
| H.262 timestamps | MISB ST 0604.6 §10 | ❌ Deferred | H.262 (MPEG-2 Video) carriage is not planned. See [deferred-features.md](/docs/project/deferred-features.md). |
| AV1 / H.266 MISP SEI splice | ST 0604.6 (future) | ⏳ Deferred | The MISP SEI splice is implemented for H.264 and H.265 only; AV1 and H.266 do not yet have a standardized MISP timestamp SEI payload. See [deferred-features.md](/docs/project/deferred-features.md). |
| Legacy EG 0104 metadata | MISB EG 0104 | ❌ Deferred | The EG 0104 "Predator" metadata format predates ST 0601 and is not in any planned consumer's workflow. See [deferred-features.md](/docs/project/deferred-features.md). |
| MISMMS cadence tracker | ST 0902.8 | ⏳ Deferred | `validate_mismms` is a per-record snapshot check. A stream-level cadence tracker (verifying 1-Hz or better KLV delivery across multiple records) is not yet implemented. See [deferred-features.md](/docs/project/deferred-features.md). |

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
// time_status carries the ST 0603 byte — 0x00 = unsynchronised, 0x01 = locked.
let misp = MispTimestamp::micros(system_time_us, /*time_status=*/ 0x01);

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
# time_status is the ST 0603 byte (0x01 = locked).
misp = MispTimestamp.micros(system_time_us, time_status=0x01)

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
// timeStatus is the ST 0603 byte as an int (0x01 = locked).
MispTimestamp misp = MispTimestamp.micros(systemTimeUs, /*timeStatus=*/ 0x01);

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
   misp_kind=0 → microsecond precision (ST 0604 Class 0).
   misp_kind=1 → nanosecond precision (H.265-only per ST 0604.6 §12.2; returns TST_E_MISP_TIME on H.264).
   time_status is the ST 0603 byte (0x01 = locked).
   Returns 0 on success; call tst_get_last_error() on failure. */
int rc = tst_muxer_push_video_misp_to(
    muxer, handle,
    nal, nal_len,
    pts_90khz,  /* 90 kHz PTS ticks */
    /*key_frame=*/ 1,
    /*misp_kind=*/ 0,      /* 0 = microsecond */
    /*time_status=*/ 0x01, /* ST 0603 locked */
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
