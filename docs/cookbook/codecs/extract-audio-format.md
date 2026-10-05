# Pull sample rate and channel count out of an audio stream

> **When to use this:** Inspect a `.ts` file and report typed audio metadata (sample rate, channel count, codec layer/profile) per audio PID.

> **Related:**
> - [guides/codec.md](/docs/guides/codec.md) — `codec::mpegaudio::frames` and `codec::aac::frames`
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — `SamplePayload::Audio` dispatch
> - [Example: `parse_audio_frames`](/examples/codec-parsing/parse_audio_frames.rs)

**Goal:** Inspect a `.ts` file and report the typed audio metadata
(sample rate, channel count, codec layer/profile) per audio PID, logging
only the points where the metadata changes.

**Pattern:** Demux to events, dispatch on `SamplePayload::Audio { codec,
frames }`, run `codec::mpegaudio::frames` or `codec::aac::frames` on the
PES blob.

```rust
use tst_core::codec;
use tst_core::mpegts::demux::{AudioCodec, Demuxer, DemuxEvent, SamplePayload};

let bytes = std::fs::read("input.ts")?;
let mut demuxer = Demuxer::new();
demuxer.feed(&bytes)?;
demuxer.flush();

while let Some(ev) = demuxer.next_event() {
    if let DemuxEvent::Sample {
        stream,
        payload: SamplePayload::Audio { codec, frames, .. },
        ..
    } = ev {
        let pid = stream.pid;
        match codec {
            AudioCodec::Mp2 => {
                for f in codec::mpegaudio::frames(&frames).filter_map(Result::ok) {
                    println!("PID 0x{:04x} {:?} {} Hz, {} ch", pid, f.layer, f.sample_rate_hz, f.channels);
                }
            }
            AudioCodec::Aac => {
                for f in codec::aac::frames(&frames).filter_map(Result::ok) {
                    // `channels()` is None for a PCE-defined layout (channel_configuration 0).
                    println!("PID 0x{:04x} AAC {:?} {} Hz, {:?} ch", pid, f.profile, f.sample_rate_hz, f.channels());
                }
            }
            _ => {}
        }
    }
}
```

**Runnable variant:** `cargo run -p tst-examples --example parse_audio_frames -- input.ts`
deduplicates output to first-change-only per PID.

**Caveats:**
- `filter_map(Result::ok)` skips parse errors silently. For first-error-stop
  semantics, use `.collect::<Result<Vec<_>, _>>()`.
- Mislabeled-private PIDs in real-world captures may yield mostly
  `BadSyncWord` errors (the source data is private, not audio).
- Silent audio still produces header-valid frames; iterator output ≠ "is
  this audio actually audible."
