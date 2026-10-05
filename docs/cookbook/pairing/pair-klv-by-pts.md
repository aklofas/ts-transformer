# Pair sync-KLV with video AUs by nearest PTS

> **When to use this:** An encoder emits sync-KLV synchronized to video frames (one KLV per frame, KLV PES PTS = frame PTS) and you want to consume frame + telemetry as a paired record.

> **Related:**
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — why the demuxer surfaces KLV + video as independent events
> - [guides/pipeline.md](/docs/guides/pipeline.md) — the `Pairer` helper for typed projection (the `Pairer` recipes in this section)
> - [Example: `pair_sync_klv`](/examples/pairing/pair_sync_klv.rs)

Reach for this when an encoder emits sync-KLV (PMT stream_type 0x15, H.222.0 § 2.12.4.2 `Metadata_AU_cell`) synchronized to video frames (one KLV per frame, KLV PES PTS = frame PTS) and you want to consume frame + telemetry as a paired record. By design, `mpegts::demux` does NOT pair sync-KLV with video AUs — it surfaces them as independent stream-tagged events with full timing info, and the pairing tolerance is consumer-domain knowledge. This recipe is the canonical nearest-PTS pattern.

Match BOTH `MetadataKind::KlvSyncAuCell` AND `MetadataKind::KlvAsync`. The natural intuition is "sync KLV is the kind that needs pairing," but many production ISR encoders declare a PID `stream_type=0x15` and ship bare KLV without the 5-byte AU cell header. The demuxer surfaces those bytes as `KlvAsync` with the PES PTS preserved on the parent event. That `KlvAsync` is still PTS-aligned with video; matching only `KlvSyncAuCell` silently drops the most common shape we see in the field.

```rust,no_run
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};
use std::collections::VecDeque;
use std::fs;

// 0.3 s at 90 kHz — wide enough to absorb encoder timestamp drift,
// narrow enough to reject a coincidental near-match from the next GOP.
const PAIRING_TOLERANCE_TICKS: i64 = 3 * 9_000;
// 32 entries of KLV history. ~1 s at 30 fps + 1 KLV/frame; 32 s at 1 Hz KLV.
const KLV_HISTORY_LEN: usize = 32;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read("input.ts")?;
    let mut d = Demuxer::new();
    d.feed(&bytes)?;
    d.flush();
    let mut history: VecDeque<(i64, Vec<u8>)> = VecDeque::with_capacity(KLV_HISTORY_LEN);
    let (mut paired, mut unpaired) = (0usize, 0usize);
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::Metadata {
                pts,
                kind: MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync,
                payload,
                ..
            } => {
                // Plain i64 ticks for the arithmetic below (Pts90khz is a newtype).
                history.push_back((pts.as_ticks(), payload));
                if history.len() > KLV_HISTORY_LEN {
                    history.pop_front();
                }
            }
            DemuxEvent::Sample {
                pts,
                payload: SamplePayload::Video { .. },
                ..
            } => {
                let pts = pts.as_ticks();
                let nearest = history.iter().min_by_key(|(kpts, _)| (kpts - pts).abs());
                match nearest {
                    Some((kpts, _)) if (kpts - pts).abs() <= PAIRING_TOLERANCE_TICKS => {
                        paired += 1;
                    }
                    _ => unpaired += 1,
                }
            }
            _ => {}
        }
    }
    println!("paired={paired} unpaired={unpaired}");
    Ok(())
}
```

Tolerance is consumer-domain knowledge. Most encoders emit KLV PES PTS exactly equal to frame PTS; a window of a few hundred milliseconds covers minor encoder drift. See [examples/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs) for the full runnable form.

Runnable: [examples/pairing/pair_sync_klv.rs](/examples/pairing/pair_sync_klv.rs); see also [examples/receiving/demux_to_events.rs](/examples/receiving/demux_to_events.rs) for the file-feed shape.
