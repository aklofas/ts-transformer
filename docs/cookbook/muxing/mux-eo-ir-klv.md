# Label EO + IR + KLV streams in a multi-stream program

> **When to use this:** Multi-stream programs carry several PIDs in one program; per-stream PMT descriptors let receivers (TSDuck, ffprobe, our `Demuxer`) render which PID is which.

> **Related:**
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — multi-stream programs, descriptor attachment, and stream_type table
> - [Example: `mux_dual_camera`](/examples/muxing/mux_dual_camera.rs)

Multi-stream programs carry several PIDs in one
program. Per-stream PMT descriptors let receivers (TSDuck, ffprobe, our
own `Demuxer`) render which PID is which without external configuration.

```rust,no_run
use tst_core::mpegts::descriptors as desc;
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, Muxer, VideoCodec,
};

const EO_PID: u16 = 0x0100;
const IR_PID: u16 = 0x0101;
const KLV_PID: u16 = 0x0102;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Per-stream descriptors live on the program builder; bind it,
    // call add_video / add_klv in order, then attach descriptors to
    // each by index. `stream_descriptors_for_*` are fallible
    // (DescriptorIndexOutOfRange when the index is past add-order),
    // so propagate with `?`.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(EO_PID, VideoCodec::H264);
        // `user_private` returns Result<_, DescriptorError> so oversized
        // labels surface as TooLarge rather than silently truncating.
        prog.stream_descriptors_for_video(0, vec![desc::user_private(b"EO 1080p")?])?;
        prog.add_video(IR_PID, VideoCodec::H264);
        prog.stream_descriptors_for_video(1, vec![desc::user_private(b"IR 640x480")?])?;
        prog.add_klv(KLV_PID, KlvStreamType::SynchronousMetadata, true);
        prog.stream_descriptors_for_klv(
            0,
            vec![
                // 0x26 + 0x27 are the canonical pair for stream_type=0x15 KLV;
                // the muxer adds its KLVA Registration (0x05) alongside them.
                desc::metadata_klva(0x00),
                desc::metadata_std(0, 0, 0),
                // Plus a human label.
                desc::user_private(b"KLV_SYNC")?,
            ],
        )?;
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };

    let mut _mux = Muxer::new(cfg)?;
    // ...push frames as usual...
    Ok(())
}
```

Validate the labels show up on the receiving end:

```bash
tstables --pid <pmt-pid> output.ts | grep -A1 "Forbidden Descriptor"
```

Or in Rust on the receive side, decode `StreamInfo::raw_descriptors`
directly (see [Reading per-stream descriptors](/docs/guides/mpegts-demux.md#reading-per-stream-descriptors)).

Runnable example: `cargo run -p tst-examples --example mux_dual_camera`.
