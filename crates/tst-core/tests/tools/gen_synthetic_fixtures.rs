//! Generate synthetic ST 0601 fixture files into tests/fixtures/st0601/.
//! Run via: `cargo run -p tst-core --bin gen-synthetic-fixtures`.
//! Idempotent — running it again produces byte-identical output.

#![allow(clippy::field_reassign_with_default)]

use std::fs;
use std::path::Path;

use tst_core::klv::UniversalLabel;
use tst_core::klv::st0601::{
    EncodeConfig, UasDatalinkLs, encode_to_vec, encode_with, encoded_len_with,
};

fn main() {
    // CARGO_MANIFEST_DIR is now crates/tst-core (this is a [[bin]] in tst-core).
    // The fixtures live in this same crate's tests/ tree.
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/st0601");
    fs::create_dir_all(&out_dir).unwrap();

    write(&out_dir.join("synthetic_minimal.klv"), &minimal());
    write(&out_dir.join("synthetic_full.klv"), &full());
    write(&out_dir.join("synthetic_funky_ul.klv"), &funky_ul());
    write(
        &out_dir.join("synthetic_field_errors.klv"),
        &field_errors_record(),
    );
    println!("Wrote 4 synthetic fixtures to {}", out_dir.display());
}

fn write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
}

fn minimal() -> Vec<u8> {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(1_700_000_000_000_000);
    encode_to_vec(&r).unwrap()
}

fn full() -> Vec<u8> {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(1_700_123_456_789_000);
    r.mission_id = Some("M-001".to_owned());
    r.platform_tail_number = Some("N12345".to_owned());
    r.platform_designation = Some("DRONE-A".to_owned());
    r.image_source_sensor = Some("EO-NOSE".to_owned());
    r.image_coordinate_system = Some("WGS-84".to_owned());
    r.platform_call_sign = Some("ECHO-1".to_owned());
    r.platform_angle_of_attack_deg = Some(12.5);
    r.platform_heading_deg = Some(123.45);
    r.platform_pitch_deg = Some(-5.0);
    r.platform_roll_deg = Some(10.0);
    r.platform_true_airspeed = Some(45.0);
    r.platform_indicated_airspeed = Some(40.0);
    r.sensor_lat_deg = Some(38.123456);
    r.sensor_lon_deg = Some(-121.654321);
    r.sensor_alt_m = Some(2500.0);
    r.sensor_hfov_deg = Some(45.0);
    r.sensor_vfov_deg = Some(30.0);
    r.sensor_rel_az_deg = Some(180.0);
    r.sensor_rel_el_deg = Some(-45.0);
    r.sensor_rel_roll_deg = Some(0.0);
    r.slant_range_m = Some(3000.0);
    r.target_width_m = Some(150.0);
    r.frame_center_lat_deg = Some(38.0);
    r.frame_center_lon_deg = Some(-121.5);
    r.frame_center_elev_m = Some(0.0);
    r.corner_lat_offset_p1_deg = Some(0.01);
    r.corner_lon_offset_p1_deg = Some(0.01);
    r.corner_lat_offset_p2_deg = Some(0.01);
    r.corner_lon_offset_p2_deg = Some(-0.01);
    r.corner_lat_offset_p3_deg = Some(-0.01);
    r.corner_lon_offset_p3_deg = Some(-0.01);
    r.corner_lat_offset_p4_deg = Some(-0.01);
    r.corner_lon_offset_p4_deg = Some(0.01);
    r.corner_lat_p1_deg = Some(38.001);
    r.corner_lon_p1_deg = Some(-121.499);
    r.corner_lat_p2_deg = Some(38.001);
    r.corner_lon_p2_deg = Some(-121.501);
    r.corner_lat_p3_deg = Some(37.999);
    r.corner_lon_p3_deg = Some(-121.501);
    r.corner_lat_p4_deg = Some(37.999);
    r.corner_lon_p4_deg = Some(-121.499);
    r.generic_flag_data = Some(0x09);
    r.security_local_set = Some(vec![0x01, 0x02, 0x03]);
    encode_to_vec(&r).unwrap()
}

fn funky_ul() -> Vec<u8> {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(123);
    r.sensor_lat_deg = Some(45.0);
    let mut opts = EncodeConfig::default();
    opts.universal_label = UniversalLabel::new([
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x09, 0x00,
        0x00,
    ]);
    opts.version = 0x09;
    let n = encoded_len_with(&r, &opts);
    let mut buf = vec![0u8; n];
    let written = encode_with(&r, &opts, &mut buf).unwrap();
    buf.truncate(written);
    buf
}

fn field_errors_record() -> Vec<u8> {
    // Hand-assemble bytes: a Tag 13 (Sensor Latitude) declared with the
    // wrong length (3 bytes instead of the spec-required 4) inside an
    // otherwise-valid ST 0601 Local Set carrying a Tag 2 timestamp.
    //
    // The public encoder will not produce this — it rejects typed/reserved tags
    // routed through the `unknown` pass-through bag, so that path cannot
    // smuggle a malformed Tag 13 onto the wire. Building the bytes by hand
    // keeps the fixture intentional-malformation while letting the encoder
    // enforce conformance everywhere else.
    let mut body: Vec<u8> = Vec::new();
    // Tag 2 (Precision Time Stamp): 8-byte big-endian u64 = 789.
    body.extend_from_slice(&[0x02, 0x08]);
    body.extend_from_slice(&789u64.to_be_bytes());
    // Tag 65 (UAS LS Version Number): value 0x13 = 19, the
    // `EncodeConfig::default().version` the original `encode_to_vec`
    // path auto-emitted. Tag 65 is BER-OID-encoded as 0x41 because it
    // fits in 7 bits. Emitted before Tag 13 because the typed-encoder
    // pass (TAGS order) ran before the unknown pass — preserving that
    // wire order keeps this fixture byte-for-byte identical to the
    // committed file generated before the reserved-tag filter existed.
    body.extend_from_slice(&[0x41, 0x01, 0x13]);
    // Tag 13 (Sensor Latitude): declared len = 3 bytes, malformed
    // (spec = 4). Appended via the old `unknown` path, hence ordered
    // after Tag 65.
    body.extend_from_slice(&[0x0D, 0x03, 0x00, 0x00, 0x00]);

    let mut out: Vec<u8> = Vec::new();
    // 16-byte ST 0601 Universal Label.
    out.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    // Outer BER length covers body + Tag 1 checksum (tag+len+value = 4 bytes).
    let body_len_with_checksum = body.len() + 4;
    assert!(
        body_len_with_checksum < 128,
        "short-form BER assumed in this fixture"
    );
    out.push(body_len_with_checksum as u8);
    out.extend_from_slice(&body);
    // Tag 1 (Checksum), 2-byte big-endian running 16-bit sum.
    out.extend_from_slice(&[0x01, 0x02]);
    let cksum = checksum_16(&out);
    out.push((cksum >> 8) as u8);
    out.push(cksum as u8);
    out
}

/// 16-bit running-sum checksum per MISB ST 0601 §7.5, computed over all
/// bytes from the start of the UL through the Tag 1 length byte.
fn checksum_16(bytes: &[u8]) -> u16 {
    let mut sum: u16 = 0;
    for (i, b) in bytes.iter().enumerate() {
        if i & 1 == 0 {
            sum = sum.wrapping_add((*b as u16) << 8);
        } else {
            sum = sum.wrapping_add(*b as u16);
        }
    }
    sum
}
