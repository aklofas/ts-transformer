# Encode ST 0601 from typed values

> **When to use this:** Synthesizing KLV for tests, generating fixtures, or translating from a different metadata format in a gateway.

> **Related:**
> - [guides/klv.md](/docs/guides/klv.md) — `UasDatalinkLs` field shape and the auto-emitted Tag 1 / Tag 65
> - [Example: `klv_encode_minimal`](/examples/klv-metadata/klv_encode_minimal.rs)

Reach for this when synthesizing KLV for tests, generating fixtures, or translating from a different metadata format in a gateway. Every field on `UasDatalinkLs` is `Option<T>` — set `Some(...)` on the fields you want emitted, leave the rest as `None`.

`encode_to_vec` auto-emits Tag 1 (16-bit running-sum checksum, mandated last) and Tag 65 (UAS LS Version Number, mandated present) when the caller didn't set them. So a default-constructed record with a few typed fields produces wire bytes that satisfy strict-compliance validation out of the box.

```rust,no_run
use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rec = UasDatalinkLs::default();
    rec.timestamp_us = Some(1_700_000_000_000_000);
    rec.platform_designation = Some("test-platform".into());
    rec.sensor_lat_deg = Some(33.6800);
    rec.sensor_lon_deg = Some(-118.5500);
    rec.sensor_alt_m = Some(3500.0);
    rec.platform_heading_deg = Some(217.456);
    rec.platform_pitch_deg = Some(-2.150);
    rec.platform_roll_deg = Some(-1.875);
    let encoded = encode_to_vec(&rec)?;
    println!("encoded {} bytes", encoded.len());
    Ok(())
}
```

Runnable: [examples/klv-metadata/klv_encode_minimal.rs](/examples/klv-metadata/klv_encode_minimal.rs).
