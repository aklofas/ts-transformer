# Pair sync-KLV with video AUs via `Pairer::with_config` (Realtime)

> **When to use this:** You want the inline nearest-PTS pattern ([Pair sync-KLV with video AUs by nearest PTS](/docs/cookbook/pairing/pair-klv-by-pts.md)) expressed through the opt-in `Pairer` helper, with bounded history, telemetry counters, and typed projection structs.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `Pairer`, `PairerMode`, and the typed output projections
> - [Pair sync-KLV with video AUs by nearest PTS](/docs/cookbook/pairing/pair-klv-by-pts.md) — the inline ~20-line equivalent
> - [Example: `pair_klv_pipeline`](/examples/pairing/pair_klv_pipeline.rs)

The [nearest-PTS](/docs/cookbook/pairing/pair-klv-by-pts.md) inline pattern in ~20 lines, expressed through
the opt-in `tst_pipeline::ext::pairing::Pairer`. Same semantics, with
bounded KLV history, telemetry counters, and typed projection structs
on the output.

```rust,no_run
use std::fs;
use std::time::Duration;
use tst_core::mpegts::demux::Demuxer;
use tst_pipeline::ext::pairing::{Pairer, PairerMode, PairerConfig, PairerOutput};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read("input.ts")?;
    let mut demux = Demuxer::new();
    demux.feed(&bytes)?;
    demux.flush();

    // PairerConfig is `#[non_exhaustive]`; construct via Default + assign.
    let mut opts = PairerConfig::default();
    opts.mode = PairerMode::Realtime;
    opts.tolerance = Duration::from_millis(300);
    opts.max_buffered_klv = 32; // ~1 s history at 30 Hz cadence
    opts.max_buffered_video = 32;
    let mut pairer = Pairer::with_config(
        0x100, // video PID (discover from ProgramMap)
        0x102, // KLV PID
        opts,
    );
    while let Some(e) = demux.next_event() {
        for o in pairer.feed(e) {
            if let PairerOutput::Paired { video, klv } = o {
                // video.raw → decoder (see "Reconstitute Annex B parameter sets")
                // klv.payload   → tst_core::klv::st0601::decode
                let _ = (video, klv);
            }
        }
    }
    let _ = pairer.flush();
    println!("{:?}", pairer.stats());
    Ok(())
}
```

Runnable: `cargo run -p tst-examples --example pair_klv_pipeline -- input.ts`.
