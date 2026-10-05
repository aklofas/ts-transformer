# EO + IR sensor pair with shared async-KLV

> **When to use this:** The platform carries two sensors (visible + thermal) and one async metadata stream serves both.

> **Related:**
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — `ProgramMap`, `KlvLink`, and `metadata_descriptor` discovery
> - [EO + IR composition with shared async-KLV](/docs/cookbook/pairing/eo-ir-shared-klv-pairer.md) — the typed-projection version using two `Pairer` instances

Reach for this when the platform carries two sensors (visible + thermal) and one async metadata stream serves both. Both video streams attach the same KLV state; there is no per-stream pairing logic. The demuxer surfaces the topology as a `ProgramMap` with two `StreamInfo` rows of `StreamKind::Video(_)` and one `StreamKind::KlvAsync`; the `klv_links` table reports the encoder-declared (or inferred / overridden) linkage.

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};

fn process_eo(_pts: i64, _klv: Option<&[u8]>) {}
fn process_ir(_pts: i64, _klv: Option<&[u8]>) {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    let mut last_klv: Option<Vec<u8>> = None;
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::Metadata { kind: MetadataKind::KlvAsync, payload, .. } => {
                last_klv = Some(payload);
            }
            DemuxEvent::Sample { stream, payload: SamplePayload::Video { .. }, pts, .. } => {
                match stream.pid {
                    0x100 => process_eo(pts.as_ticks(), last_klv.as_deref()),
                    0x101 => process_ir(pts.as_ticks(), last_klv.as_deref()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}
```

If the encoder declares the linkage via `metadata_descriptor`, the demuxer surfaces it as `KlvLink { source: LinkSource::Declared, .. }` in `ProgramMap.klv_links`. Use it as a hint when assigning routes; trust your `treat_as` overrides if you know the encoder lies.

Runnable: see [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs) for the file-feed shape; [examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs) is the related sync-KLV sibling.
