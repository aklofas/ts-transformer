# Sample-and-hold async KLV via `Pairer::last_before_pts`

> **When to use this:** Async-KLV streams where each video frame should attach the most recent KLV at `klv.pts <= video.pts`.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `Pairer::last_before_pts` and freshness ceilings
> - [Sample-and-hold async-KLV against video frames](/docs/cookbook/pairing/sample-hold-klv.md) — the inline pattern this replaces

Replaces the [sample-and-hold](/docs/cookbook/pairing/sample-hold-klv.md) inline pattern. The pairer keeps
the last 8 KLV samples, and each video frame attaches the one with the
latest PTS at or before its own (`klv.pts <= video.pts`), whatever order
the KLV arrived in. The history is what lets a reordered B-frame, whose
PTS is below the newest KLV's, still pair with the sample that preceded
it. A KLV can pair with several frames; it is emitted as `UnpairedKlv`
only if it never paired, when it is evicted from the history or at
`flush`.

```rust,no_run
use std::time::Duration;
use tst_pipeline::ext::pairing::{Pairer, PairerOutput};
# fn demux_events() -> impl Iterator<Item = tst_core::mpegts::demux::DemuxEvent> { std::iter::empty() }

let mut pairer = Pairer::last_before_pts(
    0x100, // video PID
    0x102, // async-KLV PID
    Some(Duration::from_secs(2)), // freshness ceiling — drop pair if KLV is staler
);
for e in demux_events() {
    for o in pairer.feed(e) {
        match o {
            PairerOutput::Paired { video, klv } => { let _ = (video, klv); }
            PairerOutput::UnpairedVideo(_) => { /* KLV too stale or never seen */ }
            _ => {}
        }
    }
}
let _ = pairer.flush();
```

The freshness ceiling is inclusive: a KLV exactly `freshness` behind the
frame still pairs. It is checked only on the selected KLV, so a stale
sample that arrives late never hides a fresher one. Pass
`freshness = None` to attach regardless of staleness (the inline
sample-and-hold recipe's default behavior).
