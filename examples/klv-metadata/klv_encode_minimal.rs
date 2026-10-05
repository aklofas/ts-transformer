//! Build a UasDatalinkLs from typed values, encode, round-trip.
//!
//!   cargo run -p tst-examples --example klv_encode_minimal
//!
//! Sets a representative subset of ST 0601 fields (timestamp, sensor
//! position + attitude, frame center, platform attitude), encodes, decodes
//! the result, and asserts the typed fields round-trip.

use tst_core::klv::st0601::{UasDatalinkLs, decode_strict_compliance, encode_to_vec};

// Clippy's `field_reassign_with_default` would prefer the struct-update
// form `UasDatalinkLs { timestamp_us: Some(..), .., ..Default::default() }`,
// but that would collapse the per-block teaching comments below into one
// dense initializer. The whole point of this example is to walk a reader
// through *which* tags belong together (platform attitude, sensor pose,
// frame center) — so keep the field-by-field setter form on purpose.
#[allow(clippy::field_reassign_with_default)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Every field on `UasDatalinkLs` is `Option<T>`, so `Default::default()`
    // gives an empty record. Setting `Some(...)` on a field is what causes
    // the encoder to emit it on the wire; `None` fields are skipped.
    // This is the canonical "build a typed record incrementally" pattern.
    let mut rec = UasDatalinkLs::default();

    // ST 0601 Tag 2 — Precision Time Stamp, microseconds since Unix epoch.
    // The value here is roughly Nov 2023; it's a round-ish number that
    // makes the encoded bytes obvious in a hex dump if needed.
    rec.timestamp_us = Some(1_700_000_000_000_000);

    // String tags. These are emitted verbatim as ISO 646 / ASCII bytes
    // (Tag 4 platform tail number, Tag 10 platform designation, Tag 11
    // image source sensor, Tag 12 image coordinate system). Keeping the
    // values synthetic — `test-platform`, `TEST-001`, `test-sensor` — so
    // the example never accidentally ships a real call sign or aircraft
    // identifier through this tree.
    rec.platform_designation = Some("test-platform".into());
    rec.platform_tail_number = Some("TEST-001".into());
    rec.image_source_sensor = Some("test-sensor".into());
    rec.image_coordinate_system = Some("WGS-84".into());

    // ----- platform attitude + airspeed -----
    // ST 0601 Tag 5/6/7 (heading/pitch/roll) and Tag 8/9 (true/indicated
    // airspeed). The attitude tags use ST 0601's `LinearRange` mapping
    // (see `klv::st0601::mapping`) to map small integer encodings to
    // bounded float ranges — heading 0..360 packs into 2 bytes,
    // pitch/roll ±20° also pack into 2 bytes — so the float values you
    // set here are quantized to the wire resolution on encode and
    // recovered (within that resolution) on decode.
    rec.platform_heading_deg = Some(217.456);
    rec.platform_pitch_deg = Some(-2.150);
    rec.platform_roll_deg = Some(-1.875);
    rec.platform_true_airspeed = Some(120.5);
    rec.platform_indicated_airspeed = Some(118.0);

    // ----- sensor pose -----
    // Tag 13/14/15 sensor lat/lon/alt, Tag 16/17 sensor HFOV/VFOV, Tag
    // 18/19 sensor relative azimuth / elevation. The lat/lon values
    // `33.68, -118.55` are generic Southern California offshore — chosen
    // to be over open water and therefore intentionally non-operational
    // (no real-device positions or captures ship in the repository).
    rec.sensor_lat_deg = Some(33.6800);
    rec.sensor_lon_deg = Some(-118.5500);
    rec.sensor_alt_m = Some(3500.0);
    rec.sensor_hfov_deg = Some(2.5);
    rec.sensor_vfov_deg = Some(1.875);
    rec.sensor_rel_az_deg = Some(45.0);
    rec.sensor_rel_el_deg = Some(-15.0);

    // ----- frame center geolocation -----
    // Tag 23/24/25 — the lat/lon/elev where the sensor's optical axis
    // intersects the ground (or its model thereof). In a real capture
    // these typically differ from sensor position by the slant-range
    // projection through the sensor's pointing vector; here the values
    // are just nearby synthetic coordinates that exercise the encode
    // path for those tags.
    rec.frame_center_lat_deg = Some(33.6900);
    rec.frame_center_lon_deg = Some(-118.5400);
    rec.frame_center_elev_m = Some(120.0);

    // Happy-path encoder. `encode_to_vec` auto-emits Tag 1 (16-bit
    // running-sum BCC checksum, mandated last) and Tag 65 (UAS LS
    // Version Number, mandated present) when the caller didn't set
    // them — so a default-constructed record with a few typed fields
    // produces wire bytes that satisfy strict-compliance validation
    // out of the box. See `crates/tst-core/src/klv/st0601/mod.rs` for
    // the auto-emit logic.
    let encoded = encode_to_vec(&rec)?;
    println!("encoded {} bytes", encoded.len());

    // Round-trip verification under the strictest decoder. Because the
    // encoder emits Tag 1 + Tag 65 + Tag 2 first / Tag 1 last ordering,
    // `decode_strict_compliance` accepts the output. If this assertion
    // ever fails on a default-constructed record, the encoder and
    // decoder are out of sync — a useful CI signal beyond the
    // structural unit tests.
    let decoded = decode_strict_compliance(&encoded)?;

    // Integer- / string-valued tags round-trip exactly: Tag 2 is a raw
    // 8-byte microsecond count, the string tags are byte-identical
    // copies. So the equality checks below assert *bit-identical*
    // recovery — no quantization on these fields.
    assert_eq!(decoded.timestamp_us, rec.timestamp_us);
    assert_eq!(decoded.platform_designation, rec.platform_designation);
    assert_eq!(decoded.image_coordinate_system, rec.image_coordinate_system);

    // Floats don't round-trip exactly through ST 0601's bounded numeric
    // encoding (the typed tags exercised here all use `LinearRange` —
    // see `klv::st0601::mapping`; some other ST 0601 tags use ST 1201.5
    // IMAPB instead, exposed as the separate `klv::imapb` substrate).
    // Both schemes fix a bit-width and a value range, which together
    // quantize to a finite step. The eps values here all sit well above
    // the actual per-tag resolution (verified against
    // crates/tst-core/src/klv/st0601/{tags,mapping}.rs):
    //   - heading 0..360 unsigned in 2 bytes → 360/65535 ≈ 5.49e-3°/step
    //     → eps 0.01 leaves ~2x headroom.
    //   - sensor lat ±90 signed in 4 bytes  → 180/(2·(2³¹−1)) ≈ 4.19e-8°/step.
    //   - sensor lon ±180 signed in 4 bytes → 360/(2·(2³¹−1)) ≈ 8.38e-8°/step.
    //     eps 1e-6 leaves >10x headroom on both.
    //   - frame-center elevation -900..19000 unsigned in 2 bytes
    //     → 19900/65535 ≈ 0.30 m/step → eps 0.5 leaves ~1.7x headroom.
    let approx_eq = |a: f64, b: f64, eps: f64| (a - b).abs() < eps;
    assert!(approx_eq(
        decoded.platform_heading_deg.unwrap(),
        rec.platform_heading_deg.unwrap(),
        0.01
    ));
    assert!(approx_eq(
        decoded.sensor_lat_deg.unwrap(),
        rec.sensor_lat_deg.unwrap(),
        1e-6
    ));
    assert!(approx_eq(
        decoded.sensor_lon_deg.unwrap(),
        rec.sensor_lon_deg.unwrap(),
        1e-6
    ));
    assert!(approx_eq(
        decoded.frame_center_elev_m.unwrap(),
        rec.frame_center_elev_m.unwrap(),
        0.5
    ));

    // The example never set Tag 1 or Tag 65, yet `decode_strict_compliance`
    // accepted the bytes — which proves the encoder filled them in
    // implicitly. That auto-emit is what lets producers stay focused on
    // payload tags without having to track checksum and version book-
    // keeping themselves.
    println!("OK: round-trip succeeded; encoder emitted Tag 1 + Tag 65 implicitly");
    println!(
        "    sensor_lat={:.6} (was {:.6})",
        decoded.sensor_lat_deg.unwrap(),
        rec.sensor_lat_deg.unwrap()
    );
    println!(
        "    heading_deg={:.3} (was {:.3})",
        decoded.platform_heading_deg.unwrap(),
        rec.platform_heading_deg.unwrap()
    );
    Ok(())
}
