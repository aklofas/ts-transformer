# Mux AV1 video with KLV

> **When to use this:** The encoder produces AV1 — note OBU framing replaces Annex-B NAL framing.

> **Related:**
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — AV1 binding-conformant default and stream_type 0x06
> - [guides/codec.md](/docs/guides/codec.md) — OBU framing and `obu_has_size_field` requirements
> - [Example: `mux_av1_with_klv`](/examples/muxing/mux_av1_with_klv.rs)

AV1 uses OBU framing — fundamentally different from the NAL-shaped codecs
(H.264 / H.265 / H.266). Key differences when feeding `Muxer::push_video`:

- **No Annex-B start codes.** OBUs are self-describing and length-prefixed
  via LEB128. Concatenating OBUs with no separator produces a complete
  access unit.
- **AV1-in-MPEG-2-TS binding §3.1 requires `obu_has_size_field = 1`** on
  every OBU so the demultiplexer can walk the OBU stream without a
  separate framing layer.
- **PMT `stream_type = 0x06`** plus an auto-emitted `AV01`
  `registration_descriptor` (binding §2.1) tells receivers the bytes are
  AV1 rather than KLV-async on the same stream_type byte.

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::Av1);
        prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, /*carries_pts=*/ true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };
    let mut mux = Muxer::new(cfg)?;
    // `au_obus` is a contiguous OBU sequence (each with obu_has_size_field=1).
    // The example builds one synthetic Sequence Header + Temporal Delimiter +
    // Frame access unit; real consumers feed the encoder's output verbatim.
    let au_obus: Vec<u8> = vec![/* concatenated OBUs */];
    mux.push_video(&au_obus, Pts90khz::new(0), /* key_frame = */ true)?;
    Ok(())
}
```

Runnable: [examples/muxing/mux_av1_with_klv.rs](/examples/muxing/mux_av1_with_klv.rs).
