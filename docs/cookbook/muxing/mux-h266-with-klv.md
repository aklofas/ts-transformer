# Mux H.266 / VVC video with synchronous KLV

> **When to use this:** The encoder produces H.266 (VVC) and the receiver requires strict ST 1402 sync KLV metadata.

> **Related:**
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — H.266 stream_type 0x33 and AU-cell auto-wrap
> - [Mux H.265 + sync KLV](/docs/cookbook/muxing/mux-h265-with-klv.md) — same shape, H.265 codec
> - [Example: `mux_h266_with_klv`](/examples/muxing/mux_h266_with_klv.rs)

H.266 (VVC) carries in MPEG-TS under PMT `stream_type = 0x33` per the
ITU-T H.222.0 amendment for VVC; the muxer emits that byte automatically
when `VideoCodec::H266` is configured. The push contract is identical to
H.264 / H.265 — Annex-B framing on `push_video`, one PES per call. Only
the codec flag and the SPS / PPS / VPS bytes change.

The recipe below mirrors [Mux H.265 + sync KLV](/docs/cookbook/muxing/mux-h265-with-klv.md) — flip the codec to
`VideoCodec::H266` and feed H.266 NAL bytes (NAL types 14 / 15 / 16 for
VPS / SPS / PPS).

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, Muxer, VideoCodec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H266);
        prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, /*carries_pts=*/ true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };
    let mut mux = Muxer::new(cfg)?;
    let inner_klv: Vec<u8> = vec![/* ST 0601 bytes */];
    // Muxer auto-prepends the 5-byte H.222.0 § 2.12.4.2 AU cell header.
    // metadata_service_id defaults to 0x00 per ST 1402.2 App. B Table 2.
    mux.push_klv(&inner_klv, Pts90khz::new(0), /*metadata_service_id=*/ 0x00)?;
    Ok(())
}
```

Runnable: [examples/muxing/mux_h266_with_klv.rs](/examples/muxing/mux_h266_with_klv.rs).
