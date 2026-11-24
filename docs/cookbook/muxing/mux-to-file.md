# Mux to a file (no SRT, no transport)

> **When to use this:** You want the muxer's output without any networking — building test fixtures, validating output against TSDuck/ffprobe, or running an offline pipeline.

> **Related:**
> - [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — `Muxer`, the push/pull drain pattern, and PMT shape
> - [Example: `mux_to_file`](/examples/muxing/mux_to_file.rs)

Reach for this when you want the muxer's output without any networking — building test fixtures, validating output against TSDuck/ffprobe, or running an offline pipeline. `Muxer` is the standalone TS muxer; `push_video` and `push_klv` queue input, `pull` drains 188-byte-aligned TS packets into a caller-provided buffer.

The drain loop is the standard pattern: push input, then pull until `pull` returns 0. Drain after every push so muxer memory stays bounded.

```rust,no_run
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, Muxer};
use std::fs::File;
use std::io::Write;

fn main() -> std::io::Result<()> {
    let mut mux = Muxer::new(MuxerConfig::default()).expect("valid config");
    let mut out = File::create("out.ts")?;
    let mut buf = [0u8; 1316];
    for i in 0..150i64 {
        let pts = i * 3000; // 30 fps on 90 kHz clock
        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
        let klv = vec![0x06, 0x0E, 0x2B, 0x34, /* ... */];
        mux.push_video(&nal, Pts90khz::new(pts), i == 0).expect("push_video");
        // metadata_service_id=0x00 is the ST 1402.2 App. B Table 2 default.
        // On a synchronous KLV stream it must be a service the stream's
        // metadata_klva(svc) descriptor declares, or the push is refused;
        // the default config's KLV stream is asynchronous and ignores it.
        mux.push_klv(&klv, Pts90khz::new(pts), /*metadata_service_id=*/ 0x00).expect("push_klv");
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 { break; }
            out.write_all(&buf[..n])?;
        }
    }
    Ok(())
}
```

Runnable: [examples/muxing/mux_to_file.rs](/examples/muxing/mux_to_file.rs).
