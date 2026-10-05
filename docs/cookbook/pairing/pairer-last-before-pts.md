# Sample-and-hold async KLV via `Pairer::last_before_pts`

> **When to use this:** Async-KLV streams where each video frame should attach the most recent KLV at `klv.pts <= video.pts`.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `Pairer::last_before_pts` and freshness ceilings
> - [Sample-and-hold async-KLV against video frames](/docs/cookbook/pairing/sample-hold-klv.md) — the inline pattern this replaces

Replaces the [sample-and-hold](/docs/cookbook/pairing/sample-hold-klv.md) inline pattern. Each video frame
attaches the most recent KLV at `klv.pts <= video.pts`.

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

Pass `freshness = None` to attach regardless of staleness (the inline
sample-and-hold recipe's default behavior).
