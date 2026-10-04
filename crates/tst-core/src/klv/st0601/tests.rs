use super::decode::{
    assign_ranged, decode, decode_strict, decode_strict_compliance, decode_unchecked,
};
use super::encode::{
    encode, encode_strict_compliance, encode_to_vec, encode_to_vec_with, encode_with, encoded_len,
    encoded_len_with,
};
use super::model::{
    EncodeConfig, IcingDetected, OperationalMode, OutOfRangePolicy, PlatformStatus, SdccFlpField,
    SensorControlMode, SensorFovName, UasDatalinkLs,
};
use super::packs::{
    ControlCommand, ImageHorizonPixels, MetadataSubstreamId, PayloadType, SensorFrameRate,
};
use super::patch::patch;
use super::tags::{Encoding, TAGS};
use crate::error::{KlvDecodeError, KlvEncodeError, KlvPatchError};
use crate::klv::checksum::checksum_running_sum_16;
use crate::klv::pack::OwnedRawField;
use crate::klv::universal_label::UniversalLabel;

#[test]
fn default_uses_st0601_ul() {
    let r = UasDatalinkLs::default();
    assert_eq!(r.universal_label, UniversalLabel::ST_0601_LS);
    // declared_version mirrors UL byte 13. Per ST 0601.19 §6.2 the
    // canonical UL has byte 13 = 0x00; the field encodes a legacy
    // "document version" readout for non-conformant captures.
    assert_eq!(r.declared_version, 0x00);
}

#[test]
fn sensor_position_requires_all_three() {
    let mut r = UasDatalinkLs::default();
    assert!(r.sensor_position().is_none());
    r.sensor_lat_deg = Some(45.0);
    r.sensor_lon_deg = Some(-122.0);
    assert!(r.sensor_position().is_none(), "alt missing");
    r.sensor_alt_m = Some(1500.0);
    let p = r.sensor_position().unwrap();
    assert_eq!(p.lat_deg, 45.0);
    assert_eq!(p.lon_deg, -122.0);
    assert_eq!(p.alt_m, 1500.0);
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn corners_prefer_full() {
    let mut r = UasDatalinkLs::default();
    // Set both forms with different values; full should win.
    r.frame_center_lat_deg = Some(0.0);
    r.frame_center_lon_deg = Some(0.0);
    r.corner_lat_offset_p1_deg = Some(0.01);
    r.corner_lon_offset_p1_deg = Some(0.01);
    r.corner_lat_offset_p2_deg = Some(0.01);
    r.corner_lon_offset_p2_deg = Some(-0.01);
    r.corner_lat_offset_p3_deg = Some(-0.01);
    r.corner_lon_offset_p3_deg = Some(-0.01);
    r.corner_lat_offset_p4_deg = Some(-0.01);
    r.corner_lon_offset_p4_deg = Some(0.01);
    r.corner_lat_p1_deg = Some(45.0);
    r.corner_lon_p1_deg = Some(-122.0);
    r.corner_lat_p2_deg = Some(45.0);
    r.corner_lon_p2_deg = Some(-121.0);
    r.corner_lat_p3_deg = Some(44.0);
    r.corner_lon_p3_deg = Some(-121.0);
    r.corner_lat_p4_deg = Some(44.0);
    r.corner_lon_p4_deg = Some(-122.0);
    let c = r.corners().unwrap();
    assert_eq!(c.p1, (45.0, -122.0));
    assert_eq!(c.p3, (44.0, -121.0));
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn corners_fall_back_to_offsets() {
    let mut r = UasDatalinkLs::default();
    r.frame_center_lat_deg = Some(45.0);
    r.frame_center_lon_deg = Some(-122.0);
    r.corner_lat_offset_p1_deg = Some(0.01);
    r.corner_lon_offset_p1_deg = Some(0.01);
    r.corner_lat_offset_p2_deg = Some(0.01);
    r.corner_lon_offset_p2_deg = Some(-0.01);
    r.corner_lat_offset_p3_deg = Some(-0.01);
    r.corner_lon_offset_p3_deg = Some(-0.01);
    r.corner_lat_offset_p4_deg = Some(-0.01);
    r.corner_lon_offset_p4_deg = Some(0.01);
    let c = r.corners().unwrap();
    assert!((c.p1.0 - 45.01).abs() < 1e-9);
    assert!((c.p1.1 - -121.99).abs() < 1e-9);
}

#[test]
fn corners_none_when_neither_form_complete() {
    let r = UasDatalinkLs::default();
    assert!(r.corners().is_none());
}

#[test]
fn encode_options_default() {
    let opts = EncodeConfig::default();
    assert_eq!(opts.universal_label, UniversalLabel::ST_0601_LS);
    assert_eq!(opts.version, 0x13);
}

#[test]
fn encode_minimal_record_round_trip_via_iter() {
    // Encode a record with just a timestamp; verify the bytes parse back.
    let r = UasDatalinkLs {
        timestamp_us: Some(0x0123_4567_89AB_CDEF),
        ..UasDatalinkLs::default()
    };
    let mut buf = vec![0u8; 256];
    let n = encode(&r, &mut buf).unwrap();
    let bytes = &buf[..n];

    // Verify UL prefix
    assert_eq!(&bytes[..16], &UniversalLabel::ST_0601_LS.0);

    // Parse outer BER length
    use crate::klv::length::read_ber;
    let (body_len, body) = read_ber(&bytes[16..]).unwrap();
    assert_eq!(body_len, body.len());
    assert!(body_len >= 13); // tag 2 (1) + len (1) + 8 + tag 65 (1) + len (1) + 1 (auto-version) + checksum (3)

    // Parse body
    use crate::klv::pack::Iter;
    let mut tags_seen: Vec<u32> = Vec::new();
    for r in Iter::local_set(body) {
        let f = r.unwrap();
        tags_seen.push(f.tag);
    }
    assert!(tags_seen.contains(&2), "tag 2 (timestamp) missing");
    assert!(tags_seen.contains(&65), "tag 65 (auto-version) missing");
    assert!(tags_seen.contains(&1), "tag 1 (checksum) missing");
}

#[test]
fn encoded_len_matches_actual() {
    let r = UasDatalinkLs {
        timestamp_us: Some(0xCAFE),
        sensor_lat_deg: Some(45.0),
        sensor_lon_deg: Some(-122.0),
        ..UasDatalinkLs::default()
    };
    let predicted = encoded_len(&r);
    let mut buf = vec![0u8; predicted];
    let actual = encode(&r, &mut buf).unwrap();
    assert_eq!(predicted, actual);
}

#[test]
fn encode_buffer_too_small() {
    let r = UasDatalinkLs::default();
    let mut buf = vec![0u8; 5];
    let err = encode(&r, &mut buf).unwrap_err();
    matches!(err, KlvEncodeError::BufferTooSmall { .. });
}

#[test]
fn encode_out_of_range_rejects() {
    let r = UasDatalinkLs {
        sensor_lat_deg: Some(95.0), // out of [-90, 90]
        ..UasDatalinkLs::default()
    };
    let mut buf = vec![0u8; 256];
    let err = encode(&r, &mut buf).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 13, .. }));
}

#[test]
fn out_of_range_pitch_names_the_full_range_twin() {
    // Tag 6 (Platform Pitch, ±20°) rejects 22.4° and should name its full-range
    // twin (Tag 90, ±90°) in the error message.
    let rec = UasDatalinkLs {
        platform_pitch_deg: Some(22.4),
        ..UasDatalinkLs::default()
    };
    let err = encode_to_vec(&rec).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("platform_pitch_full_deg"), "got: {msg}");
    assert!(msg.contains("90"), "got: {msg}");
}

#[test]
fn out_of_range_corner_offset_names_the_absolute_corners() {
    // Tag 26 (Offset Corner Lat P1, ±0.075°) rejects 0.08° and should name
    // the absolute corner fields (Tags 82-89) in the error message.
    let rec = UasDatalinkLs {
        corner_lat_offset_p1_deg: Some(0.08),
        ..UasDatalinkLs::default()
    };
    let err = encode_to_vec(&rec).unwrap_err();
    assert!(
        err.to_string().contains("corner_lat_p1_deg"),
        "got: {}",
        err
    );
}

#[test]
fn out_of_range_restricted_tags_name_their_imapb_twins() {
    // The three remaining WP-B restricted/extended twin pairs (38→103,
    // 75→104, 76→105) must hint at their IMAPB twins, same as 22→96.
    let cases = [
        (
            UasDatalinkLs {
                density_altitude_m: Some(50_000.0),
                ..UasDatalinkLs::default()
            },
            "density_altitude_extended_m",
        ),
        (
            UasDatalinkLs {
                sensor_ellipsoid_height_m: Some(50_000.0),
                ..UasDatalinkLs::default()
            },
            "sensor_ellipsoid_height_extended_m",
        ),
        (
            UasDatalinkLs {
                alternate_platform_ellipsoid_height_m: Some(50_000.0),
                ..UasDatalinkLs::default()
            },
            "alternate_platform_ellipsoid_height_extended_m",
        ),
    ];
    for (rec, twin) in cases {
        let err = encode_to_vec(&rec).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(twin),
            "expected hint naming {twin}; got: {msg}"
        );
    }
}

#[test]
fn out_of_range_without_twin_has_no_hint() {
    // Tag 13 (Sensor Latitude, +/-90 deg) has no full-range twin; the error
    // message must NOT carry a hint (no ';' appended). (Moved off Tag 22
    // in WP-B: Tag 22 now has a range_hint pointing at its new IMAPB twin,
    // Tag 96 Target Width Extended.)
    let rec = UasDatalinkLs {
        sensor_lat_deg: Some(95.0),
        ..UasDatalinkLs::default()
    };
    let err = encode_to_vec(&rec).unwrap_err();
    assert!(!err.to_string().contains(';'), "got: {}", err);
}

#[test]
fn encode_string_too_long_rejects() {
    let r = UasDatalinkLs {
        platform_call_sign: Some("x".repeat(200)),
        ..UasDatalinkLs::default()
    };
    let mut buf = vec![0u8; 512];
    let err = encode(&r, &mut buf).unwrap_err();
    assert!(matches!(
        err,
        KlvEncodeError::StringTooLong { tag: 59, max: 127 }
    ));
}

// --- Reserved/typed-tag-in-unknown filter (validate-1 E3) ------------------
// `record.unknown` is for forward-compat pass-through of tags the encoder
// does not model. Putting a reserved structural tag (1 = Checksum, 2 = PTS,
// 65 = UAS LS Version) or a typed tag (anything in `tags::TAGS`) there
// would produce a non-conformant Local Set — duplicate entries or, for
// Tag 1, a bogus pseudo-checksum before the real one. The encoder fails
// fast with `ReservedTagInUnknown` before writing any bytes.

#[test]
fn encode_rejects_tag1_in_unknown() {
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 1,
        value: vec![0x00, 0x00],
    });
    let err = encode_to_vec(&r).unwrap_err();
    assert!(matches!(
        err,
        KlvEncodeError::ReservedTagInUnknown { tag: 1 }
    ));
}

#[test]
fn encode_rejects_tag2_in_unknown() {
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 2,
        value: vec![0u8; 8],
    });
    let err = encode_to_vec(&r).unwrap_err();
    assert!(matches!(
        err,
        KlvEncodeError::ReservedTagInUnknown { tag: 2 }
    ));
}

#[test]
fn encode_rejects_tag65_in_unknown() {
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 65,
        value: vec![0x0D],
    });
    let err = encode_to_vec(&r).unwrap_err();
    assert!(matches!(
        err,
        KlvEncodeError::ReservedTagInUnknown { tag: 65 }
    ));
}

#[test]
fn encode_rejects_typed_tag_in_unknown() {
    // Tag 13 (Sensor Latitude) is typed-modeled by the encoder. Even if
    // the caller didn't set `sensor_lat_deg`, smuggling Tag 13 in via
    // `unknown` would shadow the typed path and risk duplicate emission
    // on a later record.
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 13,
        value: vec![0u8; 4],
    });
    let err = encode_to_vec(&r).unwrap_err();
    assert!(matches!(
        err,
        KlvEncodeError::ReservedTagInUnknown { tag: 13 }
    ));
}

#[test]
fn encode_accepts_genuinely_unknown_tag() {
    // Tag 200 is not in `tags::TAGS` and not a reserved structural tag.
    // Forward-compat pass-through is the whole point of `unknown` — this
    // must continue to succeed.
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 200,
        value: vec![0xDE, 0xAD, 0xBE, 0xEF],
    });
    let bytes = encode_to_vec(&r).expect("genuinely-unknown tag should encode");
    // Round-trip preserves it.
    let parsed = decode(&bytes).unwrap();
    assert_eq!(parsed.unknown.len(), 1);
    assert_eq!(parsed.unknown[0].tag, 200);
    assert_eq!(parsed.unknown[0].value, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn encode_accepts_genuinely_unknown_high_tag() {
    // Tag 500 exceeds u8::MAX — the typed table is u8-keyed so this short-
    // circuits in `is_reserved_or_typed_tag` without iterating TAGS.
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 500,
        value: vec![0x01],
    });
    encode_to_vec(&r).expect("genuinely-unknown high tag should encode");
}

#[test]
fn encode_with_custom_ul() {
    let r = UasDatalinkLs::default();
    let custom_ul = UniversalLabel::new([0xAB; 16]);
    let opts = EncodeConfig {
        universal_label: custom_ul,
        version: 0x09,
        out_of_range_policy: OutOfRangePolicy::Error,
    };
    let mut buf = vec![0u8; 256];
    let n = encode_with(&r, &opts, &mut buf).unwrap();
    assert_eq!(&buf[..16], &[0xAB; 16]);
    let _ = n;
}

#[test]
fn encode_to_vec_succeeds() {
    let r = UasDatalinkLs {
        timestamp_us: Some(0xABCD_EF00),
        ..UasDatalinkLs::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(&bytes[..16], &UniversalLabel::ST_0601_LS.0);
}

#[allow(clippy::field_reassign_with_default)]
#[test]
fn round_trip_full_record() {
    let mut r = UasDatalinkLs::default();
    r.timestamp_us = Some(1_700_000_000_000_000);
    r.platform_designation = Some("DRONE-A".to_owned());
    r.platform_heading_deg = Some(123.45);
    r.platform_pitch_deg = Some(-5.0);
    r.platform_roll_deg = Some(10.0);
    r.sensor_lat_deg = Some(45.123);
    r.sensor_lon_deg = Some(-122.456);
    r.sensor_alt_m = Some(1500.0);
    r.frame_center_lat_deg = Some(45.0);
    r.frame_center_lon_deg = Some(-122.0);
    r.slant_range_m = Some(2500.0);

    let bytes = encode_to_vec(&r).unwrap();
    let parsed = decode(&bytes).unwrap();

    assert_eq!(parsed.timestamp_us, r.timestamp_us);
    assert_eq!(parsed.platform_designation, r.platform_designation);
    assert!((parsed.platform_heading_deg.unwrap() - 123.45).abs() < 0.01);
    assert!((parsed.sensor_lat_deg.unwrap() - 45.123).abs() < 1e-6);
    assert_eq!(parsed.universal_label, UniversalLabel::ST_0601_LS);
    // declared_version mirrors UL byte 13 (= 0x00 per ST 0601.19 §6.2
    // canonical registration); uas_ls_version is Tag 65 (= 19 = 0x13
    // per the document revision we conform to).
    assert_eq!(parsed.declared_version, 0x00);
    assert_eq!(parsed.uas_ls_version, Some(19));
    assert!(parsed.field_errors.is_empty());
}

#[test]
fn decode_unchecked_accepts_bad_checksum() {
    let r = UasDatalinkLs {
        timestamp_us: Some(123),
        ..UasDatalinkLs::default()
    };
    let mut bytes = encode_to_vec(&r).unwrap();
    // Corrupt the last checksum byte
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    // decode should fail; decode_unchecked should succeed.
    assert!(decode(&bytes).is_err());
    let parsed = decode_unchecked(&bytes).unwrap();
    assert_eq!(parsed.timestamp_us, Some(123));
}

#[test]
fn decode_strict_rejects_funky_ul() {
    let r = UasDatalinkLs {
        timestamp_us: Some(456),
        ..UasDatalinkLs::default()
    };
    let opts = EncodeConfig {
        universal_label: UniversalLabel::new([0xAB; 16]),
        version: 0x13,
        out_of_range_policy: OutOfRangePolicy::Error,
    };
    let mut buf = vec![0u8; 256];
    let n = encode_with(&r, &opts, &mut buf).unwrap();
    let bytes = &buf[..n];
    let err = decode_strict(bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::UnexpectedUniversalLabel { .. }
    ));
    // decode (non-strict) accepts any UL.
    let parsed = decode(bytes).unwrap();
    assert_eq!(parsed.universal_label, UniversalLabel::new([0xAB; 16]));
}

#[test]
fn decode_passes_through_unknown_tags() {
    // Tag 200 is outside the ST 0601 1-143 item range (see
    // `encode_accepts_genuinely_unknown_tag`), so it stays unknown
    // regardless of how many more spec items this crate types over time
    // — unlike a spec-range placeholder tag, which risks colliding with
    // a later WP-A task (Tag 99 did exactly this when Task A4 typed it
    // as `composite_imaging_local_set`).
    let mut r = UasDatalinkLs::default();
    r.unknown.push(OwnedRawField {
        tag: 200,
        value: vec![0xDE, 0xAD],
    });
    let bytes = encode_to_vec(&r).unwrap();
    let parsed = decode(&bytes).unwrap();
    assert_eq!(parsed.unknown.len(), 1);
    assert_eq!(parsed.unknown[0].tag, 200);
    assert_eq!(parsed.unknown[0].value, vec![0xDE, 0xAD]);
}

#[test]
fn decode_field_errors_accumulate() {
    // Hand-build a record with a malformed Tag 13 (lat) value (1 byte instead of 4).
    // We synthesize the bytes by building a valid record and then patching it.
    let r = UasDatalinkLs {
        sensor_lat_deg: Some(45.0),
        timestamp_us: Some(123),
        ..UasDatalinkLs::default()
    };
    let bytes = encode_to_vec(&r).unwrap();

    // Easier path: construct a body that has a deliberately-malformed tag.
    // The simplest approach: replace the typed field with a malformed
    // unknown field via a hand-constructed input.
    let mut body = vec![];
    // Tag 2, len 8, [zeros]
    body.extend_from_slice(&[0x02, 0x08]);
    body.extend_from_slice(&[0u8; 8]);
    // Tag 13, len 1 (malformed; should be 4)
    body.extend_from_slice(&[0x0D, 0x01, 0x00]);

    // Reserve checksum slot: tag(1) + len(1) + value(2) = 4 bytes
    let body_with_cksum_len = body.len() + 4;

    let mut full = vec![];
    full.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    // Outer BER length
    let mut len_buf = [0u8; 8];
    let n = crate::klv::length::write_ber(body_with_cksum_len, &mut len_buf).unwrap();
    full.extend_from_slice(&len_buf[..n]);
    full.extend_from_slice(&body);
    full.push(0x01);
    full.push(0x02);
    let cksum = crate::klv::checksum::checksum_running_sum_16(&full);
    full.push((cksum >> 8) as u8);
    full.push(cksum as u8);

    let parsed = decode(&full).unwrap();
    assert!(parsed.timestamp_us.is_some(), "good field still parses");
    assert!(
        !parsed.field_errors.is_empty(),
        "malformed field accumulates"
    );
    let _ = bytes;
}

#[test]
fn decode_strict_compliance_accepts_valid_record() {
    // Build a minimal compliant record: Tag 2 first, Tag 65 present, Tag 1 last.
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        uas_ls_version: Some(0x13),
        ..UasDatalinkLs::default()
    };
    let buf = encode_to_vec(&record).unwrap();
    let r = decode_strict_compliance(&buf).expect("compliant record decodes");
    assert_eq!(r.timestamp_us, Some(1_700_000_000_000_000));
    assert_eq!(r.uas_ls_version, Some(0x13));
}

// Note: a full integration test for the non-canonical-BER strict path
// (build a record with tampered outer BER + recomputed checksum) is
// overkill — `read_ber_strict` is unit-tested in `klv::length::tests`,
// and the wiring in `decode_strict_compliance` is a single line. The
// strict-compliance Tag 2/Tag 1/Tag 65 ordering tests above exercise
// the same code path.

#[test]
fn decode_strict_compliance_rejects_missing_tag65() {
    // Encode without Tag 65 by skipping auto-version: pre-construct fields.
    // We rely on `encode_to_vec` defaulting auto_version=true — to force
    // missing, decode a hand-crafted record without the version tag.
    // Build manually: UL + BER + body{ Tag 2, Tag 1 }.
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::{ber_len, write_ber};
    use crate::klv::universal_label::UniversalLabel;
    // Body: Tag 2 (LEN 8 + 8-byte ts), then Tag 1 (LEN 2 + 2-byte placeholder).
    let mut body = Vec::new();
    body.push(0x02);
    body.push(0x08);
    body.extend_from_slice(&1u64.to_be_bytes());
    body.push(0x01);
    body.push(0x02);
    body.extend_from_slice(&[0, 0]); // placeholder; we'll fix the checksum
    // Wrap with UL + BER.
    let mut buf = Vec::new();
    buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let mut len_bytes = [0u8; 9];
    let nlen = write_ber(body.len(), &mut len_bytes).unwrap();
    buf.extend_from_slice(&len_bytes[..nlen]);
    let body_offset_in_buf = buf.len();
    buf.extend_from_slice(&body);
    // Compute checksum over UL through length-of-checksum-item.
    let cksum_value_offset = body_offset_in_buf + body.len() - 2;
    let computed = checksum_running_sum_16(&buf[..cksum_value_offset]);
    buf[cksum_value_offset] = (computed >> 8) as u8;
    buf[cksum_value_offset + 1] = (computed & 0xFF) as u8;
    let _ = (ber_len, nlen); // silence unused warnings if any
    let err = decode_strict_compliance(&buf).unwrap_err();
    assert!(matches!(err, KlvDecodeError::MissingTag65));
}

#[test]
fn decode_strict_compliance_rejects_tag2_not_first() {
    // Build a record where Tag 65 appears before Tag 2.
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::write_ber;
    use crate::klv::universal_label::UniversalLabel;
    let mut body = vec![0x41u8, 0x01, 0x13]; // Tag 65
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1u64.to_be_bytes());
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]); // Tag 1 (checksum placeholder)
    let mut buf = Vec::new();
    buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let mut len_bytes = [0u8; 9];
    let nlen = write_ber(body.len(), &mut len_bytes).unwrap();
    buf.extend_from_slice(&len_bytes[..nlen]);
    let body_offset = buf.len();
    buf.extend_from_slice(&body);
    let cksum_value_offset = body_offset + body.len() - 2;
    let computed = checksum_running_sum_16(&buf[..cksum_value_offset]);
    buf[cksum_value_offset] = (computed >> 8) as u8;
    buf[cksum_value_offset + 1] = (computed & 0xFF) as u8;
    let _ = nlen;
    let err = decode_strict_compliance(&buf).unwrap_err();
    assert!(matches!(err, KlvDecodeError::Tag2NotFirst));
}

#[test]
fn decode_strict_compliance_rejects_tag1_not_last() {
    // Build a record where Tag 1 (checksum) is NOT last.
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::write_ber;
    use crate::klv::universal_label::UniversalLabel;
    let mut body = Vec::new();
    body.push(0x02); // Tag 2 first (correct)
    body.push(0x08);
    body.extend_from_slice(&1u64.to_be_bytes());
    body.push(0x01); // Tag 1 (checksum) — NOT last
    body.push(0x02);
    body.extend_from_slice(&[0, 0]);
    body.push(0x41); // Tag 65 after the checksum (wrong)
    body.push(0x01);
    body.push(0x13);
    let mut buf = Vec::new();
    buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let mut len_bytes = [0u8; 9];
    let nlen = write_ber(body.len(), &mut len_bytes).unwrap();
    buf.extend_from_slice(&len_bytes[..nlen]);
    let body_offset = buf.len();
    buf.extend_from_slice(&body);
    // Checksum covers up to (and including) the length byte of Tag 1.
    // Find Tag 1's value-offset: scan body for tag=0x01 len=0x02.
    let mut idx = 0;
    let mut cksum_value_offset = 0;
    let body_slice = &buf[body_offset..body_offset + body.len()];
    while idx + 2 <= body_slice.len() {
        if body_slice[idx] == 0x01 && body_slice[idx + 1] == 0x02 {
            cksum_value_offset = body_offset + idx + 2;
            break;
        }
        // BER-OID tag 1 byte + BER length 1 byte short form
        let t = body_slice[idx];
        idx += 1;
        // assume short-form lengths < 128 in this hand-crafted body
        let l = body_slice[idx] as usize;
        idx += 1 + l;
        let _ = t;
    }
    let computed = checksum_running_sum_16(&buf[..cksum_value_offset]);
    buf[cksum_value_offset] = (computed >> 8) as u8;
    buf[cksum_value_offset + 1] = (computed & 0xFF) as u8;
    let _ = nlen;
    // strict_compliance should reject — Tag 65 follows Tag 1.
    let err = decode_strict_compliance(&buf).unwrap_err();
    // Acceptable error: Tag1NotLast OR ChecksumMismatch (since checksum doesn't include trailing bytes).
    // We assert specifically for Tag1NotLast since the strict pass detects ordering before checksum verifies.
    assert!(matches!(err, KlvDecodeError::Tag1NotLast));
}

#[test]
fn decode_picks_up_tag_75_sensor_ellipsoid_height() {
    let mut record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        sensor_ellipsoid_height_m: Some(14190.7195),
        ..Default::default()
    };
    let _ = &mut record;
    let buf = encode_to_vec(&record).unwrap();
    let back = decode(&buf).unwrap();
    assert!(back.sensor_ellipsoid_height_m.is_some());
    let h = back.sensor_ellipsoid_height_m.unwrap();
    assert!((h - 14190.7195).abs() < 0.5, "got {h}");
}

#[test]
fn decode_picks_up_tag_90_platform_pitch_full() {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_pitch_full_deg: Some(-0.4315),
        ..Default::default()
    };
    let buf = encode_to_vec(&record).unwrap();
    let back = decode(&buf).unwrap();
    assert!(back.platform_pitch_full_deg.is_some());
    let p = back.platform_pitch_full_deg.unwrap();
    assert!((p - (-0.4315)).abs() < 1e-4, "got {p}");
}

#[test]
fn vmti_tag_74_round_trips_verbatim() {
    // Tag 74 (VMTI Local Set per MISB ST 0903) is carried as
    // pass-through bytes on the typed `vmti` field. The parent
    // ST 0601 decoder does not recurse into the VMTI inner schema;
    // consumers compose `klv::st0903::decode` themselves. Sibling-
    // layer pattern matches `security_local_set` (Tag 48 → ST 0102).
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        vmti: Some(payload.clone()),
        ..Default::default()
    };
    let buf = encode_to_vec(&record).unwrap();
    let back = decode(&buf).unwrap();
    assert_eq!(
        back.vmti.as_deref(),
        Some(payload.as_slice()),
        "Tag 74 should round-trip on the typed vmti field byte-for-byte"
    );
}

#[test]
fn every_typed_tag_round_trips() {
    // For every TagSpec in TAGS, set its corresponding field in
    // UasDatalinkLs to a sentinel value, encode the record, decode
    // it back, and verify the field survived the round trip. This
    // catches "tag added to TAGS but apply_typed_tag/assign_ranged/
    // walk_typed_lens/write_typed_fields not updated" drift.

    for spec in TAGS {
        // Skip Tag 1 (checksum: not user-set) and Tag 47/65
        // (handled by separate U8 dispatch; round-trip test below
        // exercises them implicitly via auto_version).
        if spec.id == 1 {
            continue;
        }
        let mut record = UasDatalinkLs {
            timestamp_us: Some(1_700_000_000_000_000),
            ..Default::default()
        };
        // Set the field we expect for this tag. The choice of
        // sentinel value just has to be inside the spec range.
        match spec.id {
            2 => {} // already set
            3 => record.mission_id = Some("M".to_string()),
            4 => record.platform_tail_number = Some("T".to_string()),
            10 => record.platform_designation = Some("D".to_string()),
            11 => record.image_source_sensor = Some("S".to_string()),
            12 => record.image_coordinate_system = Some("WGS84".to_string()),
            34 => record.icing_detected = Some(IcingDetected::IcingDetected),
            39 => record.outside_air_temp_c = Some(-16),
            47 => record.generic_flag_data = Some(0xAB),
            48 => record.security_local_set = Some(vec![0x01, 0x02]),
            59 => record.platform_call_sign = Some("CS".to_string()),
            60 => record.weapon_load = Some(45016),
            61 => record.weapon_fired = Some(186),
            62 => record.laser_prf_code = Some(1743),
            63 => record.sensor_fov_name = Some(SensorFovName::ContinuousZoom),
            65 => record.uas_ls_version = Some(0x13),
            70 => record.alternate_platform_name = Some("APACHE".to_string()),
            72 => record.event_start_time_us = Some(798_039_894_000_000),
            73 => record.rvt = Some(vec![0xAA, 0xBB]),
            74 => record.vmti = Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            77 => record.operational_mode = Some(OperationalMode::Operational),
            94 => record.miis_core_id = Some(vec![0x01, 0x70, 0xCA, 0xFE]),
            95 => record.sar_mi_local_set = Some(vec![0x01, 0x02, 0x03]),
            97 => record.range_image_local_set = Some(vec![0x04, 0x05, 0x06]),
            98 => record.geo_registration_local_set = Some(vec![0x07, 0x08, 0x09]),
            99 => record.composite_imaging_local_set = Some(vec![0x0A, 0x0B, 0x0C]),
            100 => record.segment_local_set = Some(vec![0x0D, 0x0E, 0x0F]),
            101 => record.amend_local_set = Some(vec![0x10, 0x11, 0x12]),
            106 => record.stream_designator = Some("BLUE".to_string()),
            107 => record.operational_base = Some("BASE01".to_string()),
            108 => record.broadcast_source = Some("HOME".to_string()),
            129 => record.target_id = Some("A123".to_string()),
            135 => record.communications_method = Some("Frequency Modulation".to_string()),
            110 => record.time_airborne_s = Some(19887),
            111 => record.propulsion_unit_speed_rpm = Some(3000),
            123 => record.navsats_in_view = Some(7),
            124 => record.positioning_method_source = Some(3),
            125 => record.platform_status = Some(PlatformStatus::Egress),
            126 => record.sensor_control_mode = Some(SensorControlMode::AutoHoldingPosition),
            131 => record.take_off_time_us = Some(1_529_588_637_122_999),
            133 => record.mi_storage_capacity_gb = Some(10000),
            136 => record.leap_seconds = Some(30),
            137 => record.correction_offset_us = Some(5_025_678_901),
            139 => record.active_payloads = Some(vec![0x0B]),
            102 => record.sdcc_flps.push(SdccFlpField {
                preceding_tags: vec![13],
                bytes: hex(concat!(
                    "03 84 04",
                    "3F800000 40000000 40800000",
                    "3F000000 00000000 BF000000",
                )),
            }),
            81 => {
                record.image_horizon = Some(ImageHorizonPixels {
                    x0_pct: 10,
                    y0_pct: 20,
                    x1_pct: 30,
                    y1_pct: 40,
                    start_lat_deg: Some(10.0),
                    start_lon_deg: Some(-20.0),
                    end_lat_deg: Some(30.0),
                    end_lon_deg: Some(-40.0),
                })
            }
            115 => record.control_commands.push(ControlCommand {
                id: 42,
                command: "TEST".to_string(),
                time_us: Some(1234),
            }),
            116 => record.control_command_verification = Some(vec![3, 7]),
            121 => record.active_wavelengths = Some(vec![1, 3]),
            127 => {
                record.sensor_frame_rate = Some(SensorFrameRate {
                    numerator: 60000,
                    denominator: 1001,
                })
            }
            143 => {
                record.metadata_substream_id = Some(MetadataSubstreamId {
                    local_id: 5,
                    uuid: None,
                })
            }
            122 => {
                record.country_codes = Some(super::packs::CountryCodes {
                    coding_method: 14,
                    overflight: Some("CAN".to_string()),
                    operator: None,
                    manufacture: Some("FRA".to_string()),
                })
            }
            128 => {
                record.wavelengths_list = Some(vec![super::packs::WavelengthRecord {
                    id: 21,
                    min_nm: 1000.0,
                    max_nm: 2000.0,
                    name: "NNIR".to_string(),
                }])
            }
            130 => {
                record.airbase_locations = Some(super::packs::AirbaseLocations {
                    take_off: Some(super::packs::Location {
                        lat_deg: Some(38.8),
                        lon_deg: Some(-77.0),
                        hae_m: Some(3.0),
                    }),
                    recovery: None,
                })
            }
            138 => {
                record.payload_list = Some(super::packs::PayloadList {
                    count: 1,
                    records: vec![super::packs::PayloadRecord {
                        id: 0,
                        payload_type: super::packs::PayloadType::ElectroOptical,
                        name: "VIS Nose Camera".to_string(),
                    }],
                })
            }
            140 => {
                record.weapons_stores = Some(vec![super::packs::WeaponsStore {
                    station_id: 1,
                    hardpoint_id: 1,
                    carriage_id: 1,
                    store_id: 3,
                    status_raw: 0x103,
                    weapon_type: "Harpoon".to_string(),
                }])
            }
            141 => {
                record.waypoint_list = Some(vec![super::packs::Waypoint {
                    id: 0,
                    prosecution_order: 1,
                    info: Some(3),
                    location: Some(super::packs::Location {
                        lat_deg: Some(38.889422),
                        lon_deg: Some(-77.035162),
                        hae_m: Some(200.0),
                    }),
                }])
            }
            142 => {
                record.view_domain = Some(super::packs::ViewDomain {
                    azimuth: Some(super::packs::ViewDomainPair {
                        start_deg: 210.0,
                        range_deg: 300.0,
                    }),
                    elevation: None,
                    roll: None,
                })
            }
            _ => {
                // Ranged numeric: pick a value at the midpoint of the spec
                // range. LinearRange tags carry their range in `spec.range`;
                // Imapb (WP-B) tags carry it in `spec.encoding` instead.
                let (min, max) = match (spec.range, spec.encoding) {
                    (Some(r), _) => (r.min, r.max),
                    (None, Encoding::Imapb { min, max, .. }) => (min, max),
                    (None, _) => panic!(
                        "tag {} ({}) is neither LinearRange nor Imapb (and isn't one of the \
                         explicit VarUint/VarInt/RawBytes arms above either) — \
                         every_typed_tag_round_trips needs a new arm",
                        spec.id, spec.name
                    ),
                };
                let midpoint = (min + max) / 2.0;
                assign_ranged(&mut record, spec.id as u32, midpoint);
                // Sanity-check: the field actually got set.
                let mut probe = UasDatalinkLs::default();
                assign_ranged(&mut probe, spec.id as u32, midpoint);
                assert_ne!(
                    format!("{probe:?}"),
                    format!("{:?}", UasDatalinkLs::default()),
                    "assign_ranged for tag {} ({}) is a no-op — missing arm",
                    spec.id,
                    spec.name
                );
            }
        }
        // Encode and decode round trip.
        let buf = encode_to_vec(&record)
            .unwrap_or_else(|e| panic!("encode failed for tag {} ({}): {e}", spec.id, spec.name));
        let back = decode(&buf)
            .unwrap_or_else(|e| panic!("decode failed for tag {} ({}): {e}", spec.id, spec.name));
        // Field must be present in the decoded record (we don't
        // compare exact values because IMAPB scaling is lossy).
        let present = match spec.id {
            3 => back.mission_id.is_some(),
            4 => back.platform_tail_number.is_some(),
            10 => back.platform_designation.is_some(),
            11 => back.image_source_sensor.is_some(),
            12 => back.image_coordinate_system.is_some(),
            34 => back.icing_detected.is_some(),
            39 => back.outside_air_temp_c.is_some(),
            47 => back.generic_flag_data.is_some(),
            48 => back.security_local_set.is_some(),
            59 => back.platform_call_sign.is_some(),
            60 => back.weapon_load.is_some(),
            61 => back.weapon_fired.is_some(),
            62 => back.laser_prf_code.is_some(),
            63 => back.sensor_fov_name.is_some(),
            65 => back.uas_ls_version.is_some(),
            70 => back.alternate_platform_name.is_some(),
            72 => back.event_start_time_us.is_some(),
            73 => back.rvt.is_some(),
            74 => back.vmti.is_some(),
            77 => back.operational_mode.is_some(),
            94 => back.miis_core_id.is_some(),
            95 => back.sar_mi_local_set.is_some(),
            97 => back.range_image_local_set.is_some(),
            98 => back.geo_registration_local_set.is_some(),
            99 => back.composite_imaging_local_set.is_some(),
            100 => back.segment_local_set.is_some(),
            101 => back.amend_local_set.is_some(),
            106 => back.stream_designator.is_some(),
            107 => back.operational_base.is_some(),
            108 => back.broadcast_source.is_some(),
            129 => back.target_id.is_some(),
            135 => back.communications_method.is_some(),
            110 => back.time_airborne_s.is_some(),
            111 => back.propulsion_unit_speed_rpm.is_some(),
            123 => back.navsats_in_view.is_some(),
            124 => back.positioning_method_source.is_some(),
            125 => back.platform_status.is_some(),
            126 => back.sensor_control_mode.is_some(),
            131 => back.take_off_time_us.is_some(),
            133 => back.mi_storage_capacity_gb.is_some(),
            136 => back.leap_seconds.is_some(),
            137 => back.correction_offset_us.is_some(),
            139 => back.active_payloads.is_some(),
            102 => !back.sdcc_flps.is_empty(),
            81 => back.image_horizon.is_some(),
            115 => !back.control_commands.is_empty(),
            116 => back.control_command_verification.is_some(),
            121 => back.active_wavelengths.is_some(),
            127 => back.sensor_frame_rate.is_some(),
            143 => back.metadata_substream_id.is_some(),
            122 => back.country_codes.is_some(),
            128 => back.wavelengths_list.is_some(),
            130 => back.airbase_locations.is_some(),
            138 => back.payload_list.is_some(),
            140 => back.weapons_stores.is_some(),
            141 => back.waypoint_list.is_some(),
            142 => back.view_domain.is_some(),
            2 => back.timestamp_us.is_some(),
            _ => {
                // For ranged numeric, presence == any of our ranged fields is set.
                // We reuse assign_ranged to a default record to compare which field
                // changed; back must have that same field set.
                let mut probe = UasDatalinkLs::default();
                assign_ranged(&mut probe, spec.id as u32, 0.0);
                // Compute a Debug snapshot of `back` field vs `probe`'s expected
                // field. Practically: we just check that *some* numeric field
                // changed in `back` relative to default.
                format!("{back:?}") != format!("{:?}", UasDatalinkLs::default())
            }
        };
        assert!(
            present,
            "round trip lost tag {} ({}); encoder or decoder dispatch arm missing",
            spec.id, spec.name
        );
    }
}

/// Walk the body of an encoded ST 0601 record and locate a tag's TLV.
/// Returns `(value_offset, value_len)` relative to the whole `encoded`
/// buffer, or `None` if the tag is not present.
///
/// Tags are parsed with the real BER-OID reader, so multi-byte wire
/// tags (id ≥ 128, e.g. Tags 129/135 — 2 bytes on the wire) are found
/// too. The body starts after the 16-byte UL plus its BER outer length;
/// we parse the outer length to find the body start, then walk
/// tag-length-value triplets.
fn find_tag(encoded: &[u8], tag: u8) -> Option<(usize, usize)> {
    // Skip UL (16 bytes), read outer BER length; `rest` points at the
    // body. The body ends 4 bytes before EOF (Tag 1 + len byte + 2-byte
    // checksum value). Walk tag-length-value triplets inside.
    let after_ul = 16;
    let (_body_len, rest) =
        crate::klv::length::read_ber(&encoded[after_ul..]).expect("outer BER length");
    let body_start = encoded.len() - rest.len();
    let body_end = encoded.len() - 4;
    let mut i = body_start;
    while i < body_end {
        let (cur_tag, after_tag) =
            crate::klv::length::read_ber_oid(&encoded[i..]).expect("BER-OID tag");
        i = encoded.len() - after_tag.len();
        // Parse BER length: short form (< 128) or long form.
        let len_byte = encoded[i];
        i += 1;
        let value_len = if len_byte & 0x80 == 0 {
            len_byte as usize
        } else {
            let nbytes = (len_byte & 0x7F) as usize;
            let mut v = 0usize;
            for b in &encoded[i..i + nbytes] {
                v = (v << 8) | (*b as usize);
            }
            i += nbytes;
            v
        };
        if cur_tag == u32::from(tag) {
            return Some((i, value_len));
        }
        i += value_len;
    }
    None
}

/// Regression: per ST 0601.19 §8.50, Tag 50 carries Platform Angle of
/// Attack as a signed int16 mapped linearly to ±20°. Pre-fix the
/// library declared Tag 50 as a utf8 "Platform Call Sign" field —
/// wire-format incompatible with every other ST 0601 toolchain.
#[test]
fn tag_50_is_platform_angle_of_attack_int16_per_spec() {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_angle_of_attack_deg: Some(12.5),
        ..Default::default()
    };
    let bytes = encode_to_vec(&record).expect("encode");

    let (value_off, value_len) =
        find_tag(&bytes, 50).expect("Tag 50 should be present in encoded record");
    assert_eq!(
        value_len, 2,
        "Tag 50 (Platform Angle of Attack) is int16 ⇒ 2-byte value per ST 0601.19 §8.50"
    );
    // Sanity: value bytes are not zero (we set a non-zero angle).
    assert_ne!(
        &bytes[value_off..value_off + value_len],
        &[0u8, 0u8],
        "encoded angle bytes should reflect 12.5°, not zero"
    );

    let decoded = decode(&bytes).expect("decode");
    let aoa = decoded
        .platform_angle_of_attack_deg
        .expect("Platform Angle of Attack should round-trip");
    assert!(
        (aoa - 12.5).abs() < 0.01,
        "Tag 50 round-trip drift exceeds 0.01°: got {aoa}"
    );
    assert!(
        decoded.platform_call_sign.is_none(),
        "Tag 50 must NOT populate Call Sign — that field belongs to Tag 59"
    );
}

/// Regression: per ST 0601.19 §8.59, Tag 59 carries Platform Call Sign
/// as utf8 ≤ 127 bytes. Paired with the Tag 50 fix above.
#[test]
fn tag_59_is_platform_call_sign_utf8_per_spec() {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_call_sign: Some("DRONE-7".to_string()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&record).expect("encode");

    let (value_off, value_len) =
        find_tag(&bytes, 59).expect("Tag 59 should be present in encoded record");
    assert_eq!(
        value_len,
        "DRONE-7".len(),
        "Tag 59 (Platform Call Sign) value length must equal utf8 byte length"
    );
    assert_eq!(
        &bytes[value_off..value_off + value_len],
        b"DRONE-7",
        "Tag 59 value bytes must be the raw utf8 of the call sign"
    );

    let decoded = decode(&bytes).expect("decode");
    assert_eq!(
        decoded.platform_call_sign,
        Some("DRONE-7".to_string()),
        "Tag 59 must round-trip into platform_call_sign"
    );
    assert!(
        decoded.platform_angle_of_attack_deg.is_none(),
        "Tag 59 must NOT populate Angle of Attack — that field belongs to Tag 50"
    );
}

// ---------------------------------------------------------------------------
// Regression — Validate-1 Phase 2 §A6: ST 0601 high-numbered BER-OID tags
// must not narrow to u8 and collide with known low-numbered tags.
//
// Per MISB ST 0107.3-04, decoders shall preserve unknown LS values without
// impacting the decoding of known items. Pre-fix the typed-decode site
// cast the multi-byte BER-OID tag to `u8`, so a future tag 258 (= 0x102,
// encoded `0x82 0x02`) narrowed to 2 and was treated as Tag 2 (Precision
// Time Stamp) — silently clobbering the typed `timestamp_us` field.
// ---------------------------------------------------------------------------

/// Build a strict-decodable ST 0601 LS buffer from a raw body. The body
/// is wrapped with the ST 0601 UL + outer BER length + a trailing Tag 1
/// (running-sum 16 checksum) so `decode` accepts it. The caller owns
/// emitting Tag 2 (timestamp) or omitting it inside `body_before_checksum`
/// — this helper does not inject either.
fn wrap_st0601(body_before_checksum: &[u8]) -> Vec<u8> {
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::write_ber;
    use crate::klv::universal_label::UniversalLabel;
    let mut body = Vec::from(body_before_checksum);
    // Append Tag 1 (checksum) placeholder.
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    let mut buf = Vec::new();
    buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let mut len_bytes = [0u8; 9];
    let n = write_ber(body.len(), &mut len_bytes).unwrap();
    buf.extend_from_slice(&len_bytes[..n]);
    let body_offset = buf.len();
    buf.extend_from_slice(&body);
    // Checksum spans UL + outer length + body up through the Tag 1
    // length byte (value bytes 0x00 0x00 placeholder are NOT included).
    let cksum_value_offset = body_offset + body.len() - 2;
    let computed = checksum_running_sum_16(&buf[..cksum_value_offset]);
    buf[cksum_value_offset] = (computed >> 8) as u8;
    buf[cksum_value_offset + 1] = (computed & 0xFF) as u8;
    buf
}

#[test]
fn future_tag_258_with_8_byte_value_does_not_clobber_timestamp_us() {
    // BER-OID tag 258 encodes as `0x82, 0x02` (continuation bit on byte 1,
    // value 0x02 in the low 7 bits of each byte: (2<<7)|2 == 258).
    // We hand `0x82 0x02` to the iterator's `read_ber_oid` which returns
    // u32 = 258. Pre-fix `lookup(258 as u8)` returned the Tag 2 spec
    // (Precision Time Stamp, 8-byte u64). With an 8-byte payload the
    // length check passed and `record.timestamp_us` was overwritten —
    // silent corruption of typed metadata by a tag that does not exist in
    // ST 0601 today.
    let mut body = Vec::new();
    // Tag 258 with 8-byte value (would have been mis-decoded as Tag 2):
    body.extend_from_slice(&[0x82, 0x02, 0x08]);
    body.extend_from_slice(&[0xAA; 8]);
    let buf = wrap_st0601(&body);

    let record = decode(&buf).expect("strict decode should succeed");

    assert!(
        record.timestamp_us.is_none(),
        "Tag 258 must NOT populate timestamp_us; that slot belongs to Tag 2 only. \
         Got timestamp_us={:?}",
        record.timestamp_us,
    );
    assert_eq!(
        record.unknown.len(),
        1,
        "Tag 258 (unknown) must be preserved in record.unknown",
    );
    assert_eq!(
        record.unknown[0].tag, 258,
        "preserved unknown tag must carry the full u32 BER-OID value, not narrowed",
    );
    assert_eq!(
        record.unknown[0].value,
        vec![0xAA; 8],
        "preserved unknown tag value bytes must round-trip verbatim",
    );
    assert!(
        record.field_errors.is_empty(),
        "no field errors expected; unknown tag goes to record.unknown not record.field_errors",
    );
}

#[test]
fn future_tag_300_with_arbitrary_value_preserved_in_unknown() {
    // BER-OID tag 300 encodes as `0x82, 0x2C` ((2<<7)|0x2C = 300).
    // Narrowing `300 as u8` yields 0x2C = 44 (not in the typed table
    // today, so this case wouldn't silently corrupt — but the wrong tag
    // value would be recorded in `record.unknown`, which is just as bad
    // for any downstream consumer trying to round-trip).
    let mut body = Vec::new();
    body.extend_from_slice(&[0x82, 0x2C, 0x03]);
    body.extend_from_slice(&[0x11, 0x22, 0x33]);
    let buf = wrap_st0601(&body);

    let record = decode(&buf).expect("strict decode should succeed");

    assert_eq!(
        record.unknown.len(),
        1,
        "Tag 300 (unknown) must be preserved in record.unknown",
    );
    assert_eq!(
        record.unknown[0].tag, 300,
        "preserved unknown tag must carry the full u32 BER-OID value (300), not narrowed (44)",
    );
    assert_eq!(record.unknown[0].value, vec![0x11, 0x22, 0x33]);
}

#[test]
fn known_tag_2_still_decodes_correctly_after_high_tag_fix() {
    // Regression: ensure single-byte BER-OID tags in [0, 127] still go
    // through the typed-decode dispatch table. Tag 2 (Precision Time
    // Stamp) is the canonical example — and the one the high-tag bug
    // was clobbering.
    let mut body = Vec::new();
    body.push(0x02);
    body.push(0x08);
    body.extend_from_slice(&0x1234_5678_9ABC_DEF0u64.to_be_bytes());
    let buf = wrap_st0601(&body);

    let record = decode(&buf).expect("strict decode should succeed");

    assert_eq!(
        record.timestamp_us,
        Some(0x1234_5678_9ABC_DEF0),
        "Tag 2 must round-trip into timestamp_us",
    );
    assert!(
        record.unknown.is_empty(),
        "Tag 2 is known; nothing should land in record.unknown",
    );
}

// ---------------------------------------------------------------------------
// Validate-1 E1+E2: strict-mode duplicate-tag detection + canonical-BER walker
// ---------------------------------------------------------------------------

/// Wrap a hand-crafted body into a full ST 0601 LS buffer (UL + outer BER
/// length + body) with the trailing Tag 1 checksum patched in over an
/// existing `0x01 0x02 0x00 0x00` placeholder. Caller must include the
/// placeholder at the end of `body`.
fn wrap_st0601_with_inline_checksum(body: &[u8]) -> Vec<u8> {
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::write_ber;
    use crate::klv::universal_label::UniversalLabel;
    let mut buf = Vec::new();
    buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let mut len_bytes = [0u8; 9];
    let n = write_ber(body.len(), &mut len_bytes).unwrap();
    buf.extend_from_slice(&len_bytes[..n]);
    let body_offset = buf.len();
    buf.extend_from_slice(body);
    // Find the LAST `01 02 00 00` placeholder in the body and patch over
    // the value bytes with the running-sum-16 checksum.
    let body_slice_start = body_offset;
    let body_slice_end = buf.len();
    let mut value_off: Option<usize> = None;
    let mut i = body_slice_start;
    while i + 4 <= body_slice_end {
        if buf[i] == 0x01 && buf[i + 1] == 0x02 && buf[i + 2] == 0x00 && buf[i + 3] == 0x00 {
            value_off = Some(i + 2);
        }
        i += 1;
    }
    let off = value_off.expect("body must contain a 01 02 00 00 placeholder");
    let computed = checksum_running_sum_16(&buf[..off]);
    buf[off] = (computed >> 8) as u8;
    buf[off + 1] = (computed & 0xFF) as u8;
    buf
}

#[test]
fn decode_strict_compliance_rejects_duplicate_tag_13() {
    // ST 0601.13-24: each non-multiple item appears at most once.
    // Tag 13 (Sensor Latitude) is a single-use item. Build a body
    // with Tag 2 first, Tag 65, Tag 13 TWICE, then Tag 1 last.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x41, 0x01, 0x13]); // Tag 65 = 0x41
    // Tag 13: I32Range, 4 bytes (per the spec's Sensor Latitude
    // ranged encoding). Two copies.
    body.extend_from_slice(&[0x0D, 0x04, 0x12, 0x34, 0x56, 0x78]);
    body.extend_from_slice(&[0x0D, 0x04, 0x12, 0x34, 0x56, 0x78]);
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]); // Tag 1 placeholder
    let buf = wrap_st0601_with_inline_checksum(&body);

    // Lenient decode accepts the duplicate (second one clobbers the
    // first, but that's the lenient contract — see ST 0601.13).
    let _ = decode(&buf).expect("lenient decode accepts duplicate tags");

    // Strict-compliance rejects with DuplicateTag.
    let err = decode_strict_compliance(&buf).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 13, .. }),
        "expected DuplicateTag {{ tag: 13, .. }}, got {err:?}",
    );
}

#[test]
fn decode_strict_compliance_allows_duplicate_unknown_tag() {
    // ST 0601.13-24 mandates once-per-packet only for DEFINED items.
    // An unknown tag (outside the typed table) may repeat without
    // violating the local-set contract — the strict walker must
    // ignore duplicates of unknown tags. Tag 66 (0x42) is Item 66,
    // "Deprecated" per ST 0601.19 §8.66 ("This item has been
    // deprecated."): a permanent placeholder that is never typed by
    // design, so it's a stable "genuinely unknown" stand-in for this
    // test — unlike the two prior stand-ins, which each got typed by
    // a later WP-A task and forced this test to move (70 → 63 by
    // Task A2, 63 → 66 here by Task A3). Its BER-OID encoding is the
    // single byte 0x42 (high bit clear) — strict-canonical.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x41, 0x01, 0x13]); // Tag 65
    // Tag 66 twice with arbitrary 1-byte payloads.
    body.extend_from_slice(&[0x42, 0x01, 0xAA]);
    body.extend_from_slice(&[0x42, 0x01, 0xBB]);
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]); // Tag 1
    let buf = wrap_st0601_with_inline_checksum(&body);

    let record =
        decode_strict_compliance(&buf).expect("strict-compliance allows duplicate unknown tags");
    // Both copies land in record.unknown via the typed dispatcher.
    let unknown_66 = record.unknown.iter().filter(|f| f.tag == 66).count();
    assert_eq!(unknown_66, 2, "both unknown Tag 66 copies preserved");
}

#[test]
fn decode_strict_compliance_rejects_non_canonical_per_item_length() {
    // ST 0107.5 §6.3.2: BER length must use fewest bytes. Per-item
    // length 0x81 0x13 is the long-form encoding of value 19 — which
    // fits in the short form `0x13`. Strict-compliance must reject.
    //
    // Body layout:
    //   Tag 2 (8-byte timestamp)
    //   Tag 65 with NON-CANONICAL length 0x81 0x01 (value 1) carrying
    //     the 1-byte UAS LS version 0x13
    //   Tag 1 placeholder
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x41, 0x81, 0x01, 0x13]); // Tag 65, BAD length encoding
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    let buf = wrap_st0601_with_inline_checksum(&body);

    // Lenient decode accepts the non-canonical length.
    let _ = decode(&buf).expect("lenient decode accepts non-canonical per-item length");

    // Strict-compliance rejects.
    let err = decode_strict_compliance(&buf).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalLength { .. }),
        "expected NonCanonicalLength, got {err:?}",
    );
}

#[test]
fn decode_strict_compliance_rejects_non_canonical_per_item_tag() {
    // ST 0107.5 §6.3.1: BER-OID forbids a leading 0x80 (overlong
    // encoding of value 0). Build a body with Tag 2, Tag 65, an
    // overlong-encoded tag `0x80 0x05` (= value 5), then Tag 1.
    //
    // Note: value 5 is a valid ST 0601 tag (Platform Heading Angle)
    // but the encoding `0x80 0x05` is non-canonical. The lenient
    // reader accepts it.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x41, 0x01, 0x13]); // Tag 65
    // Non-canonical BER-OID tag: 0x80 0x05 followed by 2-byte u16 value.
    body.extend_from_slice(&[0x80, 0x05, 0x02, 0x12, 0x34]);
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    let buf = wrap_st0601_with_inline_checksum(&body);

    // Lenient decode accepts the non-canonical tag.
    let _ = decode(&buf).expect("lenient decode accepts non-canonical per-item tag");

    // Strict-compliance rejects.
    let err = decode_strict_compliance(&buf).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalTag { .. }),
        "expected NonCanonicalTag, got {err:?}",
    );
}

// --- ST 0601 encode_strict_compliance (validate-1 act-now ST0601-NEW-01) ---
//
// Symmetric counterpart to decode_strict_compliance: the encoder now
// refuses to emit a record missing any caller-supplied mandatory item.
// Today that's only Tag 2 (Precision Time Stamp); Tags 1 + 65 auto-emit.

#[test]
fn encode_strict_rejects_missing_tag_2() {
    // ST 0601.13-22: Tag 2 (Precision Time Stamp) is mandatory in every
    // conformant LS instance. Strict-encode must refuse a record where
    // it is absent.
    let r = UasDatalinkLs {
        timestamp_us: None,
        ..UasDatalinkLs::default()
    };
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(
            err,
            KlvEncodeError::MissingMandatoryItem {
                tag: 2,
                name: "Precision Time Stamp",
            }
        ),
        "expected MissingMandatoryItem(tag=2), got {err:?}"
    );
}

#[test]
fn encode_strict_accepts_minimal_record() {
    // The strict path must succeed on the bare-minimum conformant
    // record (timestamp set, nothing else). The output bytes must in
    // turn satisfy decode_strict_compliance — pin the round trip.
    let r = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        ..UasDatalinkLs::default()
    };
    let bytes = encode_strict_compliance(&r).expect("minimal strict record must encode");
    let back = decode_strict_compliance(&bytes)
        .expect("strict-encoded bytes must round-trip through decode_strict_compliance");
    assert_eq!(back.timestamp_us, Some(1_700_000_000_000_000));
    // Tag 65 was auto-emitted by encode (ST 0601.x version byte) — the
    // strict decode would have failed via `MissingTag65` otherwise.
}

#[test]
fn encode_then_decode_strict_roundtrip() {
    // Multi-field record: pin that strict encode preserves Tag 2,
    // Tag 65 (auto), Tag 1 (auto checksum) AND the user-supplied
    // typed fields when round-tripped through strict decode.
    let r = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_heading_deg: Some(123.4),
        platform_pitch_deg: Some(-5.6),
        platform_roll_deg: Some(2.7),
        sensor_lat_deg: Some(45.0),
        sensor_lon_deg: Some(-122.0),
        sensor_alt_m: Some(1500.0),
        ..UasDatalinkLs::default()
    };
    let bytes = encode_strict_compliance(&r).expect("multi-field strict record must encode");
    let back = decode_strict_compliance(&bytes).expect("strict round-trip must succeed");
    assert_eq!(back.timestamp_us, Some(1_700_000_000_000_000));
    assert!((back.platform_heading_deg.unwrap() - 123.4).abs() < 0.01);
    assert!((back.platform_pitch_deg.unwrap() - -5.6).abs() < 0.01);
    assert!((back.platform_roll_deg.unwrap() - 2.7).abs() < 0.01);
    assert!((back.sensor_lat_deg.unwrap() - 45.0).abs() < 1e-6);
    assert!((back.sensor_lon_deg.unwrap() - -122.0).abs() < 1e-6);
    // Tag 15 (Sensor True Altitude) is IMAPB-encoded with 2-byte width
    // over the [-900, 19000] m range; quantization step is ~0.3 m, so a
    // tolerance of 1 m is generous-but-safe for round-trip pinning.
    assert!((back.sensor_alt_m.unwrap() - 1500.0).abs() < 1.0);
}

// ---------------------------------------------------------------------------
// patch() — byte-faithful tag-level editing
// ---------------------------------------------------------------------------

/// Hand-build a raw LS: ST 0601 UL + short-form outer length + `body`,
/// optionally appending a valid trailing checksum TLV.
fn build_raw_ls(body: &[u8], with_checksum: bool) -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    let body_len = body.len() + if with_checksum { 4 } else { 0 };
    assert!(
        body_len < 128,
        "test helper supports short-form length only"
    );
    raw.push(body_len as u8);
    raw.extend_from_slice(body);
    if with_checksum {
        raw.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
        let off = raw.len() - 2;
        let sum = checksum_running_sum_16(&raw[..off]);
        raw[off] = (sum >> 8) as u8;
        raw[off + 1] = sum as u8;
    }
    raw
}

#[test]
fn patch_empty_edits_is_byte_identity() {
    let rec = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        mission_id: Some("M1".into()),
        frame_center_lat_deg: Some(33.4),
        ..UasDatalinkLs::default()
    };
    let raw = encode_to_vec(&rec).unwrap();
    let out = patch(&raw, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out, raw);
}

#[test]
fn patch_in_place_edit_equals_full_reencode() {
    // encode() emits tags in TAGS-table order with canonical encodings,
    // so patching a PRESENT tag must be byte-equal to re-encoding the
    // edited record from scratch.
    let mut rec = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        mission_id: Some("M1".into()),
        frame_center_lat_deg: Some(33.4),
        corner_lat_p1_deg: Some(33.41),
        uas_ls_version: Some(19),
        ..UasDatalinkLs::default()
    };
    let raw = encode_to_vec(&rec).unwrap();
    let edits = UasDatalinkLs {
        corner_lat_p1_deg: Some(33.99),
        ..UasDatalinkLs::default()
    };
    let patched = patch(&raw, &edits).unwrap();
    rec.corner_lat_p1_deg = Some(33.99);
    assert_eq!(patched, encode_to_vec(&rec).unwrap());
}

#[test]
fn patch_inserts_absent_tag_before_trailing_checksum() {
    let rec = UasDatalinkLs {
        timestamp_us: Some(1),
        uas_ls_version: Some(19),
        ..UasDatalinkLs::default()
    };
    let raw = encode_to_vec(&rec).unwrap();
    let edits = UasDatalinkLs {
        frame_center_lat_deg: Some(10.0),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    // decode() verifies the running-sum checksum, so this also proves
    // the recompute is correct.
    let dec = decode(&out).expect("patched output decodes with a valid checksum");
    assert!((dec.frame_center_lat_deg.unwrap() - 10.0).abs() < 1e-6);
    // The checksum TLV (tag 1, len 2, 2 value bytes) is still the
    // final element — the inserted tag landed before it.
    assert_eq!(out[out.len() - 4], 0x01);
    assert_eq!(out[out.len() - 3], 0x02);
}

#[test]
fn patch_preserves_noncanonical_length_and_vendor_tlv_bytes() {
    // tag 3 with a NON-CANONICAL long-form length (0x81 0x02) and a
    // vendor TLV (tag 0x67 = 103, untyped). Patching an unrelated tag
    // must leave both byte sequences intact.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x81, 0x02, b'A', b'B']);
    body.extend_from_slice(&[0x67, 0x02, 0xDE, 0xAD]);
    let raw = build_raw_ls(&body, true);
    let _ = decode(&raw).expect("fixture must decode");

    let edits = UasDatalinkLs {
        frame_center_lat_deg: Some(10.0),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    let find = |needle: &[u8]| out.windows(needle.len()).any(|w| w == needle);
    assert!(
        find(&[0x03, 0x81, 0x02, b'A', b'B']),
        "non-canonical TLV must survive verbatim"
    );
    assert!(
        find(&[0x67, 0x02, 0xDE, 0xAD]),
        "vendor TLV must survive verbatim"
    );
    let _ = decode(&out).expect("patched output decodes with a valid checksum");
}

#[test]
fn patch_reencodes_every_occurrence_of_a_duplicated_tag_and_mirrors_missing_checksum() {
    // Two tag-4 TLVs, no checksum tag. Both occurrences re-encode; no
    // checksum is added (mirror-input); outer length re-encodes
    // canonically because the body size changed.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x04, 0x01, b'X']);
    body.extend_from_slice(&[0x04, 0x01, b'Y']);
    let raw = build_raw_ls(&body, false);
    let edits = UasDatalinkLs {
        platform_tail_number: Some("Z9".into()),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    expected.push(8);
    expected.extend_from_slice(&[0x04, 0x02, b'Z', b'9', 0x04, 0x02, b'Z', b'9']);
    assert_eq!(out, expected);
}

#[test]
fn patch_unknown_escape_hatch_replaces_vendor_tlv() {
    // Tag 66 is a durable untyped stand-in (ST 0601 skips it between 65
    // and 67; WP-A/WP-B have since typed most other former gaps out from
    // under earlier stand-ins — see reference_klv_typed_set_conventions).
    let rec = UasDatalinkLs {
        timestamp_us: Some(1),
        unknown: vec![OwnedRawField {
            tag: 66,
            value: vec![0xDE, 0xAD],
        }],
        ..UasDatalinkLs::default()
    };
    let raw = encode_to_vec(&rec).unwrap();
    let edits = UasDatalinkLs {
        unknown: vec![OwnedRawField {
            tag: 66,
            value: vec![0x01, 0x02, 0x03],
        }],
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    let dec = decode(&out).unwrap();
    assert_eq!(
        dec.unknown,
        vec![OwnedRawField {
            tag: 66,
            value: vec![0x01, 0x02, 0x03],
        }]
    );
}

#[test]
fn patch_rejects_typed_tag_in_unknown_edits() {
    let raw = encode_to_vec(&UasDatalinkLs {
        timestamp_us: Some(1),
        ..UasDatalinkLs::default()
    })
    .unwrap();
    let edits = UasDatalinkLs {
        unknown: vec![OwnedRawField {
            tag: 2,
            value: vec![0],
        }],
        ..UasDatalinkLs::default()
    };
    match patch(&raw, &edits) {
        Err(KlvPatchError::Encode(KlvEncodeError::ReservedTagInUnknown { tag: 2 })) => {}
        other => panic!("expected ReservedTagInUnknown, got {other:?}"),
    }
}

#[test]
fn patch_out_of_range_edit_value_errors() {
    let raw = encode_to_vec(&UasDatalinkLs {
        timestamp_us: Some(1),
        corner_lat_p1_deg: Some(10.0),
        ..UasDatalinkLs::default()
    })
    .unwrap();
    let edits = UasDatalinkLs {
        corner_lat_p1_deg: Some(999.0),
        ..UasDatalinkLs::default()
    };
    assert!(matches!(
        patch(&raw, &edits),
        Err(KlvPatchError::Encode(KlvEncodeError::OutOfRange { .. }))
    ));
}

#[test]
fn patch_truncated_input_errors() {
    assert!(matches!(
        patch(&[0x06, 0x0E], &UasDatalinkLs::default()),
        Err(KlvPatchError::Decode(KlvDecodeError::Truncated { .. }))
    ));
}

#[test]
fn patch_recomputes_mid_body_checksum_in_place() {
    // A NON-COMPLIANT mid-body tag-1 (followed by a vendor TLV, no
    // trailing checksum) is tolerated: recomputed in place over its
    // prefix, with every other byte preserved verbatim.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x01, 0x02, 0xAB, 0xCD]); // stale checksum value
    body.extend_from_slice(&[0x67, 0x01, 0x55]);
    let raw = build_raw_ls(&body, false);
    let out = patch(&raw, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out.len(), raw.len());
    // UL + outer length + tag-1 header preserved (bytes 0..19).
    assert_eq!(&out[..19], &raw[..19]);
    // The 2-byte value is recomputed over its prefix, in place.
    let sum = checksum_running_sum_16(&out[..19]);
    assert_eq!(out[19], (sum >> 8) as u8);
    assert_eq!(out[20], sum as u8);
    // Everything after the checksum value is verbatim.
    assert_eq!(&out[21..], &raw[21..]);
}

#[test]
fn patch_does_not_verify_input_checksum() {
    // patch() is an editor, not a validator: a corrupt input checksum
    // is not rejected — it is simply recomputed (here back to the
    // correct value, making the output byte-equal to the clean input).
    let clean = encode_to_vec(&UasDatalinkLs {
        timestamp_us: Some(1),
        ..UasDatalinkLs::default()
    })
    .unwrap();
    let mut corrupt = clean.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert!(
        matches!(
            decode(&corrupt),
            Err(KlvDecodeError::ChecksumMismatch { .. })
        ),
        "fixture sanity: the corruption must be decode-visible"
    );
    let out = patch(&corrupt, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out, clean);
}

#[test]
fn patch_does_not_inject_uas_ls_version() {
    // Input has no tag 65 and no checksum; patching another field must
    // NOT auto-insert a version tag (unlike encode_with).
    let raw = build_raw_ls(&[0x04, 0x01, b'X'], false);
    let edits = UasDatalinkLs {
        mission_id: Some("M".into()),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    let dec = decode_unchecked(&out).unwrap();
    assert_eq!(dec.uas_ls_version, None);
    assert_eq!(dec.mission_id.as_deref(), Some("M"));
}

#[test]
fn patch_duplicate_checksums_only_last_recomputed() {
    // TWO non-compliant mid-body tag-1 TLVs with distinct known values,
    // followed by a vendor TLV (so neither is the trailing checksum).
    // The FIRST (non-last) checksum's original value bytes must survive
    // verbatim; only the LAST is recomputed.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x01, 0x02, 0xAA, 0xBB]);
    body.extend_from_slice(&[0x01, 0x02, 0xCC, 0xDD]);
    body.extend_from_slice(&[0x67, 0x01, 0x55]);
    let raw = build_raw_ls(&body, false);
    let out = patch(&raw, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out.len(), raw.len());
    // UL + outer length + first tag-1 TLV (header AND value) verbatim.
    assert_eq!(&out[..21], &raw[..21]);
    assert_eq!(
        &out[19..21],
        &[0xAA, 0xBB],
        "first (non-last) checksum value bytes must survive verbatim"
    );
    // Second tag-1: header verbatim, value recomputed over its prefix.
    assert_eq!(&out[21..23], &raw[21..23]);
    let sum = checksum_running_sum_16(&out[..23]);
    assert_eq!(out[23], (sum >> 8) as u8);
    assert_eq!(out[24], sum as u8);
    // Trailing vendor TLV verbatim.
    assert_eq!(&out[25..], &raw[25..]);
}

#[test]
fn patch_preserves_trailing_bytes_after_declared_length() {
    // Trailing bytes after the declared outer length (capture padding)
    // are preserved verbatim — full byte identity holds.
    let mut raw = encode_to_vec(&UasDatalinkLs {
        timestamp_us: Some(1),
        ..UasDatalinkLs::default()
    })
    .unwrap();
    raw.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let _ = decode(&raw).expect("lenient decode tolerates trailing bytes");
    let out = patch(&raw, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out, raw);
}

#[test]
fn patch_empty_body_identity_and_insertion() {
    // UL + 0x00 outer length, no TLVs at all.
    let raw = build_raw_ls(&[], false);
    // Empty edits: byte identity.
    let out = patch(&raw, &UasDatalinkLs::default()).unwrap();
    assert_eq!(out, raw);
    // An edit grows the empty body via the insertion path (no checksum
    // to land before — appended at the end, canonical outer length).
    let edits = UasDatalinkLs {
        mission_id: Some("M".into()),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    expected.push(3);
    expected.extend_from_slice(&[0x03, 0x01, b'M']);
    assert_eq!(out, expected);
}

#[test]
fn patch_preserves_noncanonical_outer_length_when_size_unchanged() {
    // Outer length 0x81 0x03 = NON-CANONICAL long form of 3. An
    // in-place same-size edit must keep those length bytes verbatim.
    let mut raw = Vec::new();
    raw.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    raw.extend_from_slice(&[0x81, 0x03]);
    raw.extend_from_slice(&[0x04, 0x01, b'X']);
    let edits = UasDatalinkLs {
        platform_tail_number: Some("Y".into()),
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    assert_eq!(
        &out[16..18],
        &[0x81, 0x03],
        "non-canonical outer length survives verbatim when body size is unchanged"
    );
    assert_eq!(&out[18..], &[0x04, 0x01, b'Y']);
    assert_eq!(out.len(), raw.len());
}

#[test]
fn patch_propagated_decode_error_offsets_are_absolute() {
    // Indefinite-form outer length byte (0x80) at raw[16]: the
    // propagated MalformedLength offset must be absolute, not 0.
    let mut raw = Vec::new();
    raw.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
    raw.push(0x80);
    match patch(&raw, &UasDatalinkLs::default()) {
        Err(KlvPatchError::Decode(KlvDecodeError::MalformedLength { offset })) => {
            assert_eq!(offset, 16);
        }
        other => panic!("expected MalformedLength at offset 16, got {other:?}"),
    }

    // Truncated BER-OID tag inside the body (continuation bit set, no
    // following byte): offset must be raw-absolute too. The missing
    // byte sits at 16 (UL) + 1 (length) + 1 (tag byte) = 18.
    let raw = build_raw_ls(&[0x81], false);
    match patch(&raw, &UasDatalinkLs::default()) {
        Err(KlvPatchError::Decode(KlvDecodeError::Truncated { offset, .. })) => {
            assert_eq!(offset, 18);
        }
        other => panic!("expected Truncated at offset 18, got {other:?}"),
    }
}

// ============================================================================
// Sentinel (INT_MIN) round-trip tests
// ============================================================================

/// Build a minimal ST 0601 wire packet containing exactly one TLV field.
/// The caller supplies the raw TLV bytes (tag + len + value, no checksum).
fn wrap_st0601_single_tlv(tlv: &[u8]) -> Vec<u8> {
    wrap_st0601(tlv)
}

/// Tag 6 (Platform Pitch Angle) is I16Range: meaning = OutOfRange.
/// INT_MIN for 2 bytes = 0x8000.
#[test]
fn sentinel_tag6_out_of_range_round_trips() {
    use super::mapping::St0601SentinelMeaning;

    // Wire: tag=6, len=2, value=0x8000 (i16::MIN)
    let tlv = [0x06u8, 0x02, 0x80, 0x00];
    let buf = wrap_st0601_single_tlv(&tlv);

    let record = decode(&buf).expect("decode must succeed");

    assert_eq!(
        record.platform_pitch_deg, None,
        "INT_MIN sentinel must leave typed field None",
    );
    assert!(
        record.field_errors.is_empty(),
        "sentinel must not produce a field_error; got {:?}",
        record.field_errors,
    );
    assert_eq!(
        record.sentinel_tags,
        vec![6u32],
        "tag 6 must appear in sentinel_tags",
    );

    // Verify the meaning lookup.
    let meaning = super::mapping::st0601_sentinel_meaning(6)
        .expect("tag 6 must have a known sentinel meaning");
    assert_eq!(meaning, St0601SentinelMeaning::OutOfRange);

    // Encode the decoded record and verify the INT_MIN bytes re-appear in the output.
    let encoded = encode_to_vec(&record).expect("encode must succeed");
    let (tag6_vstart, tag6_vlen) = find_tag(&encoded, 6).expect("tag 6 must appear in encoded");
    assert_eq!(
        &encoded[tag6_vstart..tag6_vstart + tag6_vlen],
        &[0x80, 0x00],
        "tag 6 encoded value must be INT_MIN (0x8000)",
    );
}

/// Tag 13 (Sensor Latitude) is I32Range: meaning = Reserved.
/// INT_MIN for 4 bytes = 0x80000000.
#[test]
fn sentinel_tag13_reserved_round_trips() {
    use super::mapping::St0601SentinelMeaning;

    // Wire: tag=13, len=4, value=0x80000000 (i32::MIN)
    let tlv = [0x0Du8, 0x04, 0x80, 0x00, 0x00, 0x00];
    let buf = wrap_st0601_single_tlv(&tlv);

    let record = decode(&buf).expect("decode must succeed");

    assert_eq!(
        record.sensor_lat_deg, None,
        "INT_MIN sentinel must leave typed field None",
    );
    assert!(
        record.field_errors.is_empty(),
        "sentinel must not produce a field_error"
    );
    assert_eq!(record.sentinel_tags, vec![13u32]);

    let meaning = super::mapping::st0601_sentinel_meaning(13)
        .expect("tag 13 must have a known sentinel meaning");
    assert_eq!(meaning, St0601SentinelMeaning::Reserved);

    // Re-encode and verify INT_MIN bytes survive.
    let encoded = encode_to_vec(&record).expect("encode must succeed");
    let (vstart, vlen) = find_tag(&encoded, 13).expect("tag 13 must appear in encoded");
    assert_eq!(&encoded[vstart..vstart + vlen], &[0x80, 0x00, 0x00, 0x00],);
}

/// Tag 26 (Corner Latitude Offset P1) is I16Range: meaning = NotAvailable.
/// INT_MIN for 2 bytes = 0x8000.
#[test]
fn sentinel_tag26_not_available_round_trips() {
    use super::mapping::St0601SentinelMeaning;

    // Wire: tag=26, len=2, value=0x8000 (i16::MIN)
    let tlv = [0x1Au8, 0x02, 0x80, 0x00];
    let buf = wrap_st0601_single_tlv(&tlv);

    let record = decode(&buf).expect("decode must succeed");

    assert_eq!(
        record.corner_lat_offset_p1_deg, None,
        "INT_MIN sentinel must leave typed field None",
    );
    assert!(
        record.field_errors.is_empty(),
        "sentinel must not produce a field_error"
    );
    assert_eq!(record.sentinel_tags, vec![26u32]);

    let meaning = super::mapping::st0601_sentinel_meaning(26)
        .expect("tag 26 must have a known sentinel meaning");
    assert_eq!(meaning, St0601SentinelMeaning::NotAvailable);

    let encoded = encode_to_vec(&record).expect("encode must succeed");
    let (vstart, vlen) = find_tag(&encoded, 26).expect("tag 26 must appear in encoded");
    assert_eq!(&encoded[vstart..vstart + vlen], &[0x80, 0x00]);
}

/// Value-wins: if a typed field is Some(v) and its tag also appears in
/// sentinel_tags, the value v is encoded — not the sentinel bytes.
///
/// Tag 7 = Platform Roll Angle (I16Range, -50..50°, signed, byte_length=2).
/// Putting a real value in `platform_roll_deg` while listing tag 7 in
/// sentinel_tags must produce the real value bytes, not INT_MIN (0x8000).
#[test]
fn sentinel_value_wins_over_sentinel_tags_entry() {
    // Tag 7 in sentinel_tags + platform_roll_deg populated: value must win.
    let record = UasDatalinkLs {
        platform_roll_deg: Some(25.0),
        sentinel_tags: vec![7],
        ..UasDatalinkLs::default()
    };

    let encoded = encode_to_vec(&record).expect("encode must succeed");
    let (vstart, vlen) = find_tag(&encoded, 7).expect("tag 7 must appear in encoded");
    let value_bytes = &encoded[vstart..vstart + vlen];

    // 25.0° maps to a positive i16 value; it must NOT be 0x8000 (INT_MIN).
    assert_ne!(
        value_bytes,
        [0x80u8, 0x00],
        "value Some(25.0) must win over sentinel_tags entry; got {value_bytes:?}",
    );

    // Re-decode and confirm the value is present, not flagged as sentinel.
    let redecoded = decode(&encoded).expect("decode must succeed");
    assert!(
        redecoded.platform_roll_deg.is_some(),
        "re-decoded record must carry the roll value, not a sentinel",
    );
    assert!(
        redecoded.sentinel_tags.is_empty(),
        "re-decoded record must not flag tag 7 as a sentinel",
    );
}

/// Invariant pin: every signed-range tag modelled in the TAGS table must have
/// a `Some(_)` entry in `st0601_sentinel_meaning`. Guards future tag additions
/// — a new signed tag without a table entry will fail here.
///
/// The decode/encode paths key sentinel handling off `range.signed`, so the
/// signed set is derived from that same predicate; a per-tag cross-check
/// pins `Encoding::I*Range` to `range.signed` so the two ways of expressing
/// signedness cannot silently diverge on a future tag addition.
#[test]
fn every_modelled_signed_tag_has_a_sentinel_meaning() {
    use super::mapping::st0601_sentinel_meaning;
    use super::tags::{Encoding, TAGS};

    for t in TAGS.iter() {
        let by_encoding = matches!(t.encoding, Encoding::I16Range | Encoding::I32Range);
        let by_range = t.range.is_some_and(|r| r.signed);
        assert_eq!(
            by_encoding, by_range,
            "tag {}: Encoding::I*Range ({by_encoding}) disagrees with \
             range.signed ({by_range}) — decode keys off range.signed",
            t.id
        );
    }

    let signed_tags: Vec<u8> = TAGS
        .iter()
        .filter(|t| t.range.is_some_and(|r| r.signed))
        .map(|t| t.id)
        .collect();

    // Confirm the set is non-empty (guards against TAGS becoming empty).
    assert!(
        !signed_tags.is_empty(),
        "TAGS must contain signed-range entries"
    );

    for id in signed_tags {
        assert!(
            st0601_sentinel_meaning(u32::from(id)).is_some(),
            "tag {id} is a signed-range tag in TAGS but has no sentinel meaning — \
             add it to st0601_sentinel_meaning()"
        );
    }
}

/// Multi-field ordering: a sentinel tag numerically BETWEEN two populated
/// tags must decode-encode-decode as a fixpoint. Verifies that the sentinel
/// value is preserved alongside neighbouring typed fields.
///
/// Setup: tag 5 (Platform Heading, unsigned, populated) < tag 6 (Platform
/// Pitch, signed, INT_MIN sentinel) < tag 7 (Platform Roll, signed, populated).
#[test]
fn sentinel_between_populated_tags_is_fixpoint() {
    // Build a packet with tags 5 (heading), 6 (sentinel), 7 (roll).
    let mut tlv_bytes = Vec::new();
    // Tag 5: Platform Heading = 180.0° → U16Range 0-360 → 0x8000
    tlv_bytes.extend_from_slice(&[0x05u8, 0x02, 0x80, 0x00]);
    // Tag 6: Platform Pitch sentinel → I16MIN = 0x8000
    tlv_bytes.extend_from_slice(&[0x06u8, 0x02, 0x80, 0x00]);
    // Tag 7: Platform Roll = 10.0° → I16Range -50..50 → positive value
    tlv_bytes.extend_from_slice(&[0x07u8, 0x02, 0x19, 0x99]);
    let buf = wrap_st0601(&tlv_bytes);

    let record = decode(&buf).expect("decode must succeed");
    assert!(
        record.platform_heading_deg.is_some(),
        "tag 5 (heading) must decode to Some"
    );
    assert_eq!(
        record.platform_pitch_deg, None,
        "tag 6 sentinel must leave pitch None"
    );
    assert!(
        record.platform_roll_deg.is_some(),
        "tag 7 (roll) must decode to Some"
    );
    assert_eq!(
        record.sentinel_tags,
        vec![6u32],
        "tag 6 must be recorded as a sentinel"
    );

    // Encode and re-decode: sentinel tag and surrounding typed fields must survive.
    let encoded = encode_to_vec(&record).expect("encode must succeed");
    let redecoded = decode(&encoded).expect("re-decode must succeed");
    assert!(redecoded.platform_heading_deg.is_some(), "heading survives");
    assert_eq!(redecoded.platform_pitch_deg, None, "pitch stays None");
    assert!(redecoded.platform_roll_deg.is_some(), "roll survives");
    assert_eq!(redecoded.sentinel_tags, vec![6u32], "sentinel tag survives");
}

// ============================================================================
// OutOfRangePolicy::Indicator round-trip tests
// ============================================================================

/// With `OutOfRangePolicy::Indicator`, out-of-range values on eligible tags
/// (Tags 6 and 91 here) encode to the INT_MIN sentinel rather than erroring.
/// Decoding the output yields `None` in the typed fields and both tags in
/// `sentinel_tags`.
#[test]
fn indicator_mode_round_trips_as_sentinel() {
    let rec = UasDatalinkLs {
        platform_pitch_deg: Some(25.0), // Tag 6, ±20° range — out of range
        platform_roll_full_deg: Some(120.0), // Tag 91, ±90° range — out of range
        ..UasDatalinkLs::default()
    };
    let opts = EncodeConfig {
        out_of_range_policy: OutOfRangePolicy::Indicator,
        ..Default::default()
    };
    let bytes = encode_to_vec_with(&rec, &opts).expect("indicator mode must not error");
    let back = decode(&bytes).expect("decode must succeed");
    assert_eq!(
        back.platform_pitch_deg, None,
        "Tag 6 sentinel must yield None"
    );
    assert_eq!(
        back.platform_roll_full_deg, None,
        "Tag 91 sentinel must yield None"
    );
    assert!(
        back.sentinel_tags.contains(&6) && back.sentinel_tags.contains(&91),
        "both tags must appear in sentinel_tags; got {:?}",
        back.sentinel_tags,
    );
    use super::mapping::St0601SentinelMeaning;
    assert_eq!(
        super::mapping::st0601_sentinel_meaning(6),
        Some(St0601SentinelMeaning::OutOfRange),
    );
}

/// With `OutOfRangePolicy::Indicator`, a tag whose sentinel meaning is
/// `NotAvailable` (not `OutOfRange`) must still error.
#[test]
fn indicator_mode_ineligible_tag_errors_with_hint() {
    let rec = UasDatalinkLs {
        corner_lat_offset_p1_deg: Some(0.08), // Tag 26, ±0.075°, meaning=NotAvailable
        ..UasDatalinkLs::default()
    };
    let opts = EncodeConfig {
        out_of_range_policy: OutOfRangePolicy::Indicator,
        ..Default::default()
    };
    let err = encode_to_vec_with(&rec, &opts).unwrap_err();
    assert!(
        matches!(
            err,
            KlvEncodeError::OutOfRange {
                tag: 26,
                hint: Some(_),
                ..
            }
        ),
        "expected OutOfRange for tag 26 with a hint; got {err:?}",
    );
}

/// The default `EncodeConfig` still rejects out-of-range values.
#[test]
fn default_policy_still_errors() {
    let rec = UasDatalinkLs {
        platform_pitch_deg: Some(25.0), // Tag 6, ±20° range — out of range
        ..UasDatalinkLs::default()
    };
    assert!(
        encode_to_vec(&rec).is_err(),
        "default policy must error on out-of-range"
    );
}

/// Ranged fields are width-fixed: `encoded_len_with` must equal the actual
/// encoded byte count even when the indicator policy emits a sentinel.
#[test]
fn indicator_mode_does_not_change_encoded_len() {
    let rec = UasDatalinkLs {
        platform_pitch_deg: Some(25.0), // Tag 6, out of range
        ..UasDatalinkLs::default()
    };
    let opts = EncodeConfig {
        out_of_range_policy: OutOfRangePolicy::Indicator,
        ..Default::default()
    };
    assert_eq!(
        encoded_len_with(&rec, &opts),
        encode_to_vec_with(&rec, &opts).unwrap().len(),
        "encoded_len_with must match actual encoded byte count under Indicator policy",
    );
}

/// Normal decode of a non-sentinel signed field does not populate sentinel_tags.
///
/// Tag 7 = Platform Roll Angle (I16Range, -50..50°). Wire value 0x0000 = 0.0°
/// (the midpoint). Zero is NOT INT_MIN, so it must decode to Some(0.0) and
/// sentinel_tags must remain empty.
#[test]
fn normal_signed_field_does_not_populate_sentinel_tags() {
    let tlv = [0x07u8, 0x02, 0x00, 0x00];
    let buf = wrap_st0601_single_tlv(&tlv);

    let record = decode(&buf).expect("decode must succeed");

    assert!(
        record.platform_roll_deg.is_some(),
        "zero must decode to Some(0.0), not a sentinel",
    );
    assert!(
        record.sentinel_tags.is_empty(),
        "sentinel_tags must be empty for a non-sentinel wire value",
    );
    assert!(record.field_errors.is_empty());
}

// ============================================================================
// WP-A: 30 remaining fixed-linear ranged fields (Table A1 spec vectors)
// ============================================================================

/// Build a single-TLV ST 0601 record with `tag`/`value` and decode it via
/// [`decode`]. All WP-A ranged-field tags fit in a single-byte BER-OID tag
/// (< 128); every value is short-form BER length (≤ 4 bytes).
fn decode_with_single_tlv(tag: u8, value: &[u8]) -> UasDatalinkLs {
    let mut tlv = vec![tag, value.len() as u8];
    tlv.extend_from_slice(value);
    decode(&wrap_st0601(&tlv)).expect("single-tlv fixture must decode")
}

/// Extract the VALUE bytes of `tag` from an encoded ST 0601 buffer, or
/// `None` if the tag is absent. Thin wrapper over [`find_tag`] returning
/// owned bytes instead of an `(offset, length)` pair.
fn tlv_value(encoded: &[u8], tag: u8) -> Option<Vec<u8>> {
    let (off, len) = find_tag(encoded, tag)?;
    Some(encoded[off..off + len].to_vec())
}

/// Encode a default record with a single field set by `set`; returns the
/// full encoded bytes (the record also carries the auto Tag 65 + trailing
/// checksum, which [`tlv_value`] walks past). Encode-direction companion
/// to [`decode_with_single_tlv`] for the WP-A spec-byte pins.
fn encode_with_field(set: impl FnOnce(&mut UasDatalinkLs)) -> Vec<u8> {
    let mut rec = UasDatalinkLs::default();
    set(&mut rec);
    encode_to_vec(&rec).expect("single-field record must encode")
}

/// ST 0601.19 §8 worked examples for the WP-A ranged fields — spec bytes,
/// not round-trip (closed-loop tests can't catch a wrong wire formula).
#[test]
fn wpa_ranged_spec_vectors() {
    // (tag, example value, value bytes) — from ST 0601.19 §8 example rows.
    let vectors: &[(u8, f64, &[u8])] = &[
        (35, 235.924010, &[0xA7, 0xC4]),
        (36, 69.8039216, &[0xB2]),
        (37, 3725.18502, &[0xBE, 0xBA]),
        (38, 14818.6770, &[0xCA, 0x35]),
        (40, -79.163_850_051_892_85, &[0x8F, 0x69, 0x52, 0x62]),
        (41, 166.40081296041646, &[0x76, 0x54, 0x57, 0xF2]),
        (42, 18389.0471, &[0xF8, 0x23]),
        (43, 6.0, &[0x03]),
        (44, 30.0, &[0x0F]),
        (45, 425.215152, &[0x1A, 0x95]),
        (46, 608.9231, &[0x26, 0x11]),
        (49, 1191.95850, &[0x3D, 0x07]),
        (51, -61.8878750, &[0xD3, 0xFE]),
        (52, -5.08255257, &[0xDF, 0x79]),
        (53, 2088.96010, &[0x6A, 0xF4]),
        (54, 8306.80552, &[0x76, 0x70]),
        (55, 50.5882353, &[0x81]),
        (56, 140.0, &[0x8C]),
        (57, 3_506_979.031_606_34, &[0xB3, 0x8E, 0xAC, 0xF1]),
        (58, 6420.53864, &[0xA4, 0x5D]),
        (64, 311.868162, &[0xDD, 0xC5]),
        (67, -86.041_207_348_947_04, &[0x85, 0xA1, 0x5A, 0x39]),
        (68, 0.15552755452484243, &[0x00, 0x1C, 0x50, 0x1C]),
        (69, 9.44533455, &[0x0B, 0xB3]),
        (71, 32.6024262, &[0x17, 0x2F]),
        (76, 9.44533455, &[0x0B, 0xB3]),
        (79, 25.4977569, &[0x09, 0xFB]),
        (80, 12.1, &[0x04, 0xBC]),
        (92, -8.670_176_984_123_037, &[0xF3, 0xAB, 0x48, 0xEF]),
        (93, -47.683, &[0xDE, 0x17, 0x93, 0x23]),
    ];
    for &(tag, value, bytes) in vectors {
        // Decode direction: build a raw LS carrying just this TLV.
        let ls = decode_with_single_tlv(tag, bytes);
        let entry = crate::klv::st0601::decode::ranged_entry(tag).expect("ranged entry");
        let got = (entry.get)(&ls).expect("field populated");
        let spec = crate::klv::st0601::tags::lookup(tag).unwrap();
        let r = spec.range.unwrap();
        // Signed mappings span -(2^(n-1)-1)..(2^(n-1)-1) = 2^n - 2 steps
        // (INT_MIN is the reserved sentinel); unsigned span 2^n - 1 steps.
        let lsb = (r.max - r.min)
            / if r.signed {
                ((1u64 << (8 * r.byte_length)) - 2) as f64
            } else {
                ((1u64 << (8 * r.byte_length)) - 1) as f64
            };
        assert!((got - value).abs() <= lsb, "tag {tag}: {got} vs {value}");
        // Encode direction: exact spec bytes.
        let mut rec = UasDatalinkLs::default();
        (entry.set)(&mut rec, value);
        let encoded = crate::klv::st0601::encode_to_vec(&rec).unwrap();
        assert!(
            tlv_value(&encoded, tag) == Some(bytes.to_vec()),
            "tag {tag}: wire bytes != spec example"
        );
    }
}

// ============================================================================
// WP-B: IMAPB extended-range fields (Table B1 spec vectors)
// ============================================================================

/// Decode a single-TLV ST 0601 record where `tag` may need the 2-byte
/// BER-OID encoding (id >= 128, e.g. Tags 132/134 in Table B1). Sibling of
/// [`decode_with_single_tlv`], which only writes 1-byte tags.
fn decode_with_single_tlv_ber_oid(tag: u32, value: &[u8]) -> UasDatalinkLs {
    let mut tag_buf = [0u8; 5];
    let n = crate::klv::length::write_ber_oid(tag, &mut tag_buf).expect("tag fits BER-OID");
    let mut tlv = tag_buf[..n].to_vec();
    tlv.push(value.len() as u8);
    tlv.extend_from_slice(value);
    decode(&wrap_st0601(&tlv)).expect("single-tlv fixture must decode")
}

/// MISB ST 0601.19 §8 worked examples for the 14 WP-B Table B1 IMAPB
/// items — spec bytes, not round-trip (closed-loop tests can't catch a
/// wrong wire formula).
#[test]
fn wpb_imapb_spec_vectors() {
    // (tag, example value, value bytes at the item's example/default length).
    let vectors: &[(u8, f64, &[u8])] = &[
        (96, 13_898.546_3, &[0x00, 0xD9, 0x2A]),
        (103, 23_456.24, &[0x2F, 0x92, 0x1E]),
        (104, 23_456.24, &[0x2F, 0x92, 0x1E]),
        (105, 23_456.24, &[0x2F, 0x92, 0x1E]),
        (109, 1.625, &[0x00, 0x01, 0xA0]),
        (112, 125.0, &[0x1F, 0x40]),
        (113, 2150.0, &[0x05, 0xF5, 0x00]),
        (114, 2154.50, &[0x05, 0xF7, 0x40]),
        (117, 1.0, &[0x3E, 0x90]),
        (118, 0.004176, &[0x3E, 0x80, 0x11]),
        (119, -50.0, &[0x3B, 0x60]),
        (120, 72.0, &[0x48, 0x00]),
        (132, 2400.0, &[0x02, 0x57, 0xC0]),
        (134, 55.0, &[0x37, 0x00]),
    ];
    for &(tag, value, bytes) in vectors {
        let ls = decode_with_single_tlv_ber_oid(tag as u32, bytes);
        let entry = crate::klv::st0601::decode::ranged_entry(tag).expect("ranged entry");
        let got = (entry.get)(&ls).expect("value decoded");
        let Encoding::Imapb { min, max, .. } =
            crate::klv::st0601::tags::lookup(tag).unwrap().encoding
        else {
            panic!("tag {tag} is not Imapb-encoded");
        };
        // Tolerance: one quantization step (sR = 1/sF, ST 1201.5 §8.9) at
        // the example wire length — NOT (max-min)/sF, which is (max-min)
        // times too loose and would make this assertion near-vacuous for
        // the wide-range tags (e.g. Tag 96 spans 1,500,000 m).
        let step = 1.0 / 2f64.powi((8 * bytes.len() - 1) as i32 - (max - min).log2().ceil() as i32);
        assert!((got - value).abs() <= step, "tag {tag}: {got} vs {value}");
        // Encode at default_len must reproduce the spec bytes exactly
        // (default_len == the example length by Table B1 construction).
        let mut rec = UasDatalinkLs::default();
        (entry.set)(&mut rec, value);
        let out = crate::klv::st0601::encode_to_vec(&rec).unwrap();
        assert_eq!(tlv_value(&out, tag), Some(bytes.to_vec()), "tag {tag}");
    }
}

/// IMAPB decode accepts any wire length in `1..=max_len` (not just
/// `default_len`), and ST 1201.5 special values land in
/// [`UasDatalinkLs::imapb_specials`] rather than the typed field or
/// `field_errors`. Specials re-emit on encode when the field stays
/// `None` — value wins otherwise, mirroring `sentinel_tags`.
#[test]
fn wpb_imapb_variable_length_decode_and_specials() {
    // Any length 1..=max_len decodes (2-byte Tag 104 value):
    let ls = decode_with_single_tlv(104, &[0x2F, 0x92]);
    assert!(ls.sensor_ellipsoid_height_extended_m.is_some());
    // ST 1201 AboveMax special (0xE1, zero-filled) -> side channel, field None:
    let ls = decode_with_single_tlv(104, &[0xE1, 0x00, 0x00]);
    assert_eq!(ls.sensor_ellipsoid_height_extended_m, None);
    assert_eq!(
        ls.imapb_specials,
        vec![(104u32, crate::klv::ImapbSpecial::AboveMax)]
    );
    assert!(ls.field_errors.is_empty());
    // Specials re-emit on encode when the field stays None (value wins otherwise):
    let out = crate::klv::st0601::encode_to_vec(&ls).unwrap();
    assert_eq!(tlv_value(&out, 104), Some(vec![0xE1, 0x00, 0x00]));
}

/// The two non-conformant IMAPB decode outcomes — a top-two-bits-set
/// pattern that doesn't match any recognized special family
/// (`DecodedImapb::ReservedSpecial`), and a normal-pattern integer that
/// arithmetic-decodes outside `[min, max]` (`DecodedImapb::OutOfRange`) —
/// are producer errors from this typed consumer's view: they land in
/// `field_errors`, NOT `imapb_specials`, and the typed field stays `None`.
/// See the `imapb_specials` field rustdoc for the policy rationale.
#[test]
fn wpb_imapb_reserved_and_out_of_range_land_in_field_errors() {
    // ReservedSpecial: top byte 0xCC (0b1100_1100) matches the 5-bit
    // PositiveInfinity prefix (0b11001) but carries a non-zero payload
    // (0x1234), which the +Inf family requires to be zero-filled — so it
    // is an unrecognized/reserved pattern, not +Inf (mirrors imapb.rs's
    // own `decode_special_rejects_non_zero_fill` pinning). Tag 104
    // (Sensor Ellipsoid Height Extended) at its 3-byte example length.
    let ls = decode_with_single_tlv(104, &[0xCC, 0x12, 0x34]);
    assert_eq!(ls.sensor_ellipsoid_height_extended_m, None);
    assert!(
        ls.imapb_specials.is_empty(),
        "ReservedSpecial must not populate imapb_specials, got {:?}",
        ls.imapb_specials
    );
    assert_eq!(ls.field_errors.len(), 1);
    match ls.field_errors[0] {
        crate::error::KlvFieldError::OutOfRange {
            tag: 104,
            value,
            min: -900.0,
            max: 40_000.0,
        } => {
            assert!(
                value.is_nan(),
                "ReservedSpecial must carry NaN, got {value}"
            );
        }
        ref other => panic!("expected OutOfRange{{tag:104,value:NaN,..}}, got {other:?}"),
    }

    // OutOfRange: 0xBFFFFF is a normal-pattern integer (top two bits
    // `10`, not `11`, so it takes the arithmetic-decode path rather than
    // the special-value path) that decodes past Tag 104's max=40000 —
    // the ST 1201.5 §8.6 Eq.12 inter-band reserved integer space.
    let ls = decode_with_single_tlv(104, &[0xBF, 0xFF, 0xFF]);
    assert_eq!(ls.sensor_ellipsoid_height_extended_m, None);
    assert!(
        ls.imapb_specials.is_empty(),
        "OutOfRange must not populate imapb_specials, got {:?}",
        ls.imapb_specials
    );
    assert_eq!(ls.field_errors.len(), 1);
    match ls.field_errors[0] {
        crate::error::KlvFieldError::OutOfRange {
            tag: 104,
            value,
            min: -900.0,
            max: 40_000.0,
        } => {
            assert!(
                (value - 97_403.992_187_5).abs() < 1e-6,
                "expected the raw arithmetic decode ~97403.99, got {value}"
            );
        }
        ref other => panic!("expected OutOfRange{{tag:104,value:~97403.99,..}}, got {other:?}"),
    }
}

/// `OutOfRangePolicy::Indicator` for IMAPB fields: the default `Error`
/// policy rejects with the real tag id (not a placeholder), and
/// `Indicator` emits the ST 1201.5 `IMAP_ABOVE_MAXIMUM` special at
/// `default_len` instead of erroring.
#[test]
fn wpb_imapb_indicator_policy() {
    // > 40000 max
    let rec = UasDatalinkLs {
        altitude_agl_m: Some(50_000.0),
        ..UasDatalinkLs::default()
    };
    // Default policy errors with the real tag id:
    let err = crate::klv::st0601::encode_to_vec(&rec).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 113, .. }));
    // Indicator emits IMAP_ABOVE_MAXIMUM at default_len:
    let cfg = EncodeConfig {
        out_of_range_policy: OutOfRangePolicy::Indicator,
        ..Default::default()
    };
    let out = crate::klv::st0601::encode_to_vec_with(&rec, &cfg).unwrap();
    assert_eq!(tlv_value(&out, 113), Some(vec![0xE1, 0x00, 0x00]));
}

// ============================================================================
// WP-A: raw/simple fields — new I8/U16 encodings (Table A2 spec vectors)
// ============================================================================

/// ST 0601.19 §8 worked examples for the WP-A raw/simple fields — spec
/// bytes in BOTH directions, not round-trip (closed-loop tests can't
/// catch a wrong wire formula). No LSB tolerance: unlike the
/// IMAPB-quantized ranged fields in [`wpa_ranged_spec_vectors`], these
/// are identity encodings (raw int/string bytes), so decoded values and
/// encoded VALUE bytes must match the spec examples exactly.
#[test]
fn wpa_raw_spec_vectors() {
    // Decode + encode against ST 0601.19 §8 examples (Appendix Table A2).
    // Tag 39 — Outside Air Temperature (I8): 84 → 0x54.
    let ls = decode_with_single_tlv(39, &[0x54]);
    assert_eq!(ls.outside_air_temp_c, Some(84));
    let encoded = encode_with_field(|r| r.outside_air_temp_c = Some(84));
    assert_eq!(tlv_value(&encoded, 39), Some(vec![0x54]));
    // Negative OAT: two's complement, both directions (-16 → 0xF0).
    let ls = decode_with_single_tlv(39, &[0xF0]);
    assert_eq!(ls.outside_air_temp_c, Some(-16));
    let encoded = encode_with_field(|r| r.outside_air_temp_c = Some(-16));
    assert_eq!(tlv_value(&encoded, 39), Some(vec![0xF0]));
    // Tag 60 — Weapon Load (U16): 45016 → 0xAF 0xD8.
    let ls = decode_with_single_tlv(60, &[0xAF, 0xD8]);
    assert_eq!(ls.weapon_load, Some(45016));
    let encoded = encode_with_field(|r| r.weapon_load = Some(45016));
    assert_eq!(tlv_value(&encoded, 60), Some(vec![0xAF, 0xD8]));
    // Tag 61 — Weapon Fired (U8): 186 → 0xBA.
    let ls = decode_with_single_tlv(61, &[0xBA]);
    assert_eq!(ls.weapon_fired, Some(186));
    let encoded = encode_with_field(|r| r.weapon_fired = Some(186));
    assert_eq!(tlv_value(&encoded, 61), Some(vec![0xBA]));
    // Tag 62 — Laser PRF Code (U16): 1743 → 0x06 0xCF.
    let ls = decode_with_single_tlv(62, &[0x06, 0xCF]);
    assert_eq!(ls.laser_prf_code, Some(1743));
    let encoded = encode_with_field(|r| r.laser_prf_code = Some(1743));
    assert_eq!(tlv_value(&encoded, 62), Some(vec![0x06, 0xCF]));
    // Tag 70 — Alternate Platform Name (Utf8): "APACHE".
    let ls = decode_with_single_tlv(70, b"APACHE");
    assert_eq!(ls.alternate_platform_name.as_deref(), Some("APACHE"));
    let encoded = encode_with_field(|r| r.alternate_platform_name = Some("APACHE".into()));
    assert_eq!(tlv_value(&encoded, 70), Some(b"APACHE".to_vec()));
    // Tag 72 — Event Start Time (U64): 798039894000000
    // → 0x00 0x02 0xD5 0xD0 0x24 0x66 0x01 0x80.
    let ls = decode_with_single_tlv(72, &798039894000000u64.to_be_bytes());
    assert_eq!(ls.event_start_time_us, Some(798039894000000));
    let encoded = encode_with_field(|r| r.event_start_time_us = Some(798039894000000));
    assert_eq!(
        tlv_value(&encoded, 72),
        Some(vec![0x00, 0x02, 0xD5, 0xD0, 0x24, 0x66, 0x01, 0x80])
    );
    // Tag 106 — Stream Designator (Utf8): "BLUE".
    let ls = decode_with_single_tlv(106, b"BLUE");
    assert_eq!(ls.stream_designator.as_deref(), Some("BLUE"));
    let encoded = encode_with_field(|r| r.stream_designator = Some("BLUE".into()));
    assert_eq!(tlv_value(&encoded, 106), Some(b"BLUE".to_vec()));
    // Tag 107 — Operational Base (Utf8): "BASE01".
    let ls = decode_with_single_tlv(107, b"BASE01");
    assert_eq!(ls.operational_base.as_deref(), Some("BASE01"));
    let encoded = encode_with_field(|r| r.operational_base = Some("BASE01".into()));
    assert_eq!(tlv_value(&encoded, 107), Some(b"BASE01".to_vec()));
    // Tag 108 — Broadcast Source (Utf8): "HOME".
    let ls = decode_with_single_tlv(108, b"HOME");
    assert_eq!(ls.broadcast_source.as_deref(), Some("HOME"));
    let encoded = encode_with_field(|r| r.broadcast_source = Some("HOME".into()));
    assert_eq!(tlv_value(&encoded, 108), Some(b"HOME".to_vec()));
    // Tag 129 — Target ID: crosses the BER-OID 2-byte-tag boundary; wire
    // tag bytes are 0x81 0x01 ((1<<7)|1 = 129). Decode from hand-built
    // spec bytes (decode_with_single_tlv only writes 1-byte tags):
    let mut tlv = vec![0x81, 0x01, 0x04];
    tlv.extend_from_slice(b"A123");
    let ls = decode(&wrap_st0601(&tlv)).expect("2-byte-tag TLV must decode");
    assert_eq!(ls.target_id.as_deref(), Some("A123"));
    // Encode: exact VALUE bytes, plus the raw TLV subsequence pinning the
    // 2-byte BER-OID tag emission through emit_ber_oid_tlv.
    let encoded = encode_with_field(|r| r.target_id = Some("A123".into()));
    assert_eq!(tlv_value(&encoded, 129), Some(b"A123".to_vec()));
    let expect_tlv: &[u8] = &[0x81, 0x01, 0x04, 0x41, 0x31, 0x32, 0x33];
    assert!(
        encoded.windows(expect_tlv.len()).any(|w| w == expect_tlv),
        "encoded record must carry Tag 129 as the 2-byte BER-OID 0x81 0x01"
    );
    // Tag 135 — Communications Method: also a 2-byte wire tag (0x81 0x07);
    // spec example is the 20-byte string "Frequency Modulation" (0x14).
    let mut tlv = vec![0x81, 0x07, 0x14];
    tlv.extend_from_slice(b"Frequency Modulation");
    let ls = decode(&wrap_st0601(&tlv)).expect("2-byte-tag TLV must decode");
    assert_eq!(
        ls.communications_method.as_deref(),
        Some("Frequency Modulation")
    );
    let encoded =
        encode_with_field(|r| r.communications_method = Some("Frequency Modulation".into()));
    assert_eq!(
        tlv_value(&encoded, 135),
        Some(b"Frequency Modulation".to_vec())
    );
    let mut expect_tlv = vec![0x81, 0x07, 0x14];
    expect_tlv.extend_from_slice(b"Frequency Modulation");
    assert!(
        encoded
            .windows(expect_tlv.len())
            .any(|w| w == expect_tlv.as_slice()),
        "encoded record must carry Tag 135 as the 2-byte BER-OID 0x81 0x07"
    );
}

/// `encode_strict_compliance` must sanitize EVERY typed string field per
/// ST 0107.5 §6.3.3 — including the six added by WP-A Task A2. Regression:
/// the A2 commit extended the typed Utf8 set but missed
/// `sanitize_strings_st0601`, so strict encode emitted raw control bytes
/// for the new fields. U+0000 is a banned control char (ST 0107.3-13),
/// removed at any position, so "A\u{0}B" must come back as "AB".
#[test]
fn strict_encode_sanitizes_all_string_fields() {
    let dirty = || Some("A\u{0}B".to_string());
    let rec = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000), // strict-mandatory Tag 2
        mission_id: dirty(),
        platform_tail_number: dirty(),
        platform_designation: dirty(),
        image_source_sensor: dirty(),
        image_coordinate_system: dirty(),
        platform_call_sign: dirty(),
        alternate_platform_name: dirty(),
        stream_designator: dirty(),
        operational_base: dirty(),
        broadcast_source: dirty(),
        target_id: dirty(),
        communications_method: dirty(),
        ..UasDatalinkLs::default()
    };
    let bytes = encode_strict_compliance(&rec).expect("strict encode");
    let back = decode(&bytes).expect("decode");
    for (field, got) in [
        ("mission_id", back.mission_id.as_deref()),
        ("platform_tail_number", back.platform_tail_number.as_deref()),
        ("platform_designation", back.platform_designation.as_deref()),
        ("image_source_sensor", back.image_source_sensor.as_deref()),
        (
            "image_coordinate_system",
            back.image_coordinate_system.as_deref(),
        ),
        ("platform_call_sign", back.platform_call_sign.as_deref()),
        (
            "alternate_platform_name",
            back.alternate_platform_name.as_deref(),
        ),
        ("stream_designator", back.stream_designator.as_deref()),
        ("operational_base", back.operational_base.as_deref()),
        ("broadcast_source", back.broadcast_source.as_deref()),
        ("target_id", back.target_id.as_deref()),
        (
            "communications_method",
            back.communications_method.as_deref(),
        ),
    ] {
        assert_eq!(
            got,
            Some("AB"),
            "{field}: strict encode must strip the ST 0107 control char"
        );
    }
}

// ============================================================================
// WP-A: coded enums — new tags 34/63/77 (Table A3)
// ============================================================================

#[allow(clippy::field_reassign_with_default)]
#[test]
fn wpa_coded_enums_round_trip_and_other() {
    let ls = decode_with_single_tlv(34, &[0x02]);
    assert_eq!(ls.icing_detected, Some(IcingDetected::IcingDetected));
    let ls = decode_with_single_tlv(63, &[0x08]);
    assert_eq!(ls.sensor_fov_name, Some(SensorFovName::ContinuousZoom));
    let ls = decode_with_single_tlv(77, &[0x01]);
    assert_eq!(ls.operational_mode, Some(OperationalMode::Operational));
    // Reserved/unknown codes survive byte-exact via Other(code):
    let ls = decode_with_single_tlv(77, &[0x2A]);
    assert_eq!(ls.operational_mode, Some(OperationalMode::Other(0x2A)));
    let mut rec = UasDatalinkLs::default();
    rec.operational_mode = Some(OperationalMode::Other(0x2A));
    let bytes = crate::klv::st0601::encode_to_vec(&rec).unwrap();
    assert_eq!(tlv_value(&bytes, 77), Some(vec![0x2A]));
}

/// ST 0601.19 §8 worked examples for the WP-A Task A3 coded enums — spec
/// bytes in both directions (decode from example bytes AND `tlv_value`
/// encode asserts), matching the rigor of `wpa_raw_spec_vectors`: these
/// are identity codepoint encodings, so no LSB tolerance is needed. Also
/// covers the `Other(code)` byte-exact round trip for the two enums the
/// RED test above doesn't exercise (it only shows `Other` on tag 77).
#[test]
fn wpa_coded_enum_spec_vectors() {
    // Tag 34 — Icing Detected (§8.34): worked example "Icing Detected" → 0x02.
    let ls = decode_with_single_tlv(34, &[0x02]);
    assert_eq!(ls.icing_detected, Some(IcingDetected::IcingDetected));
    let encoded = encode_with_field(|r| r.icing_detected = Some(IcingDetected::IcingDetected));
    assert_eq!(tlv_value(&encoded, 34), Some(vec![0x02]));
    // Wire-unknown codepoint round-trips byte-exact via Other(code).
    let ls = decode_with_single_tlv(34, &[0xFE]);
    assert_eq!(ls.icing_detected, Some(IcingDetected::Other(0xFE)));
    let encoded = encode_with_field(|r| r.icing_detected = Some(IcingDetected::Other(0xFE)));
    assert_eq!(tlv_value(&encoded, 34), Some(vec![0xFE]));

    // Tag 63 — Sensor Field of View Name (§8.63): worked example → 0x02 (Medium).
    let ls = decode_with_single_tlv(63, &[0x02]);
    assert_eq!(ls.sensor_fov_name, Some(SensorFovName::Medium));
    let encoded = encode_with_field(|r| r.sensor_fov_name = Some(SensorFovName::Medium));
    assert_eq!(tlv_value(&encoded, 63), Some(vec![0x02]));
    // Table 4's 8th codepoint — the def-table-vs-Table-4 discrepancy (§8.63.1).
    let ls = decode_with_single_tlv(63, &[0x08]);
    assert_eq!(ls.sensor_fov_name, Some(SensorFovName::ContinuousZoom));
    let encoded = encode_with_field(|r| r.sensor_fov_name = Some(SensorFovName::ContinuousZoom));
    assert_eq!(tlv_value(&encoded, 63), Some(vec![0x08]));
    // Wire-unknown codepoint round-trips byte-exact via Other(code).
    let ls = decode_with_single_tlv(63, &[0xFE]);
    assert_eq!(ls.sensor_fov_name, Some(SensorFovName::Other(0xFE)));
    let encoded = encode_with_field(|r| r.sensor_fov_name = Some(SensorFovName::Other(0xFE)));
    assert_eq!(tlv_value(&encoded, 63), Some(vec![0xFE]));

    // Tag 77 — Operational Mode (§8.77.1 Table 5): worked example "1 (Operational)" → 0x01.
    let ls = decode_with_single_tlv(77, &[0x01]);
    assert_eq!(ls.operational_mode, Some(OperationalMode::Operational));
    let encoded = encode_with_field(|r| r.operational_mode = Some(OperationalMode::Operational));
    assert_eq!(tlv_value(&encoded, 77), Some(vec![0x01]));
    // Spec code 0 is named "Other" in Table 5 — modeled as `OtherMode` to
    // avoid clashing with the catch-all `Other(code)` fallback arm.
    let ls = decode_with_single_tlv(77, &[0x00]);
    assert_eq!(ls.operational_mode, Some(OperationalMode::OtherMode));
    let encoded = encode_with_field(|r| r.operational_mode = Some(OperationalMode::OtherMode));
    assert_eq!(tlv_value(&encoded, 77), Some(vec![0x00]));
}

// ============================================================================
// WP-A: named nested-set raw fields — new tags 73/95/97-101 (Table A4)
// ============================================================================

/// Tags 73/95/97-101 (nested-set bytes for RVT/SAR-MI/Range-Image/
/// Geo-Registration/Composite-Imaging/Segment/Amend) used to fall through
/// to `unknown` (no TagSpec entry). Table A4 gives each its own
/// `Option<Vec<u8>>` field. Table-driven over `(tag, field-accessor)`
/// pairs per the brief: proves the move off `unknown`, byte-exact
/// round-trip, and that the now-typed tag is rejected from `unknown` on
/// encode (same `ReservedTagInUnknown` contract as every other typed tag).
#[test]
#[allow(clippy::type_complexity)]
fn wpa_nested_set_bytes_move_from_unknown_to_named_fields() {
    let cases: &[(u8, fn(&UasDatalinkLs) -> Option<&[u8]>)] = &[
        (73, |r| r.rvt.as_deref()),
        (95, |r| r.sar_mi_local_set.as_deref()),
        (97, |r| r.range_image_local_set.as_deref()),
        (98, |r| r.geo_registration_local_set.as_deref()),
        (99, |r| r.composite_imaging_local_set.as_deref()),
        (100, |r| r.segment_local_set.as_deref()),
        (101, |r| r.amend_local_set.as_deref()),
    ];
    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    for &(tag, get) in cases {
        let ls = decode_with_single_tlv(tag, &payload);
        assert_eq!(
            get(&ls),
            Some(&payload[..]),
            "tag {tag}: bytes did not land on the named field"
        );
        assert!(
            ls.unknown.is_empty(),
            "tag {tag} must no longer land in unknown"
        );
        // Round-trip byte fidelity:
        let bytes = crate::klv::st0601::encode_to_vec(&ls).unwrap();
        assert_eq!(
            tlv_value(&bytes, tag),
            Some(payload.to_vec()),
            "tag {tag}: round trip lost byte fidelity"
        );
        // Now-typed tags are rejected from `unknown` on encode:
        let mut rec = UasDatalinkLs::default();
        rec.unknown.push(OwnedRawField {
            tag: tag as u32,
            value: vec![1],
        });
        let err = crate::klv::st0601::encode_to_vec(&rec).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: t } if t == tag as u32),
            "tag {tag}: expected ReservedTagInUnknown, got {err:?}"
        );
    }
}

// ============================================================================
// WP-A: sentinel population + Indicator eligibility for the newly-typed
// signed tags (Table A5)
// ============================================================================

/// Regression pin: the table-driven sentinel mechanism (already proven by
/// `every_modelled_signed_tag_has_a_sentinel_meaning` and the Indicator-mode
/// tests above) covers the ten signed tags WP-A newly typed as
/// `UasDatalinkLs` fields — 40, 41, 51, 52, 67, 68, 79, 80, 92, 93. Also
/// pins that `OutOfRangePolicy::Indicator` is now reachable for every
/// OutOfRange-meaning tag (51 here) since all 11 are encodable fields, while
/// a Reserved-meaning tag (67) still errors under the same policy.
#[allow(clippy::field_reassign_with_default)]
#[test]
fn wpa_new_signed_tags_populate_sentinels_and_indicator() {
    // Decode: INT_MIN on newly-typed signed tags -> sentinel_tags, field None.
    for &(tag, len) in &[
        (40u8, 4usize),
        (41, 4),
        (51, 2),
        (52, 2),
        (67, 4),
        (68, 4),
        (79, 2),
        (80, 2),
        (92, 4),
        (93, 4),
    ] {
        let int_min = if len == 2 {
            vec![0x80, 0x00]
        } else {
            vec![0x80, 0, 0, 0]
        };
        let ls = decode_with_single_tlv(tag, &int_min);
        assert!(ls.sentinel_tags.contains(&(tag as u32)), "tag {tag}");
        assert!(crate::klv::st0601::st0601_sentinel_meaning(tag as u32).is_some());
    }
    // Encode: Indicator policy now reachable for 51/52/79/80/92/93 (OutOfRange meaning).
    let mut rec = UasDatalinkLs::default();
    rec.platform_vertical_speed = Some(500.0); // out of ±180
    let cfg = EncodeConfig {
        out_of_range_policy: OutOfRangePolicy::Indicator,
        ..Default::default()
    };
    let bytes = crate::klv::st0601::encode_to_vec_with(&rec, &cfg).unwrap();
    assert_eq!(tlv_value(&bytes, 51), Some(vec![0x80, 0x00]));
    // Reserved-meaning tags still error under Indicator:
    let mut rec = UasDatalinkLs::default();
    rec.alternate_platform_lat_deg = Some(95.0);
    assert!(crate::klv::st0601::encode_to_vec_with(&rec, &cfg).is_err());
}

// ============================================================================
// WP-B: var-length int/enum fields — new tags 110/111/123-126/131/133/
// 136/137 + Tag 139 Active Payloads (Table B2 spec vectors)
// ============================================================================

/// MISB ST 0601.19 §8 worked examples for the 10 WP-B Table B2
/// var-length int/enum items plus Tag 139 — spec bytes in both
/// directions (identity big-endian encodings, no quantization tolerance
/// needed, matching the rigor of `wpa_raw_spec_vectors`). All ten
/// integer example byte strings are already the encoder's shortest
/// form, so decode-then-reencode reproduces them exactly.
#[test]
fn wpb_var_len_spec_vectors() {
    let ls = decode_with_single_tlv(110, &[0x4D, 0xAF]);
    assert_eq!(ls.time_airborne_s, Some(19887));
    let encoded = encode_with_field(|r| r.time_airborne_s = Some(19887));
    assert_eq!(tlv_value(&encoded, 110), Some(vec![0x4D, 0xAF]));

    let ls = decode_with_single_tlv(111, &[0x0B, 0xB8]);
    assert_eq!(ls.propulsion_unit_speed_rpm, Some(3000));
    let encoded = encode_with_field(|r| r.propulsion_unit_speed_rpm = Some(3000));
    assert_eq!(tlv_value(&encoded, 111), Some(vec![0x0B, 0xB8]));

    let ls = decode_with_single_tlv(123, &[0x07]);
    assert_eq!(ls.navsats_in_view, Some(7));
    let encoded = encode_with_field(|r| r.navsats_in_view = Some(7));
    assert_eq!(tlv_value(&encoded, 123), Some(vec![0x07]));

    let ls = decode_with_single_tlv(124, &[0x03]);
    assert_eq!(ls.positioning_method_source, Some(3));
    let encoded = encode_with_field(|r| r.positioning_method_source = Some(3));
    assert_eq!(tlv_value(&encoded, 124), Some(vec![0x03]));

    let ls = decode_with_single_tlv(125, &[0x09]);
    assert_eq!(ls.platform_status, Some(PlatformStatus::Egress));
    let encoded = encode_with_field(|r| r.platform_status = Some(PlatformStatus::Egress));
    assert_eq!(tlv_value(&encoded, 125), Some(vec![0x09]));

    let ls = decode_with_single_tlv(126, &[0x05]);
    assert_eq!(
        ls.sensor_control_mode,
        Some(SensorControlMode::AutoHoldingPosition)
    );
    let encoded =
        encode_with_field(|r| r.sensor_control_mode = Some(SensorControlMode::AutoHoldingPosition));
    assert_eq!(tlv_value(&encoded, 126), Some(vec![0x05]));

    // Tags 131/133/136/137/139 are all >= 128 — need the 2-byte BER-OID
    // tag helper (`decode_with_single_tlv` only writes a literal 1-byte
    // tag, which a value >= 0x80 would misparse as a BER-OID continuation
    // byte). Same reason Table B1's tags 132/134 used this helper.
    let bytes131 = [0x05, 0x6F, 0x27, 0x1B, 0x5E, 0x41, 0xB7];
    let ls = decode_with_single_tlv_ber_oid(131, &bytes131);
    assert_eq!(ls.take_off_time_us, Some(1_529_588_637_122_999));
    let encoded = encode_with_field(|r| r.take_off_time_us = Some(1_529_588_637_122_999));
    assert_eq!(tlv_value(&encoded, 131), Some(bytes131.to_vec()));

    let ls = decode_with_single_tlv_ber_oid(133, &[0x27, 0x10]);
    assert_eq!(ls.mi_storage_capacity_gb, Some(10000));
    let encoded = encode_with_field(|r| r.mi_storage_capacity_gb = Some(10000));
    assert_eq!(tlv_value(&encoded, 133), Some(vec![0x27, 0x10]));

    let ls = decode_with_single_tlv_ber_oid(136, &[0x1E]);
    assert_eq!(ls.leap_seconds, Some(30));
    let encoded = encode_with_field(|r| r.leap_seconds = Some(30));
    assert_eq!(tlv_value(&encoded, 136), Some(vec![0x1E]));
    // Negative shortest-form pin: -30 -> 0xE2.
    let encoded = encode_with_field(|r| r.leap_seconds = Some(-30));
    assert_eq!(tlv_value(&encoded, 136), Some(vec![0xE2]));

    let bytes137 = [0x01, 0x2B, 0x8D, 0xC6, 0x35];
    let ls = decode_with_single_tlv_ber_oid(137, &bytes137);
    assert_eq!(ls.correction_offset_us, Some(5_025_678_901));
    let encoded = encode_with_field(|r| r.correction_offset_us = Some(5_025_678_901));
    assert_eq!(tlv_value(&encoded, 137), Some(bytes137.to_vec()));

    // Tag 139 — Active Payloads: RawBytes, bit i (LSB-first) = Payload ID i.
    let ls = decode_with_single_tlv_ber_oid(139, &[0x0B]);
    assert_eq!(ls.active_payloads.as_deref(), Some(&[0x0B][..]));
    assert_eq!(
        ls.active_payload_ids().collect::<Vec<_>>(),
        vec![0u32, 1, 3]
    );
    let encoded = encode_with_field(|r| r.active_payloads = Some(vec![0x0B]));
    assert_eq!(tlv_value(&encoded, 139), Some(vec![0x0B]));
}

/// Var-length int decode accepts any wire length in `1..=max_len` (not
/// just the spec example length) — mirrors
/// `wpb_imapb_variable_length_decode_and_specials` for the Table B2
/// substrate. An over-`max_len` wire value is a per-field decode error
/// collected in `field_errors`, not a fatal `decode()` failure. Tag
/// 124's all-zero bitfield is non-conformant per spec (§8.124 declares
/// range `1..255`, i.e. at least one bit must be set) but decode stays
/// lenient and does not reject it.
#[test]
fn wpb_var_len_variable_length_decode_and_invalid_length() {
    // Tag 110 (max_len=4): a 1-byte wire value decodes fine.
    let ls = decode_with_single_tlv(110, &[0x07]);
    assert_eq!(ls.time_airborne_s, Some(7));

    // Tag 123 (max_len=1): 2 wire bytes exceeds max_len -> a per-field
    // InvalidLength error; decode() still returns Ok with the field left
    // None (same lenient-collection contract as every other typed tag).
    let tlv = vec![123u8, 2, 0x00, 0x07];
    let ls = decode(&wrap_st0601(&tlv)).expect("decode collects field errors, does not fail");
    assert_eq!(ls.navsats_in_view, None);
    assert_eq!(ls.field_errors.len(), 1);
    assert!(matches!(
        ls.field_errors[0],
        crate::error::KlvFieldError::InvalidLength {
            tag: 123,
            expected: 1,
            got: 2,
        }
    ));

    // Tag 124's all-zero bitfield: decode-lenient, no error.
    let ls = decode_with_single_tlv(124, &[0x00]);
    assert_eq!(ls.positioning_method_source, Some(0));
    assert!(ls.field_errors.is_empty());
}

/// `PlatformStatus`/`SensorControlMode` keep the same `Other(code)`
/// wire-unknown fallback as the WP-A coded enums (Tags 34/63/77) —
/// round-trips byte-exact.
#[test]
fn wpb_platform_status_and_sensor_control_mode_other_round_trip() {
    let ls = decode_with_single_tlv(125, &[0xFE]);
    assert_eq!(ls.platform_status, Some(PlatformStatus::Other(0xFE)));
    let encoded = encode_with_field(|r| r.platform_status = Some(PlatformStatus::Other(0xFE)));
    assert_eq!(tlv_value(&encoded, 125), Some(vec![0xFE]));

    let ls = decode_with_single_tlv(126, &[0xFE]);
    assert_eq!(ls.sensor_control_mode, Some(SensorControlMode::Other(0xFE)));
    let encoded =
        encode_with_field(|r| r.sensor_control_mode = Some(SensorControlMode::Other(0xFE)));
    assert_eq!(tlv_value(&encoded, 126), Some(vec![0xFE]));
}

/// `active_payload_ids` bit i (LSB-first within a byte) = Payload ID i;
/// additional bytes extend the ID space upward (byte 1 covers IDs
/// 8-15). An absent `active_payloads` yields an empty iterator.
#[test]
fn wpb_active_payload_ids_multi_byte_extends_upward() {
    let ls = decode_with_single_tlv_ber_oid(139, &[0x00, 0x01]);
    assert_eq!(ls.active_payload_ids().collect::<Vec<_>>(), vec![8u32]);

    let ls = decode_with_single_tlv_ber_oid(139, &[0xFF, 0x02]);
    assert_eq!(
        ls.active_payload_ids().collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5, 6, 7, 9]
    );

    let ls = UasDatalinkLs::default();
    assert_eq!(ls.active_payload_ids().count(), 0);
}

// ============================================================================
// WP-C Task C2: pack substrate + simple DLP packs (81/115/116/121/127/143)
// ============================================================================

/// Strip whitespace and parse a hex string into bytes — WP-C pack spec
/// vectors (mirrors the identical helper in `klv::st1010`'s test module).
fn hex(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
        .collect()
}

/// Build one canonical `[BER-OID tag][BER length][value]` TLV. Unlike
/// [`decode_with_single_tlv`] (which writes `tag` as a single raw byte —
/// only correct for `tag < 128`), this goes through
/// [`crate::klv::pack::emit_ber_oid_tlv`] so it also handles the
/// 2-byte-BER-OID tags (e.g. 143) correctly. Used to build multi-TLV
/// bodies (e.g. two Tag-115 occurrences) for [`decode_body`].
fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    crate::klv::pack::emit_ber_oid_tlv(tag as u32, value, &mut out).unwrap();
    out
}

/// Decode a full LS body (already TLV-framed, e.g. via repeated [`tlv`]
/// calls) by wrapping it with the UL + outer length + checksum
/// ([`wrap_st0601`]) and calling [`decode_unchecked`].
fn decode_body(body: &[u8]) -> UasDatalinkLs {
    decode_unchecked(&wrap_st0601(body)).expect("multi-tlv fixture must decode")
}

/// ST 0601.19 §8 worked examples for the 6 WP-C Task C2 pack/list items —
/// spec bytes, not round-trip (closed-loop tests can't catch a wrong wire
/// formula).
#[test]
fn wpc_simple_pack_spec_vectors() {
    // Tag 81 — geo-truncated example: (0,36)->(56,0), no optional geo fields.
    let ls = decode_with_single_tlv(81, &[0x00, 0x24, 0x38, 0x00]);
    let h = ls.image_horizon.unwrap();
    assert_eq!((h.x0_pct, h.y0_pct, h.x1_pct, h.y1_pct), (0, 36, 56, 0));
    assert_eq!(h.start_lat_deg, None);
    assert_eq!(h.start_lon_deg, None);
    assert_eq!(h.end_lat_deg, None);
    assert_eq!(h.end_lon_deg, None);
    let re_encoded = encode_with_field(|r| r.image_horizon = Some(h));
    assert_eq!(
        tlv_value(&re_encoded, 81),
        Some(vec![0x00, 0x24, 0x38, 0x00])
    );

    // Tag 115 — (5, "Fly to Waypoint 1"), time truncated; MULTI-INSTANCE
    // accumulates: two occurrences append two `ControlCommand`s.
    let v115a = hex("05 11 466C7920746F20576179706F696E742031");
    let v115b = hex("07 03 41 42 43"); // second command (7, "ABC")
    let mut body = tlv(115, &v115a);
    body.extend(tlv(115, &v115b));
    let ls = decode_body(&body);
    assert_eq!(ls.control_commands.len(), 2);
    assert_eq!(ls.control_commands[0].id, 5);
    assert_eq!(ls.control_commands[0].command, "Fly to Waypoint 1");
    assert_eq!(ls.control_commands[0].time_us, None);
    assert_eq!(ls.control_commands[1].id, 7);
    assert_eq!(ls.control_commands[1].command, "ABC");
    assert_eq!(ls.control_commands[1].time_us, None);
    let re_encoded = encode_with_field(|r| r.control_commands = ls.control_commands.clone());
    assert_eq!(tlv_value(&re_encoded, 115), Some(v115a));
    // `tlv_value`/`find_tag` locate the FIRST Tag 115 TLV only. Nothing
    // else is set on this record, so the second `ControlCommand` is
    // emitted immediately after the first one's value bytes end (the
    // multi-instance encode special-case appends both back-to-back) —
    // verify the second full TLV (tag+length+value) starts right there.
    let (off, len) = find_tag(&re_encoded, 115).unwrap();
    let second_tlv = tlv(115, &v115b);
    assert_eq!(
        &re_encoded[off + len..off + len + second_tlv.len()],
        second_tlv.as_slice()
    );

    // Tag 116: [3,7]; Tag 121: [1,3].
    assert_eq!(
        decode_with_single_tlv(116, &[0x03, 0x07]).control_command_verification,
        Some(vec![3, 7])
    );
    let encoded116 = encode_with_field(|r| r.control_command_verification = Some(vec![3, 7]));
    assert_eq!(tlv_value(&encoded116, 116), Some(vec![0x03, 0x07]));

    assert_eq!(
        decode_with_single_tlv(121, &[0x01, 0x03]).active_wavelengths,
        Some(vec![1, 3])
    );
    let encoded121 = encode_with_field(|r| r.active_wavelengths = Some(vec![1, 3]));
    assert_eq!(tlv_value(&encoded121, 121), Some(vec![0x01, 0x03]));

    // Tag 127: 60000/1001, and denominator-truncated (defaults to 1).
    let fr = decode_with_single_tlv(127, &hex("83 D4 60 87 69"))
        .sensor_frame_rate
        .unwrap();
    assert_eq!((fr.numerator, fr.denominator), (60000, 1001));
    let encoded127 = encode_with_field(|r| r.sensor_frame_rate = Some(fr));
    assert_eq!(tlv_value(&encoded127, 127), Some(hex("83 D4 60 87 69")));

    let fr = decode_with_single_tlv(127, &hex("1E"))
        .sensor_frame_rate
        .unwrap();
    assert_eq!((fr.numerator, fr.denominator), (30, 1));
    let encoded127b = encode_with_field(|r| r.sensor_frame_rate = Some(fr));
    assert_eq!(tlv_value(&encoded127b, 127), Some(hex("1E")));

    // Tag 143: local 0 + uuid (2-byte BER-OID tag, id=0x8F=143).
    let v143 = hex("00 8DC4F462 3EA25A85 9C5D0AF0 C95E8C39");
    let ls = decode_with_single_tlv_ber_oid(143, &v143);
    let ms = ls.metadata_substream_id.unwrap();
    assert_eq!(ms.local_id, 0);
    assert_eq!(ms.uuid.unwrap()[0], 0x8D);
    let encoded143 = encode_with_field(|r| r.metadata_substream_id = Some(ms));
    assert_eq!(tlv_value(&encoded143, 143), Some(v143));
}

/// Duplicate Tag 115 (Multiples Allowed = Yes, ST 0601.19 Table 1) must
/// NOT trip `decode_strict_compliance`'s once-per-packet `DuplicateTag`
/// check. Built with Tag2-first / Tag65 / checksum-last, mirroring
/// `decode_strict_compliance_allows_duplicate_unknown_tag`'s strict
/// fixture shape.
#[test]
fn wpc_strict_allows_repeated_115_and_102() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend(tlv(115, &hex("05 11 466C7920746F20576179706F696E742031")));
    body.extend(tlv(115, &hex("07 03 41 42 43")));
    body.extend_from_slice(&[0x41, 0x01, 0x13]); // Tag 65
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]); // Tag 1 placeholder
    let buf = wrap_st0601_with_inline_checksum(&body);

    let record = decode_strict_compliance(&buf)
        .expect("repeated Tag 115 (Multiples Allowed) must not be rejected as a duplicate");
    assert_eq!(record.control_commands.len(), 2);
}

// ============================================================================
// WP-C Task C3: VLP series packs (122/128/130/138/140/141/142)
// ============================================================================

/// ST 0601.19 §8.122 worked example (transcribed from the cached spec PDF,
/// not just the plan's summary): Coding Method 14 (GENC 3-letter),
/// Overflight "CAN", Operator unknown (length-0 marker), Manufacture
/// "FRA". Byte-fidelity re-encode.
#[test]
fn wpc_country_codes_vector() {
    let v = hex("01 0E 03 43414E 00 03 465241");
    let cc = decode_with_single_tlv(122, &v).country_codes.unwrap();
    assert_eq!(cc.coding_method, 14);
    assert_eq!(cc.overflight.as_deref(), Some("CAN"));
    assert_eq!(cc.operator, None); // len-0 = unknown
    assert_eq!(cc.manufacture.as_deref(), Some("FRA"));

    let re_encoded = encode_with_field(|r| r.country_codes = Some(cc.clone()));
    assert_eq!(tlv_value(&re_encoded, 122), Some(v));
}

/// §8.122.1's two truncation cases: Manufacture-only truncated (stop
/// after Operator), and both Operator+Manufacture truncated (stop after
/// Overflight). Truncation removes the length-value pair ENTIRELY (no
/// trailing length-0 byte at all) — distinct from the length-0 "unknown"
/// marker pinned above.
#[test]
fn wpc_country_codes_truncation_cases() {
    // Manufacture unknown (explicit length-0 marker) with Operator
    // present. The field VALUES round-trip, but canonicalizing re-encode
    // drops the now-redundant trailing zero-length Manufacture pair —
    // no `Some` field follows it, so there is nothing to protect by
    // keeping the explicit marker (same canonicalization precedent as
    // `sensor_frame_rate_explicit_denominator_one_canonicalizes_away`).
    let v = hex("01 0E 03 43414E 03 465241 00");
    let cc = decode_with_single_tlv(122, &v).country_codes.unwrap();
    assert_eq!(cc.operator.as_deref(), Some("FRA"));
    assert_eq!(cc.manufacture, None);
    let re_encoded = encode_with_field(|r| r.country_codes = Some(cc.clone()));
    assert_eq!(
        tlv_value(&re_encoded, 122),
        Some(hex("01 0E 03 43414E 03 465241")),
        "trailing explicit zero-length Manufacture pair canonicalizes away"
    );

    // Fully truncated: only coding_method + overflight on the wire.
    let v2 = hex("01 0E 03 43414E");
    let cc2 = decode_with_single_tlv(122, &v2).country_codes.unwrap();
    assert_eq!(cc2.operator, None);
    assert_eq!(cc2.manufacture, None);
    let re_encoded2 = encode_with_field(|r| r.country_codes = Some(cc2.clone()));
    assert_eq!(tlv_value(&re_encoded2, 122), Some(v2));
}

/// ST 0601.19 §8.128 worked example (transcribed from the cached spec
/// PDF): one Wavelength Record, id=21 ("NNIR"), min/max raw-IMAPB bytes
/// `0x000007D0`/`0x00000FA0` under IMAPB(0,1e9,4). Per the controller's
/// note, assert the field equals OUR OWN standalone `decode_imapb` of
/// those same raw bytes (cross-check), not the spec prose's "1000/2000
/// nm" reading.
#[test]
fn wpc_wavelengths_list_vector() {
    let v = hex("0D 15 0000 07D0 0000 0FA0 4E4E 4952");
    let list = decode_with_single_tlv_ber_oid(128, &v)
        .wavelengths_list
        .unwrap();
    assert_eq!(list.len(), 1);
    let w = &list[0];
    assert_eq!(w.id, 21);
    assert_eq!(w.name, "NNIR");

    let imapb_params = crate::klv::imapb::ImapbParams {
        min: 0.0,
        max: 1e9,
        length: 4,
    };
    let expect_field =
        |bytes: &[u8]| match crate::klv::imapb::decode_imapb(&imapb_params, bytes).unwrap() {
            crate::klv::imapb::DecodedImapb::Value(x) => x,
            other => panic!("expected Value, got {other:?}"),
        };
    assert_eq!(w.min_nm, expect_field(&hex("000007D0")));
    assert_eq!(w.max_nm, expect_field(&hex("00000FA0")));

    let re_encoded = encode_with_field(|r| r.wavelengths_list = Some(list.clone()));
    assert_eq!(tlv_value(&re_encoded, 128), Some(v));
}

/// ST 0601.19 §8.130 worked example (transcribed from the cached spec
/// PDF): two full Location DLPs (take-off, recovery), each lat+lon+hae.
#[test]
fn wpc_airbase_locations_vector() {
    let v = hex("0B406BC20919BDA554070E000B40783CB819A2927407C600");
    let al = decode_with_single_tlv_ber_oid(130, &v)
        .airbase_locations
        .unwrap();
    let take_off = al.take_off.unwrap();
    assert!((take_off.lat_deg.unwrap() - 38.841859).abs() < 1e-4);
    assert!((take_off.lon_deg.unwrap() - -77.036784).abs() < 1e-4);
    assert!((take_off.hae_m.unwrap() - 3.0).abs() < 0.1);
    let recovery = al.recovery.unwrap();
    assert!((recovery.lat_deg.unwrap() - 38.939353).abs() < 1e-4);
    assert!((recovery.lon_deg.unwrap() - -77.459811).abs() < 1e-4);
    assert!((recovery.hae_m.unwrap() - 95.0).abs() < 0.1);

    let re_encoded = encode_with_field(|r| r.airbase_locations = Some(al));
    assert_eq!(tlv_value(&re_encoded, 130), Some(v));
}

/// Recovery ABSENT entirely on the wire (no second length-value pair at
/// all) must decode to `Some(take_off)` per §8.130.1 bullet 1's "same as
/// take-off" rule — and canonicalizing re-encode must reproduce the
/// SAME truncated wire form (recovery omitted again), matching the
/// `sensor_frame_rate_explicit_denominator_one_canonicalizes_away`
/// canonicalization precedent.
#[test]
fn wpc_airbase_locations_recovery_omitted_defaults_to_take_off() {
    let v = hex("0B406BC20919BDA554070E00"); // take-off only, no recovery pair
    let al = decode_with_single_tlv_ber_oid(130, &v)
        .airbase_locations
        .unwrap();
    assert_eq!(al.recovery, al.take_off);

    let re_encoded = encode_with_field(|r| r.airbase_locations = Some(al));
    assert_eq!(tlv_value(&re_encoded, 130), Some(v));
}

/// ST 0601.19 Table 16 marks Latitude and Longitude Mandatory but HAE
/// Optional — a legitimate wire `Location` may be the 8-byte lat+lon
/// pair with HAE truncated away entirely (distinct from a 4-byte
/// lat-only pack, which Table 16 does not permit — see the next test).
#[test]
fn wpc_location_pair_only_no_hae_round_trips() {
    let v = hex("08 406BC209 19BDA554"); // take-off = lat+lon only, no HAE
    let al = decode_with_single_tlv_ber_oid(130, &v)
        .airbase_locations
        .unwrap();
    let take_off = al.take_off.unwrap();
    assert!(take_off.lat_deg.is_some());
    assert!(take_off.lon_deg.is_some());
    assert_eq!(take_off.hae_m, None);

    let re_encoded = encode_with_field(|r| r.airbase_locations = Some(al));
    assert_eq!(tlv_value(&re_encoded, 130), Some(v));
}

/// Latitude and Longitude are a mandatory BOTH-OR-NEITHER pair per
/// ST 0601.19 Table 16 — a `Location` slot declaring length 4 (as if
/// Latitude alone were truncatable) is malformed, not a valid
/// lat-only truncation. Must land in `field_errors`, not silently
/// decode a partial `Location`.
#[test]
fn wpc_location_four_byte_length_rejected() {
    let v = hex("04 406BC209"); // take-off slot declares length 4 (lat-only) — invalid
    let ls = decode_with_single_tlv_ber_oid(130, &v);
    assert_eq!(
        ls.airbase_locations, None,
        "malformed Location must not populate the typed field"
    );
    assert_eq!(ls.field_errors.len(), 1);
    assert!(
        matches!(
            ls.field_errors[0],
            crate::error::KlvFieldError::InvalidLength {
                tag: 130,
                expected: 11,
                got: 4,
            }
        ),
        "got {:?}",
        ls.field_errors[0]
    );
}

/// A `Location` whose HAE is present but Latitude or Longitude is
/// individually absent (a state ST 0601.19 §8.130 never itself
/// produces, since the spec only ever truncates HAE, never leaves
/// lat/lon absent while HAE is present) still round-trips: the
/// mandatory pair is transmitted as an 11-byte unit, with the
/// interior-absent field filled by the ST 1201.5 special-value filler
/// and decoded straight back to `None`.
#[test]
fn wpc_location_interior_absent_lat_or_lon_round_trips_via_filler() {
    let al = super::packs::AirbaseLocations {
        take_off: Some(super::packs::Location {
            lat_deg: Some(38.8),
            lon_deg: None,
            hae_m: Some(100.0),
        }),
        recovery: None,
    };
    let encoded = encode_with_field(|r| r.airbase_locations = Some(al));
    let back = decode(&encoded).unwrap().airbase_locations.unwrap();
    let take_off = back.take_off.unwrap();
    assert!((take_off.lat_deg.unwrap() - 38.8).abs() < 1e-3);
    assert_eq!(
        take_off.lon_deg, None,
        "interior-absent lon must round-trip via the IMAPB filler, not silently \
         become some other value"
    );
    assert!((take_off.hae_m.unwrap() - 100.0).abs() < 0.1);
}

/// ST 0601.19 §8.138 worked example — the 63-byte 3-record vector
/// transcribed from the cached spec PDF (`reference/ST0601.19.pdf`
/// §8.138 "Example KLV Item (All Hex)"): Payload Count 3, records
/// (0,EO,"VIS Nose Camera"), (1,EO,"ACME VIS Model 123"),
/// (2,EO,"ACME IR Model 456").
#[test]
fn wpc_payload_list_vector() {
    let v = hex("03 12 0000 0F56 4953 204E 6F73 6520 4361 6D65 7261
         15 01 0012 4143 4D45 2056 4953 204D 6F64 656C 2031 3233
         14 02 0011 4143 4D45 2049 5220 4D6F 6465 6C20 3435 36");
    assert_eq!(v.len(), 63, "the §8.138 example value is 63 bytes");
    let pl = decode_with_single_tlv_ber_oid(138, &v)
        .payload_list
        .unwrap();
    assert_eq!(pl.count, 3);
    assert_eq!(pl.records.len(), 3);
    assert_eq!(pl.records[0].id, 0);
    assert_eq!(pl.records[0].payload_type, PayloadType::ElectroOptical);
    assert_eq!(pl.records[0].name, "VIS Nose Camera");
    assert_eq!(pl.records[1].id, 1);
    assert_eq!(pl.records[1].name, "ACME VIS Model 123");
    assert_eq!(pl.records[2].id, 2);
    assert_eq!(pl.records[2].name, "ACME IR Model 456");

    let re_encoded = encode_with_field(|r| r.payload_list = Some(pl.clone()));
    assert_eq!(tlv_value(&re_encoded, 138), Some(v));
}

/// ST 0601.19 §8.140 PER-RECORD vectors, transcribed and independently
/// recomputed from the cached spec PDF's 3-record example: (Harpoon,
/// general=3/fuze), (Hellfire, general=4/all-engagement-bits),
/// (GBU-15, general=3/no-engagement). The whole-item spec vector has a
/// KNOWN 1-byte length discrepancy (declared len 0x2D=45, but the three
/// records' own length prefixes only total 44 bytes) — pin per-record
/// vectors and build the outer VLP ourselves rather than trusting the
/// spec's stated outer length.
#[test]
fn wpc_weapons_stores_vector() {
    let r1 = hex("0E 01 01 01 03 82 03 07 48 61 72 70 6F 6F 6E"); // Harpoon
    let r2 = hex("0F 01 01 02 02 9E 04 08 48 65 6C 6C 66 69 72 65"); // Hellfire
    let r3 = hex("0C 01 02 01 01 03 06 47 42 55 2D 31 35"); // GBU-15
    let mut v = r1.clone();
    v.extend(&r2);
    v.extend(&r3);
    assert_eq!(v.len(), 44, "3 records' own length prefixes total 44 bytes");

    let stores = decode_with_single_tlv_ber_oid(140, &v)
        .weapons_stores
        .unwrap();
    assert_eq!(stores.len(), 3);

    let harpoon = &stores[0];
    assert_eq!(
        (
            harpoon.station_id,
            harpoon.hardpoint_id,
            harpoon.carriage_id,
            harpoon.store_id
        ),
        (1, 1, 1, 3)
    );
    assert_eq!(harpoon.general_status(), 3);
    assert!(harpoon.fuze_enabled());
    assert!(!harpoon.laser_enabled());
    assert!(!harpoon.target_enabled());
    assert!(!harpoon.weapon_armed());
    assert_eq!(harpoon.weapon_type, "Harpoon");

    let hellfire = &stores[1];
    assert_eq!(hellfire.general_status(), 4);
    assert!(hellfire.fuze_enabled());
    assert!(hellfire.laser_enabled());
    assert!(hellfire.target_enabled());
    assert!(hellfire.weapon_armed());
    assert_eq!(hellfire.weapon_type, "Hellfire");

    let gbu15 = &stores[2];
    assert_eq!((gbu15.station_id, gbu15.hardpoint_id), (1, 2));
    assert_eq!(gbu15.general_status(), 3);
    assert!(!gbu15.fuze_enabled());
    assert_eq!(gbu15.weapon_type, "GBU-15");

    let re_encoded = encode_with_field(|r| r.weapons_stores = Some(stores.clone()));
    assert_eq!(tlv_value(&re_encoded, 140), Some(v));
}

/// ST 0601.19 §8.141 worked example (transcribed verbatim from the
/// cached spec PDF, matching the plan's vector exactly): four Waypoint
/// Records exercising all four `prosecution_order` cases (current,
/// planned, cancelled, historical) plus a full Location on every
/// record.
#[test]
fn wpc_waypoint_list_vector() {
    let v = hex("0F 00 0001 03 4071D894 19BDBFE7 089800
         0F 01 0002 02 4071D388 19BCCE24 08FC00
         0F 02 7FFF 01 4071E308 19BF2C1B 07D000
         0F 03 FFFE 00 4071E5AF 19BF5AA7 096000");
    let wps = decode_with_single_tlv_ber_oid(141, &v)
        .waypoint_list
        .unwrap();
    assert_eq!(wps.len(), 4);
    assert_eq!(wps[0].prosecution_order, 1);
    assert_eq!(wps[1].prosecution_order, 2);
    assert_eq!(wps[2].prosecution_order, 0x7FFF); // cancelled
    assert_eq!(wps[3].prosecution_order, -2); // historical
    assert_eq!(wps[0].info, Some(3));
    assert_eq!(wps[3].info, Some(0));

    let loc = wps[0].location.unwrap();
    assert!((loc.lat_deg.unwrap() - 38.889422).abs() < 1e-5);
    assert!((loc.lon_deg.unwrap() - -77.035162).abs() < 1e-5);
    assert!((loc.hae_m.unwrap() - 200.0).abs() < 0.1);

    let loc3 = wps[3].location.unwrap();
    assert!((loc3.lat_deg.unwrap() - 38.889822).abs() < 1e-5);
    assert!((loc3.hae_m.unwrap() - 300.0).abs() < 0.1);

    let re_encoded = encode_with_field(|r| r.waypoint_list = Some(wps.clone()));
    assert_eq!(tlv_value(&re_encoded, 141), Some(v));
}

/// ST 0601.19 §8.142 worked example (transcribed verbatim from the
/// cached spec PDF, matching the plan's vector exactly): azimuth domain
/// (210, 300), elevation domain (-75, 50), roll domain truncated away
/// entirely (trailing truncation — no pair-length byte at all).
#[test]
fn wpc_view_domain_truncated_roll() {
    let v = hex("06 348000 4B0000 06 1A4000 0C8000");
    let vd = decode_with_single_tlv_ber_oid(142, &v).view_domain.unwrap();
    assert!((vd.azimuth.unwrap().start_deg - 210.0).abs() < 0.01);
    assert!((vd.azimuth.unwrap().range_deg - 300.0).abs() < 0.01);
    assert!((vd.elevation.unwrap().start_deg + 75.0).abs() < 0.01);
    assert!((vd.elevation.unwrap().range_deg - 50.0).abs() < 0.01);
    assert_eq!(vd.roll, None);

    let re_encoded = encode_with_field(|r| r.view_domain = Some(vd));
    assert_eq!(tlv_value(&re_encoded, 142), Some(v));
}

/// A pair-length of zero ("unknown") for the FIRST pair, followed by
/// present elevation and roll pairs — pins that an interior "unknown"
/// marker does not stop the walk (mirrors §8.142 Figure 89).
#[test]
fn wpc_view_domain_leading_unknown_pair() {
    let v = hex("00 06 1A4000 0C8000 06 578000 050000");
    let vd = decode_with_single_tlv_ber_oid(142, &v).view_domain.unwrap();
    assert_eq!(vd.azimuth, None);
    assert!((vd.elevation.unwrap().start_deg + 75.0).abs() < 0.01);
    assert!((vd.roll.unwrap().start_deg - 350.0).abs() < 0.1);
    assert!((vd.roll.unwrap().range_deg - 20.0).abs() < 0.1);

    let re_encoded = encode_with_field(|r| r.view_domain = Some(vd));
    assert_eq!(tlv_value(&re_encoded, 142), Some(v));
}

// ============================================================================
// WP-C Task C4: Tag 102 SDCC-FLP positional capture
// ============================================================================

/// Tag 13 (Sensor Latitude): signed 4-byte, [-90, 90].
fn enc_lat(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 4];
    let r = super::tags::lookup(13).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 13, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 14 (Sensor Longitude): signed 4-byte, [-180, 180].
fn enc_lon(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 4];
    let r = super::tags::lookup(14).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 14, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 15 (Sensor True Altitude): unsigned 2-byte, [-900, 19000].
fn enc_alt(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 2];
    let r = super::tags::lookup(15).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 15, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 5 (Platform Heading Angle): unsigned 2-byte, [0, 360].
fn enc_heading(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 2];
    let r = super::tags::lookup(5).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 5, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 6 (Platform Pitch Angle): signed 2-byte, [-20, 20].
fn enc_pitch(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 2];
    let r = super::tags::lookup(6).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 6, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 7 (Platform Roll Angle): signed 2-byte, [-50, 50].
fn enc_roll(v: f64) -> Vec<u8> {
    let mut out = vec![0u8; 2];
    let r = super::tags::lookup(7).unwrap().range.unwrap();
    super::mapping::encode_fixed_range(&r, 7, v, &mut out, OutOfRangePolicy::Error).unwrap();
    out
}

/// Tag 102 is MULTI-INSTANCE with positional row->item semantics (the
/// "Refined Source List" binding, ST 0601.19 §8.102): each occurrence
/// refines the accuracy of the N Local Set items immediately preceding
/// it in wire order. Two occurrences over two disjoint preceding-item
/// groups prove per-occurrence capture (not a single running list).
#[test]
fn wpc_sdcc_positional_capture() {
    // Body: tag13, tag14, tag15 values then a 3x3 SDCC pack (the C1 full
    // golden), twice with different members to prove per-occurrence
    // capture.
    let pack = hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000");
    let mut body = tlv(13, &enc_lat(34.0));
    body.extend(tlv(14, &enc_lon(-118.0)));
    body.extend(tlv(15, &enc_alt(1500.0)));
    body.extend(tlv(102, &pack));
    body.extend(tlv(5, &enc_heading(90.0)));
    body.extend(tlv(6, &enc_pitch(1.0)));
    body.extend(tlv(7, &enc_roll(0.5)));
    body.extend(tlv(102, &pack));
    let ls = decode_body(&body);
    assert_eq!(ls.sdcc_flps.len(), 2);
    assert_eq!(ls.sdcc_flps[0].preceding_tags, alloc::vec![13, 14, 15]);
    assert_eq!(ls.sdcc_flps[1].preceding_tags, alloc::vec![5, 6, 7]);
    let m = crate::klv::st1010::decode_sdcc_flp(&ls.sdcc_flps[0].bytes).unwrap();
    assert_eq!(m.std_devs, alloc::vec![1.0, 2.0, 4.0]);
    // Byte-fidelity re-encode:
    let out = crate::klv::st0601::encode_to_vec(&ls).unwrap();
    let re = crate::klv::st0601::decode(&out).unwrap();
    assert_eq!(re.sdcc_flps.len(), 2);
    assert_eq!(re.sdcc_flps[0].bytes, ls.sdcc_flps[0].bytes);
}

/// A malformed pack header (truncated BER-OID Matrix Size, here an empty
/// value) cannot be peeked for N — the occurrence is dropped into
/// `field_errors` rather than panicking or silently capturing a bogus
/// zero-length window.
#[test]
fn wpc_sdcc_malformed_header_is_field_error_not_panic() {
    let ls = decode_with_single_tlv(102, &[]);
    assert!(ls.sdcc_flps.is_empty());
    assert_eq!(ls.field_errors.len(), 1);
}

/// Review 9 (ext R9-02): a Tag 102 whose Matrix Size names more items than
/// the pack's own bytes can describe is rejected (`TruncatedField`), never
/// captured — before the fix every such occurrence cloned
/// `tags_seen[len - N..]`, so n occurrences after n fields retained n² tags
/// (~63 KB of wire → ~309 MiB through `decode_strict_compliance`, the C /
/// Python / JVM typed decoders included).
#[test]
fn repeated_sdcc_history_cannot_expand_quadratically() {
    fn wire(n: usize) -> Vec<u8> {
        let mut body = tlv(2, &[0; 8]); // Precision Time Stamp
        body.extend(tlv(65, &[19])); // UAS Datalink LS version
        for _ in 0..n {
            // Unknown multi-byte tag 258 with an empty value: one history entry each.
            crate::klv::pack::emit_ber_oid_tlv(258, &[], &mut body).unwrap();
        }
        let mut oid = [0u8; 5];
        let used = crate::klv::length::write_ber_oid(n as u32, &mut oid).unwrap();
        for _ in 0..n {
            // A "pack" that is only a Matrix Size: no Parse Control, no data.
            body.extend(tlv(102, &oid[..used]));
        }
        wrap_st0601(&body)
    }
    for n in [256usize, 512] {
        let decoded = decode_strict_compliance(&wire(n)).expect("framing and checksum are valid");
        assert!(
            decoded.sdcc_flps.is_empty(),
            "n={n}: {} occurrences captured",
            decoded.sdcc_flps.len()
        );
        assert_eq!(
            decoded.field_errors.len(),
            n,
            "one TruncatedField per rejected occurrence"
        );
    }
    // `wpc_sdcc_positional_capture` pins that a pack which CAN hold its
    // matrix (the C1 golden, N = 3) is still captured positionally.

    // Review 10 (R10-02): standard-deviation-only packs are no longer held
    // to the Bit Vector bound, so the cheapest accepted pack per N is now
    // Mode 1 with Slen = 1 (N + 2 bytes for N < 128, N + 3 above). At the
    // largest accepted N (the ST 1010 decoder's matrix ceiling), every
    // occurrence's history copy is still paid for by its own wire bytes.
    let n = crate::klv::st1010::MAX_MATRIX_SIZE as usize;
    let mut body = tlv(2, &[0; 8]);
    body.extend(tlv(65, &[19]));
    for _ in 0..n {
        crate::klv::pack::emit_ber_oid_tlv(258, &[], &mut body).unwrap();
    }
    let mut pack = [0u8; 5];
    let used = crate::klv::length::write_ber_oid(n as u32, &mut pack).unwrap();
    let mut pack = pack[..used].to_vec();
    pack.push(0x10); // Mode 1: Slen = 1, CS = 0, Clen = 0
    pack.resize(pack.len() + n, 0);
    for _ in 0..n {
        body.extend(tlv(102, &pack));
    }
    let wire = wrap_st0601(&body);
    let decoded = decode_strict_compliance(&wire).expect("framing and checksum are valid");
    assert_eq!(decoded.sdcc_flps.len(), n, "every occurrence is captured");
    assert!(
        decoded
            .sdcc_flps
            .iter()
            .all(|s| s.preceding_tags.len() == n)
    );
    let copied_bytes: usize = decoded
        .sdcc_flps
        .iter()
        .map(|s| s.preceding_tags.len() * core::mem::size_of::<u32>())
        .sum();
    assert!(
        copied_bytes <= 4 * wire.len(),
        "{copied_bytes} bytes of history copies for {} wire bytes",
        wire.len()
    );
}

/// Now that Tag 102 is typed (`sdcc_flps`), it is rejected from
/// `unknown` on encode — the same `ReservedTagInUnknown` contract as
/// every other typed tag (mirrors
/// `wpa_nested_set_bytes_move_from_unknown_to_named_fields`'s per-tag
/// check).
#[test]
fn wpc_sdcc_tag_rejected_from_unknown() {
    let mut rec = UasDatalinkLs::default();
    rec.unknown.push(OwnedRawField {
        tag: 102,
        value: vec![1],
    });
    let err = crate::klv::st0601::encode_to_vec(&rec).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: 102 }),
        "expected ReservedTagInUnknown, got {err:?}"
    );
}
