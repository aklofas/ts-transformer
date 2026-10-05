# Sample-and-hold async-KLV against video frames

> **When to use this:** KLV is emitted independently of video — typically 1–10 Hz async metadata against 25–60 fps video.

> **Related:**
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — async-KLV carriage and PES timestamps
> - [Sample-and-hold async KLV via `Pairer::last_before_pts`](/docs/cookbook/pairing/pairer-last-before-pts.md) — the typed-projection version using `Pairer`

Reach for this when KLV is emitted independently of video — typically 1–10 Hz async metadata against 25–60 fps video. The canonical pairing is "the most recent KLV record where `klv.pts <= frame.pts`." There is no ambiguity about which KLV pairs with which frame; the only knob is whether to drop a frame when the most recent KLV is too stale.

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    // Maintain "current KLV state" per metadata PID:
    let mut last_klv: Option<(i64, Vec<u8>)> = None;
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::Metadata { pts, kind: MetadataKind::KlvAsync, payload, .. } => {
                last_klv = Some((pts.as_ticks(), payload));
            }
            DemuxEvent::Sample { payload: SamplePayload::Video { .. }, pts: _frame_pts, .. } => {
                // Use last_klv if available, regardless of how stale.
                // Optional: compare ages and drop if stale beyond a freshness window.
                let _telemetry = last_klv.as_ref().map(|(_, payload)| payload);
            }
            _ => {}
        }
    }
    Ok(())
}
```

Runnable: see [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs) for the file-feed shape; [examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs) is the related nearest-PTS sibling for sync KLV.
