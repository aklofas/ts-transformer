# Build a STANAG 4609-conformant stream

> **When to use this:** You are building a stream toward STANAG 4609 /
> MISP conformance and need the library-side pieces — ST 0604 per-frame timestamps,
> an ST 0902 minimum-metadata validator gate, strict-compliance KLV encoding,
> and a MIIS Core Identifier in Tag 94.

> **Related:**
> - [reference/stanag-4609.md](/docs/reference/stanag-4609.md) — conformance matrix, all four language snippets
> - [guides/klv.md](/docs/guides/klv.md) — AU-cell auto-wrap contract, `encode_strict_compliance`
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — muxer setup, stream handles, sync vs async KLV

STANAG 4609 Ed 5 (= MISP-2019.1) has four specific requirements that go
beyond a plain H.264 + KLV stream:

1. **ST 0604 video timestamps** — MISP requires an absolute-time timestamp
   in every frame. A `user_data_unregistered` SEI NAL placed ahead of the
   access unit's first VCL NAL carries the capture time as a Precision Time
   Stamp (microseconds). This is what `push_video_misp_to` /
   `send_video_misp_to` do; the ordinary video push calls leave the access
   unit unchanged.
2. **ST 0603 Time Status byte** — the SEI also embeds a byte that tells the
   receiver whether the clock is locked to an absolute time reference. Bit 7
   is `0` for locked and bits 4–0 are reserved as `11111`, so pass `0x1F`
   (locked) or `0x9F` (lock unknown) as `time_status` to
   `MispTimestamp::micros`; the library writes the byte unchanged.
3. **ST 0902 Minimum Metadata Set (MISMMS) compliance** — each of the 23
   items in ST 0902.8 Table 1 must be reported at least once every 30
   seconds. `validate_mismms` checks that one record carries all of them
   before you encode.
4. **ST 0601 strict-compliance encoding** — `encode_strict_compliance`
   requires Tag 2 (Precision Time Stamp) and cleans string fields per
   ST 0107, then encodes as `encode_to_vec` does, which already rejects
   values outside their ST 0601 wire range.

One of those items is the **MIIS Core Identifier** (ST 1204.3, ST 0601
Tag 94), which MISP also requires on its own. It lets a receiving archive
system correlate streams from the same physical sensor or platform.

## Rust

```rust,no_run
use std::time::SystemTime;
use tst_core::codec::misp_time::MispTimestamp;
use tst_core::klv::st0601::{UasDatalinkLs, encode_strict_compliance, validate_mismms};
use tst_core::klv::st1204::{CoreId, IdType, encode_to_vec as encode_core_id};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Build the muxer program: H.264 + sync-KLV (ST 1402 stream_type
    //       0x15, carries_pts=true so each KLV record gets a PES PTS).
    //
    //   "Sync" KLV is STANAG 4609's preferred shape: one KLV record per
    //   video frame, PES PTS = frame PTS. The muxer auto-prepends the 5-byte
    //   Metadata_AU_cell header (H.222.0 §2.12.4.2) — pass raw KLV LS bytes.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(
            0x1031,
            KlvStreamType::SynchronousMetadata, // stream_type 0x15, KLVA descriptor
            /*carries_pts=*/ true,              // each KLV PES gets a PTS
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };
    let mut muxer = Muxer::new(cfg)?;

    // ── 2. Acquire per-stream handles. The "0th" video/KLV stream is
    //       index 0. unwrap() is safe here because we just configured them.
    let vid_handle = muxer.video_stream_handle(0).unwrap();
    let klv_handle = muxer.klv_stream_handle(0).unwrap();

    // ── 3. Build a MIIS Core Identifier (ST 1204.3) for Tag 94.
    //
    //   The Core ID ties every frame in this stream to the same physical
    //   sensor.  Generate the UUID once at startup (use a stable UUID for a
    //   real platform — store it in config, not regenerated each run).
    let sensor_uuid: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
    ];
    // `CoreId` is `#[non_exhaustive]`: construct it with `CoreId::new`
    // (version, sensor, platform, window, minor).
    let core_id = CoreId::new(1, Some((IdType::Physical, sensor_uuid)), None, None, None);
    // Wire format: a sequence of 16-byte UUID blocks preceded by a 1-byte
    // version + 1-byte usage flag.  encode_to_vec handles the framing.
    let core_id_bytes = encode_core_id(&core_id);

    // ── Per-frame encode loop ────────────────────────────────────────────────

    // Capture time from your platform's GPS/PPS-disciplined clock.
    // MISB ST 0603 Time Status: 0x1F = locked, 0x9F = lock unknown
    // (normal, forward time; bits 4-0 reserved = 11111).
    let time_status: u8 = 0x1F; // locked

    // Simulated per-frame data — replace with real encoder/sensor output.
    let frame_pts_us: u64 = 1_700_000_000_000_000; // MISP timestamp (microseconds)
    // 90 kHz ticks from microseconds: ×9/100 keeps sub-second precision.
    // (u64 headroom: even year-2100 epoch-µs × 9 stays far below u64::MAX.)
    let frame_pts_90khz = Pts90khz::new((frame_pts_us * 9 / 100) as i64);
    let nal: Vec<u8> = vec![/* H.264 Annex-B NAL bytes from your encoder */];
    let is_key_frame = true;

    // ── 4. Splice the ST 0604 MISP SEI into the access unit.
    //
    //   MispTimestamp::micros builds the ST 0604 user_data_unregistered SEI payload
    //   (Precision Time Stamp, microseconds, 8-byte value).  push_video_misp_to
    //   inserts the SEI NAL *before* the first VCL NAL, then TS-frames the AU.
    let misp = MispTimestamp::micros(frame_pts_us, time_status);
    muxer.push_video_misp_to(vid_handle, &nal, frame_pts_90khz, is_key_frame, &misp)?;

    // ── 5. Build the KLV record with required MISMMS fields.
    let mut rec = UasDatalinkLs::default();
    rec.timestamp_us = Some(frame_pts_us); // Tag 2 — required by MISMMS
    rec.platform_heading_deg = Some(217.5); // Tag 5 — required
    rec.platform_pitch_deg = Some(-2.1);   // Tag 6 — required (±20° range)
    rec.platform_roll_deg = Some(-1.8);    // Tag 7 — required (±50° range)
    rec.sensor_lat_deg = Some(33.6800);    // Tag 13 — required
    rec.sensor_lon_deg = Some(-118.5500);  // Tag 14 — required
    rec.sensor_alt_m = Some(3500.0);       // Tag 15 — required
    rec.sensor_hfov_deg = Some(45.0);      // Tag 16 — required
    rec.sensor_vfov_deg = Some(30.0);      // Tag 17 — required
    rec.slant_range_m = Some(4800.0);      // Tag 21 — required
    // ST 0902 also requires Tags 3, 10, 11, 12, 18-20, 22-25 and the
    // Security Local Set (Tag 48). This sketch leaves them out, so the
    // validate_mismms call below reports them; set them from your mission
    // and sensor state.
    // Embed the Core ID in Tag 94 so archive systems can correlate streams.
    rec.miis_core_id = Some(core_id_bytes);

    // ── 6. Validate the record against the ST 0902 MISMMS before encoding.
    //
    //   validate_mismms checks the record against the 23 MISMMS items
    //   (ST 0902.8 Table 1).  A non-empty Vec names every unmet item; the
    //   record will fail a strict receiver's minimum-metadata check.
    let violations = validate_mismms(&rec);
    if !violations.is_empty() {
        // Surface the violation list so the operator can fix the gap.
        // In production, emit a metric and continue (or abort, your policy).
        eprintln!("MISMMS violations (drop or alert): {:?}", violations);
    }

    // ── 7. Encode via the strict-compliance path.
    //
    //   encode_strict_compliance requires Tag 2 (Precision Time Stamp) and
    //   cleans string fields (ST 0107), then encodes as encode_to_vec does;
    //   both reject a value outside its ST 0601 wire range.
    let klv_bytes = encode_strict_compliance(&rec)?;

    // push_klv auto-prepends the 5-byte Metadata_AU_cell header (because
    // this stream is SynchronousMetadata).  Pass raw KLV LS bytes — no
    // pre-wrapping.  metadata_service_id defaults to 0x00 (ST 1402.2 App. B).
    muxer.push_klv_to(klv_handle, &klv_bytes, frame_pts_90khz, 0x00)?;

    // ── 8. Drain the muxer output buffer into your transport.
    let mut buf = vec![0u8; 188 * 64]; // 64 TS packets at a time
    loop {
        let n = muxer.pull(&mut buf);
        if n == 0 {
            break;
        }
        // Write buf[..n] to your SRT/UDP/TCP/file transport here.
    }

    Ok(())
}
```

## Python variant

The Python binding exposes `Muxer.push_video_misp_to` (keyword-only after the
NAL bytes) and `encode_uas_datalink_strict_compliance` for the same pipeline.
The **binding-side** `MuxSender` wrappers do not yet expose
`send_video_misp_to` — in Python/JVM/C use `Muxer.push_video_misp_to`
directly when you need MISP timestamps (the Rust `MuxSender` exposes
`send_video_misp_to` natively; see the
[Binding-side MuxSender misp mirrors](/docs/project/deferred-features.md)
deferred-features entry).

```python
import io as _io
from tstrans.codec import MispTimestamp
from tstrans.klv import (
    UasDatalinkLs,
    CoreId, IdType,
    encode_core_id,
    encode_uas_datalink_strict_compliance,
    validate_mismms,
)
from tstrans.mpegts import (
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerProgramConfigBuilder,
    VideoCodec,
    Pts90khz,
)

# ── 1. Build the muxer config: H.264 + sync-KLV.
prog = MuxerProgramConfigBuilder(1, 0x1000)
prog.add_video(0x1011, VideoCodec.H264)
prog.add_klv(0x1031, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
cfg = MuxerConfig.builder().add_program(prog.build()).build()
muxer = Muxer(cfg)

# ── 2. Acquire handles (index 0 = the single video / KLV stream).
vid_handle = muxer.video_stream_handle(0)
klv_handle = muxer.klv_stream_handle(0)

# ── 3. Build the Core ID for Tag 94.
sensor_uuid = bytes.fromhex("0123456789abcdef0123456789abcdef")
core_id = CoreId(
    version=1,
    sensor=(IdType.PHYSICAL, sensor_uuid),
    platform=None,
    window=None,
    minor=None,
)
core_id_bytes = encode_core_id(core_id)

# ── Per-frame loop ────────────────────────────────────────────────────────────

frame_pts_us: int = 1_700_000_000_000_000   # capture wall-clock (µs)
# 90 kHz ticks from microseconds: ×9/100 keeps sub-second precision.
frame_pts_90k = Pts90khz(frame_pts_us * 9 // 100)
nal = bytes()       # replace: H.264 Annex-B NAL bytes from your encoder
time_status = 0x1F  # ST 0603: 0x1F = locked, 0x9F = lock unknown

# ── 4. Splice the MISP SEI.
#   push_video_misp_to uses keyword-only args after the NAL bytes.
misp = MispTimestamp.micros(frame_pts_us, time_status)
muxer.push_video_misp_to(
    vid_handle,
    nal,
    pts=frame_pts_90k,
    dts=None,        # None → use PTS as DTS (no B-frame reordering)
    key_frame=True,
    misp=misp,
)

# ── 5. Build and validate the KLV record.
rec = UasDatalinkLs(
    timestamp_us=frame_pts_us,
    platform_heading_deg=217.5,
    platform_pitch_deg=-2.1,
    platform_roll_deg=-1.8,
    sensor_lat_deg=33.6800,
    sensor_lon_deg=-118.5500,
    sensor_alt_m=3500.0,
    sensor_hfov_deg=45.0,
    sensor_vfov_deg=30.0,
    slant_range_m=4800.0,
    miis_core_id=core_id_bytes,  # Tag 94
)
violations = validate_mismms(rec)
if violations:
    print("MISMMS violations:", violations)

# ── 6. Strict-compliance encode and push.
klv_bytes = encode_uas_datalink_strict_compliance(rec)
muxer.push_klv_to(klv_handle, klv_bytes, pts=frame_pts_90k, metadata_service_id=0)

# ── 7. Drain output into your transport.
buf = bytearray(188 * 64)
while True:
    n = muxer.pull(buf)
    if n == 0:
        break
    # Write buf[:n] to your transport here.
```

## Notes

**`encode_strict_compliance` vs `encode_to_vec`:** Both reject ST 0601
field values that fall outside their wire range (e.g., Tag 6 Platform Pitch
is ±20°, not ±90°); strict compliance also requires Tag 2 (Precision Time
Stamp) and removes banned characters and padding from string fields. For
narrow-range tags, the error message names the full-range twin
field (e.g., Tag 6 → Tag 90). If your platform can produce pitch values
outside ±20°, use Tag 90 (Platform Pitch Angle, ±90°) and Tag 91 (Platform
Roll Angle, ±90°) instead. Use `encode_to_vec` where a record without
Tag 2 must still pass through.

**`validate_mismms` is record-level:** The validator checks a single KLV
record in isolation against the 23 items of ST 0902.8 Table 1. It
does not track inter-record cadence (the 30-second MISMMS reporting cadence
is a stream-level requirement not checked here — see the
[MISMMS cadence tracker](/docs/project/deferred-features.md) entry).

**Tag 94 Core ID:** `encode_core_id` encodes a `CoreId` into 18, 34 or 50
bytes of binary wire format (1 version + 1 usage + one to three 16-byte UUID
blocks). Assign
it to `UasDatalinkLs::miis_core_id` as a `Vec<u8>`. Use a stable UUID per
physical sensor — changing it across sessions breaks archive correlation.

**Binding-side `send_video_misp_to`:** the Python, JVM, and C shell
wrappers around `MuxSender` do not yet expose the MISP variant of
`send_video` (the Rust `MuxSender::send_video_misp_to` exists). Use
`Muxer::push_video_misp_to` directly (as above), or the C ABI entry point
`tst_muxer_push_video_misp_to`. See the
[reference/stanag-4609.md](/docs/reference/stanag-4609.md) page for the
C and JVM snippet.
