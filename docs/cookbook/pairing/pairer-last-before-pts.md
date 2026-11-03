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

The history is cleared at a reconnect and at a signalled discontinuity (a set `discontinuity_indicator`) on the video PID, the KLV PID or the PCR PID of the video's program (the PCR PID counts only when that PID also carries an elementary stream); entries that never paired come out as `UnpairedKlv`. The PES in flight at the flag completes after it on the old clock, so the first KLV sample and the first video sample completing after a flag come out unpaired (`UnpairedKlv`, `UnpairedVideo`) when their PTS is within two seconds of the newest PTS before the flag. A new clock that lands within two seconds of the old one loses its first KLV sample (every frame until the next sample is unpaired) and its first video frame the same way. A flag on any other PID and a silent restart are invisible to the pairer, so call `pairer.flush()` yourself at a restart your source does not signal. A PID that carries only PCR produces no demuxer event, so call `pairer.flush()` at a restart on it too.

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
