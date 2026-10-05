# Pair sync-KLV in batch mode (`PairerMode::Buffered`)

> **When to use this:** KLV PES is interleaved *after* its matching video PES (some encoders), and Realtime mode misses the pairing.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `PairerMode::Buffered` semantics and `max_lag`
> - [Pair sync-KLV with video AUs via `Pairer::with_config` (Realtime)](/docs/cookbook/pairing/pairer-realtime.md) — the Realtime sibling

When KLV PES is interleaved *after* its matching video PES (some
encoders), Realtime mode misses the pairing. Buffered mode holds video
briefly to look ahead.

```rust,no_run
use std::time::Duration;
use tst_pipeline::ext::pairing::{Pairer, PairerMode, PairerConfig};

let mut opts = PairerConfig::default();
opts.mode = PairerMode::Buffered { max_lag: Duration::from_secs(2) };
opts.tolerance = Duration::from_millis(300);
opts.max_buffered_klv = 32;
opts.max_buffered_video = 60; // ≈2 s @ 30 fps
let mut pairer = Pairer::with_config(0x100, 0x102, opts);
// feed loop unchanged from the Realtime recipe.
```

Trade-off: up to ~2 s pairing-induced latency in exchange for picking
up otherwise-lost matches. Pick `Realtime` if you can't tolerate the
delay.
