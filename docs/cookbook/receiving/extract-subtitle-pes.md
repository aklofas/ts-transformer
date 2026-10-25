# Extract subtitle PES bytes from a captured `.ts` file

> **When to use this:** Receive-side inspection — discover what subtitle codecs are in a capture and read the cue text.

> **Related:**
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — `Demuxer`, `DemuxEvent`, and `SamplePayload::Subtitle`
> - [Example: `demux_subtitle_file`](/examples/codec-parsing/demux_subtitle_file.rs)

Use case: receive-side inspection — what subtitle codecs are in a
capture, and what's the cue text?

```rust
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};

let mut demux = Demuxer::new();
// Feed bounded chunks (one feed() over the 4 MiB ingress ceiling fails)
// and drain after each; the trailing `None` flushes the last PES.
for chunk in bytes.chunks(188 * 1024).map(Some).chain([None]) {
    match chunk {
        Some(chunk) => demux.feed(chunk)?,
        None => demux.flush(),
    }
    while let Some(e) = demux.next_event() {
        if let DemuxEvent::Sample {
            stream,
            payload: SamplePayload::Subtitle { codec, payload },
            ..
        } = e
        {
            println!(
                "PID 0x{:04x} codec={:?} bytes={}",
                stream.pid,
                codec,
                payload.len()
            );
        }
    }
}
```

Runnable: `cargo run -p tst-examples --example demux_subtitle_file -- input.ts`.
