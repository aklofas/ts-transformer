# EO + IR composition with shared async-KLV

> **When to use this:** Two video PIDs share one async-KLV PID and you want telemetry counters + typed output projections per branch.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — composing two `Pairer` instances
> - [EO + IR sensor pair with shared async-KLV](/docs/cookbook/pairing/eo-ir-shared-klv.md) — the inline `Option<Vec<u8>>` equivalent

Two video PIDs sharing one async-KLV PID. The [EO + IR sensor pair](/docs/cookbook/pairing/eo-ir-shared-klv.md) recipe's inline
`Option<Vec<u8>>` pattern remains valid; this recipe shows the same
shape via two `Pairer` instances (one per video PID).

```rust,no_run
use tst_pipeline::ext::pairing::{Pairer, PairerOutput};
# fn demux_events() -> impl Iterator<Item = tst_core::mpegts::demux::DemuxEvent> { std::iter::empty() }

const EO_PID: u16 = 0x100;
const IR_PID: u16 = 0x101;
const KLV_PID: u16 = 0x102;

let mut eo_pairer = Pairer::last_before_pts(EO_PID, KLV_PID, None);
let mut ir_pairer = Pairer::last_before_pts(IR_PID, KLV_PID, None);

for e in demux_events() {
    // Both pairers need program maps and epoch boundaries as well as
    // samples; a sample for the other video PID comes back as `PassThrough`.
    let mut outputs = eo_pairer.feed(e.clone());
    outputs.extend(ir_pairer.feed(e));
    for o in outputs {
        match o {
            PairerOutput::Paired { video, klv } => {
                match video.stream.pid {
                    EO_PID => { /* EO-paired */ let _ = (video, klv); }
                    IR_PID => { /* IR-paired */ let _ = (video, klv); }
                    _ => unreachable!(),
                }
            }
            _ => {}
        }
    }
}
```

Compared to the inline EO + IR pattern, the Pairer-based composition
adds telemetry counters per branch and the typed output projections,
at the cost of cloning each event once. For large video payloads, route
samples to their matching branch and forward every other event to both.
