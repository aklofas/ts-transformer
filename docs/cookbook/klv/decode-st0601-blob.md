# Decode ST 0601 from a captured `.klv` blob

> **When to use this:** Validating producer output, building dashboards on captured data, or debugging a receiver.

> **Related:**
> - [guides/klv.md](/docs/guides/klv.md) — the decode strictness ladder and `UasDatalinkLs` field shape
> - [Example: `extract_klv`](/examples/klv-metadata/extract_klv.rs) + [`klv_decode_file`](/examples/klv-metadata/klv_decode_file.rs)

Reach for this when validating producer output, building dashboards on top of captured data, or debugging a receiver. The two-step pipeline is: extract KLV blobs from the `.ts` first, then decode each blob through the strictness ladder.

`extract_klv` runs the `Demuxer` over the capture and writes each `DemuxEvent::Metadata` payload (sync KLV arrives with its AU-cell header already stripped) as `<prefix>_NNNN_<kind>.klv`, where `NNNN` is a 4-digit index from 0 and `<kind>` is `sync`, `async` or `unknown`. Each `.klv` blob then feeds `klv_decode_file`, which walks the ladder `decode_strict_compliance` → `decode_strict` → `decode` → `decode_unchecked`, reporting which level accepted.

```rust,no_run
use tst_core::klv::st0601::{decode, decode_strict, decode_strict_compliance};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let buf = fs::read("capture_0000_async.klv")?;
    let parsed = decode_strict_compliance(&buf)
        .or_else(|_| decode_strict(&buf))
        .or_else(|_| decode(&buf))?;
    if let Some(ts) = parsed.timestamp_us {
        println!("timestamp_us: {ts}");
    }
    if let (Some(lat), Some(lon)) = (parsed.sensor_lat_deg, parsed.sensor_lon_deg) {
        println!("sensor: {lat:.6}, {lon:.6}");
    }
    Ok(())
}
```

Runnable: [examples/klv-metadata/extract_klv.rs](/examples/klv-metadata/extract_klv.rs) and [examples/klv-metadata/klv_decode_file.rs](/examples/klv-metadata/klv_decode_file.rs).

`decode`'s `UasDatalinkLs` types 142 of the 143 active ST 0601.19
items — the long tail (atmospheric/wind, target location, alternate
platform, sensor velocity, repeated-record packs like Waypoint List
and Weapons Stores, and the SDCC-FLP covariance pack) is all field
reads away, not a separate decode step. For the repeated-record and
covariance shapes specifically, see [Reading waypoint lists, weapons
stores, and SDCC covariance](/docs/cookbook/klv/decode-long-tail.md).
