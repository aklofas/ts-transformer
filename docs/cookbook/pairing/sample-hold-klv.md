# Sample-and-hold async-KLV against video frames

> **When to use this:** KLV is emitted independently of video — typically 1–10 Hz async metadata against 25–60 fps video.

> **Related:**
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — async-KLV carriage and PES timestamps
> - [Sample-and-hold async KLV via `Pairer::last_before_pts`](/docs/cookbook/pairing/pairer-last-before-pts.md) — the typed-projection version using `Pairer`

Reach for this when KLV is emitted independently of video — typically 1–10 Hz async metadata against 25–60 fps video. The canonical pairing is "the most recent KLV record where `klv.pts <= frame.pts`." There is no ambiguity about which KLV pairs with which frame; a small ordered buffer keeps that true when records arrive out of order (a B-frame's PTS can be below the newest record's). The only knob is whether to drop a frame when the most recent KLV is too stale.

```rust,no_run
use std::collections::BTreeMap;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut d = Demuxer::new();
    // KLV records keyed by PTS ticks, so a frame pairs with the latest
    // record at or before its own PTS whatever order the records arrived in:
    let mut klv: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::Metadata { pts, kind: MetadataKind::KlvAsync, payload, .. } => {
                klv.insert(pts.as_ticks(), payload);
            }
            DemuxEvent::Sample { payload: SamplePayload::Video { .. }, pts: frame_pts, .. } => {
                let frame_ticks = frame_pts.as_ticks();
                // Latest record with klv.pts <= frame.pts, regardless of how stale.
                // Optional: compare ages and drop if stale beyond a freshness window.
                let _telemetry = klv.range(..=frame_ticks).next_back().map(|(_, p)| p);
                // Prune older records. This assumes frame PTS never goes backwards;
                // for B-frame content use `Pairer::last_before_pts` (see the related recipe).
                if let Some(&keep) = klv.range(..=frame_ticks).next_back().map(|(k, _)| k) {
                    klv = klv.split_off(&keep);
                }
            }
            _ => {}
        }
    }
    Ok(())
}
```

Runnable: see [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs) for the file-feed shape; [examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs) is the related nearest-PTS sibling for sync KLV.
