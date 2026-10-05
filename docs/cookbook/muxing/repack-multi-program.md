# Repack two single-program inputs into one multi-program TS

> **When to use this:** You have two independent (EO + IR + KLV) feeds and need to ship them through one SRT socket without forcing each to its own UDP port.

> **Related:**
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — multi-program PMT shape and per-program handles
> - [Example: `repack_two_programs`](/examples/muxing/repack_two_programs.rs)

When you have two independent (EO + IR + KLV) feeds and need to ship them
through one SRT socket without forcing each to its own UDP port:

```rust,no_run
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build each program independently, then add both to the parent
    // builder. PIDs must be unique across all programs in one config.
    let config = {
        let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
        p1.add_video(0x1011, VideoCodec::H264);
        p1.add_klv(0x1031, KlvStreamType::PrivateData, false);

        let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
        p2.add_video(0x1111, VideoCodec::H264);
        p2.add_klv(0x1131, KlvStreamType::PrivateData, false);

        let mut b = MuxerConfig::builder();
        b.add_program(p1.build());
        b.add_program(p2.build());
        b.build()?
    };

    // Resolve handles per-program; push_video_to/push_klv_to route to
    // the correct elementary stream even when two programs carry the same
    // codec.  The bare push_video / push_klv reject with AmbiguousTarget
    // when more than one stream of that kind exists across all programs.
    // let mut mux = Muxer::new(config)?;
    // let v1 = mux.video_handles_for_program(1)?[0];
    // let v2 = mux.video_handles_for_program(2)?[0];
    // mux.push_video_to(v1, &nal_bytes, pts, key_frame)?;
    Ok(())
}
```

On the receive side, the consumer sees two independent `ProgramMap` events
and can route `Sample`/`Metadata` events by `stream.program_number`. The
receiver picks one program of interest with ffmpeg `-map p:N` or TSDuck
`--pid-only`. PID uniqueness across programs is required by the muxer;
renumber program 2's input PIDs into a non-conflicting range during the
demux→remux step.

Runnable: [examples/muxing/repack_two_programs.rs](/examples/muxing/repack_two_programs.rs).
