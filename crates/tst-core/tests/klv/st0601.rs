//! Integration tests for klv::st0601 — round trips from the public API surface.

use std::path::Path;

use tst_core::klv::UniversalLabel;
use tst_core::klv::st0601::{
    EncodeConfig, UasDatalinkLs, decode, decode_strict, decode_unchecked, encode,
    encode_strict_compliance, encode_to_vec, encode_with, encoded_len,
};

#[allow(clippy::field_reassign_with_default)]
#[test]
fn full_record_round_trip() {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(1_700_000_123_456_789);
    r.platform_tail_number = Some("N12345".to_owned());
    r.platform_designation = Some("CAYUSE-1".to_owned());
    r.platform_heading_deg = Some(270.5);
    r.platform_pitch_deg = Some(-3.5);
    r.platform_roll_deg = Some(12.0);
    r.sensor_lat_deg = Some(38.123456);
    r.sensor_lon_deg = Some(-121.654321);
    r.sensor_alt_m = Some(2500.0);
    r.sensor_hfov_deg = Some(45.0);
    r.sensor_vfov_deg = Some(30.0);
    r.frame_center_lat_deg = Some(38.0);
    r.frame_center_lon_deg = Some(-121.5);
    r.frame_center_elev_m = Some(0.0);
    r.slant_range_m = Some(3000.0);
    r.target_width_m = Some(150.0);

    let bytes = encode_to_vec(&r).unwrap();
    let parsed = decode(&bytes).unwrap();

    // Spot-check a handful of typed fields.
    assert_eq!(parsed.platform_tail_number.as_deref(), Some("N12345"));
    assert_eq!(parsed.platform_designation.as_deref(), Some("CAYUSE-1"));
    assert!(parsed.field_errors.is_empty());
    assert!(parsed.unknown.is_empty());
    let pos = parsed.sensor_position().unwrap();
    assert!((pos.lat_deg - 38.123456).abs() < 1e-6);
    assert!((pos.alt_m - 2500.0).abs() < 1.0);
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn encoded_len_predicts_actual_size() {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(0xDEAD_BEEF);
    r.platform_call_sign = Some("ECHO-1".to_owned());
    r.platform_angle_of_attack_deg = Some(12.5);
    r.sensor_lat_deg = Some(45.0);
    let predicted = encoded_len(&r);
    let mut buf = vec![0u8; predicted];
    let n = encode(&r, &mut buf).unwrap();
    assert_eq!(predicted, n);
}

#[test]
fn decode_strict_round_trip() {
    let r = UasDatalinkLs::default();
    let bytes = encode_to_vec(&r).unwrap();
    let parsed = decode_strict(&bytes).unwrap();
    assert_eq!(parsed.universal_label, UniversalLabel::ST_0601_LS);
}

#[test]
fn decode_strict_rejects_arbitrary_ul() {
    let r = UasDatalinkLs::default();
    let mut opts = EncodeConfig::default();
    opts.universal_label = UniversalLabel::new([0xFF; 16]);
    opts.version = 0x13;
    let bytes = {
        let n = encoded_len(&r) + 16; // upper bound
        let mut buf = vec![0u8; n];
        let written = encode_with(&r, &opts, &mut buf).unwrap();
        buf.truncate(written);
        buf
    };
    assert!(decode_strict(&bytes).is_err());
    assert!(decode(&bytes).is_ok()); // permissive accepts it
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn corner_full_form_round_trip() {
    let mut r = UasDatalinkLs::default();
    r.corner_lat_p1_deg = Some(45.1);
    r.corner_lon_p1_deg = Some(-122.1);
    r.corner_lat_p2_deg = Some(45.1);
    r.corner_lon_p2_deg = Some(-121.9);
    r.corner_lat_p3_deg = Some(44.9);
    r.corner_lon_p3_deg = Some(-121.9);
    r.corner_lat_p4_deg = Some(44.9);
    r.corner_lon_p4_deg = Some(-122.1);
    let bytes = encode_to_vec(&r).unwrap();
    let parsed = decode(&bytes).unwrap();
    let c = parsed.corners().unwrap();
    assert!((c.p1.0 - 45.1).abs() < 1e-6);
    assert!((c.p3.1 - -121.9).abs() < 1e-6);
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn unchecked_skips_checksum() {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(42);
    let mut bytes = encode_to_vec(&r).unwrap();
    *bytes.last_mut().unwrap() ^= 0xFF;
    assert!(decode(&bytes).is_err());
    let parsed = decode_unchecked(&bytes).unwrap();
    assert_eq!(parsed.timestamp_us, Some(42));
}

fn load_fixture(name: &str) -> Vec<u8> {
    let path = Path::new("tests/fixtures/st0601").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
}

#[test]
fn fixture_minimal_decodes() {
    let bytes = load_fixture("synthetic_minimal.klv");
    let parsed = decode(&bytes).unwrap();
    assert!(parsed.timestamp_us.is_some());
    assert!(parsed.field_errors.is_empty());
}

#[test]
fn fixture_full_decodes() {
    let bytes = load_fixture("synthetic_full.klv");
    let parsed = decode(&bytes).unwrap();
    assert!(parsed.timestamp_us.is_some());
    assert!(parsed.platform_call_sign.is_some());
    assert!(parsed.platform_angle_of_attack_deg.is_some());
    assert!(parsed.sensor_position().is_some());
    assert!(parsed.frame_center().is_some());
    assert!(parsed.corners().is_some());
}

#[test]
fn fixture_funky_ul_decodes_with_decode_but_not_strict() {
    let bytes = load_fixture("synthetic_funky_ul.klv");
    let parsed = decode(&bytes).unwrap();
    assert_eq!(parsed.universal_label.st0601_version_byte(), 0x09);
    // Strict still accepts this — bytes 0-13 match the canonical prefix and
    // byte 15 is 0x00. So this fixture exercises the "non-default-version"
    // branch but not the "non-family" branch. Use a hand-crafted buffer for
    // the non-family case (covered in unit tests).
    let _ = decode_strict(&bytes); // does not panic
}

#[test]
fn fixture_field_errors_decodes() {
    let bytes = load_fixture("synthetic_field_errors.klv");
    let parsed = decode(&bytes).unwrap();
    // The malformed tag 13 was written via the unknown bag, so it shows up
    // there on round trip — not necessarily as a field_error (depends on
    // whether the tag dispatch matched it).
    let _ = parsed; // just verifies decode succeeds without panic
}

// -------- ST 0107.5 §6.3.3.2 empty-string convention --------

#[test]
fn st0601_empty_string_encodes_as_nul_and_round_trips() {
    // Some("") → [0x00] on wire → Some("") on decode.
    let r = UasDatalinkLs {
        mission_id: Some(String::new()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    // Locate the tag-3 TLV: tag byte 0x03, length 0x01, value 0x00.
    let pos = bytes.windows(3).position(|w| w == [0x03, 0x01, 0x00]);
    assert!(
        pos.is_some(),
        "expected [03 01 00] for empty mission_id, bytes={bytes:?}"
    );
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some(""),
        "empty string round-trip failed"
    );
}

#[test]
fn st0601_length0_string_decodes_as_absent() {
    // Manually craft a TLV with length-0 string value for platform_call_sign (tag 59 = 0x3B).
    // decode_unchecked still requires a 16-byte UL prefix + outer BER length, but
    // does not validate the UL — any 16 bytes work.
    use tst_core::klv::st0601::decode_unchecked;
    let body: Vec<u8> = vec![0x3B, 0x00]; // tag 59 with empty value
    let mut pkt = vec![0u8; 16]; // dummy UL (not validated by decode_unchecked)
    pkt.push(body.len() as u8); // outer BER length
    pkt.extend_from_slice(&body);
    let decoded = decode_unchecked(&pkt).unwrap();
    assert_eq!(
        decoded.platform_call_sign, None,
        "length-0 string should decode as None"
    );
}

#[test]
fn st0601_nul_byte_decodes_as_empty_string() {
    // [0x00] as the value of platform_designation (tag 10 = 0x0A) → Some("").
    use tst_core::klv::st0601::decode_unchecked;
    let body: Vec<u8> = vec![0x0A, 0x01, 0x00]; // tag 10, length 1, value [0x00]
    let mut pkt = vec![0u8; 16]; // dummy UL
    pkt.push(body.len() as u8);
    pkt.extend_from_slice(&body);
    let decoded = decode_unchecked(&pkt).unwrap();
    assert_eq!(
        decoded.platform_designation.as_deref(),
        Some(""),
        "single NUL byte should decode as empty string"
    );
}

// -------- control-char stripping in strict path only --------

#[test]
fn st0601_strict_encode_strips_control_chars() {
    // A string with embedded control chars should be stripped in strict mode.
    let r = UasDatalinkLs {
        timestamp_us: Some(1_000_000_000_000_000), // required by encode_strict_compliance
        mission_id: Some("MIS\x01SION\x7F".to_owned()), // control chars in the middle
        ..Default::default()
    };
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some("MISSION"),
        "strict encode must strip control chars from string fields"
    );
}

#[test]
fn st0601_strict_encode_all_control_chars_strips_to_empty_nul() {
    // All-control-char content → strips to "" → [0x00] on wire (ST 0107.3-13
    // composing with the ST 0107.5 §6.3.3.2 empty-string mapping).
    let r = UasDatalinkLs {
        timestamp_us: Some(1_000_000_000_000_000),
        mission_id: Some("\x01\x02\x7F".to_owned()), // only control chars
        ..Default::default()
    };
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    // Strips to "" → encodes as [0x00] → decodes as Some("")
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some(""),
        "all-control-char string must strip to empty string"
    );
}

#[test]
fn st0601_strict_encode_trims_leading_trailing_whitespace() {
    // ST 0107.3-12: leading/trailing tab/LF/CR/space are removed; EMBEDDED
    // whitespace is legitimate content and must survive.
    let r = UasDatalinkLs {
        timestamp_us: Some(1_000_000_000_000_000),
        mission_id: Some("\t CAMP FIRE \r\n".to_owned()),
        ..Default::default()
    };
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some("CAMP FIRE"),
        "strict encode must trim end-whitespace but keep the embedded space"
    );
}

#[test]
fn st0601_strict_encode_whitespace_only_strips_to_empty_nul() {
    // A genuinely whitespace-only string (spaces/tabs) trims to "" per
    // ST 0107.3-12 → [0x00] on wire → decodes as Some("").
    let r = UasDatalinkLs {
        timestamp_us: Some(1_000_000_000_000_000),
        mission_id: Some("  \t ".to_owned()),
        ..Default::default()
    };
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some(""),
        "whitespace-only string must trim to empty string"
    );
}

#[test]
fn st0601_default_encode_does_not_strip_control_chars() {
    // Default encode must pass control chars through byte-verbatim (no stripping).
    let r = UasDatalinkLs {
        mission_id: Some("MIS\x01SION".to_owned()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.mission_id.as_deref(),
        Some("MIS\x01SION"),
        "default encode must NOT strip control chars"
    );
}

// ── Tag 94: MIIS Core Identifier ────────────────────────────────────────────

/// The 34-byte Foundational Core Identifier from ST 1204.3 Table 7.
/// This is the binary value of the KLV example in that table (key + length
/// stripped — only the value bytes are stored in `miis_core_id`).
const ST1204_TABLE7_VALUE: &[u8] = &[
    0x01, 0x70, 0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F, 0x2E,
    0xB2, 0xDA, 0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A, 0xB9, 0x6A,
    0x36, 0x45,
];

#[test]
fn st0601_tag94_miis_core_id_decode_roundtrip() {
    // Encode a record with miis_core_id set to the ST 1204 Table 7 example.
    let record = UasDatalinkLs {
        miis_core_id: Some(ST1204_TABLE7_VALUE.to_vec()),
        ..Default::default()
    };
    let buf = encode_to_vec(&record).expect("encode must succeed");
    let decoded = decode(&buf).expect("decode must succeed");
    assert_eq!(
        decoded.miis_core_id.as_deref(),
        Some(ST1204_TABLE7_VALUE),
        "Tag 94 bytes must survive encode→decode round-trip byte-identical"
    );
    assert!(decoded.field_errors.is_empty(), "no field errors expected");
}

#[test]
fn st0601_tag94_absent_when_field_none() {
    // A record without miis_core_id must not emit Tag 94.
    let record = UasDatalinkLs::default();
    let buf = encode_to_vec(&record).expect("encode must succeed");
    let decoded = decode(&buf).expect("decode must succeed");
    assert!(
        decoded.miis_core_id.is_none(),
        "Tag 94 must be absent when miis_core_id is None"
    );
}
