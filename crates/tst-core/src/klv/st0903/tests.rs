use super::VMTI_LS_UL;
use super::decode::{decode, decode_strict};
use super::encode::{
    encode_standalone_strict_compliance, encode_strict_compliance, encode_to_vec,
    encode_to_vec_standalone, encoded_len, encoded_len_standalone,
};
use super::model::VmtiLs;
use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};
use crate::klv::pack::OwnedRawField;
use crate::klv::st0903::vtarget_pack::{VTargetPack, VTargetPackError};
use crate::test_prelude::*;

/// Build a minimal LS containing only Tag 1 (Checksum) + Tag 2 (PTS)
/// + Tag 4 (Version) — the three commonly-required tags. (Strict
///   mode validates a tighter set — see §10.1 carriage rules.)
fn minimal_ls_bytes() -> Vec<u8> {
    // tag 1 (Checksum, U16Be) = 0
    // tag 2 (PTS, U64Be) = 1_700_000_000_000_000
    // tag 4 (Version, V2) = 6 (1-byte truncated big-endian)
    let mut out = Vec::new();
    out.extend_from_slice(&[1, 2, 0, 0]);
    out.extend_from_slice(&[2, 8]);
    out.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    out.extend_from_slice(&[4, 1, 6]);
    out
}

#[test]
fn decode_minimal_ls() {
    let bytes = minimal_ls_bytes();
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.checksum, Some(0));
    assert_eq!(ls.precision_time_stamp, Some(1_700_000_000_000_000));
    assert_eq!(ls.version_number, Some(6));
    assert!(ls.targets.is_empty());
    assert!(ls.unknown.is_empty());
    assert!(ls.field_errors.is_empty());
}

#[test]
fn decode_unknown_tag_preserved() {
    let mut bytes = minimal_ls_bytes();
    // Append unknown tag 100 with 3-byte value. (Tag 100 is in the
    // gap between defined tag 13 and tag 101; safe choice for
    // "unknown.")
    bytes.extend_from_slice(&[100, 3, 0xAA, 0xBB, 0xCC]);
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.unknown.len(), 1);
    assert_eq!(ls.unknown[0].tag, 100);
    assert_eq!(ls.unknown[0].value, vec![0xAA, 0xBB, 0xCC]);
}

#[test]
fn decode_truncated_value_lenient_field_error() {
    // tag 4 (Version, V2) declares BER length 5 but only 1 byte
    // of value is present. Lenient decode does not panic;
    // field_errors capture the truncation. Strict mode rejects.
    let bytes = [4u8, 5, 0x01];
    let ls = decode(&bytes).unwrap();
    assert!(!ls.field_errors.is_empty());
}

/// Hostile bytes targeting the U64Be (Tag 2 PTS) decode path must
/// never panic. The upstream `value.len() != 8` length check
/// intercepts wrong-sized slices before the `try_into` runs, so the
/// fallible-conversion safety net is defense-in-depth — both the well-formed
/// and malformed cases below exercise the contract that decode
/// returns a value (lenient: with field_errors / strict: Err)
/// instead of panicking.
#[test]
fn decode_tag2_pts_wrong_length_no_panic() {
    // 7-byte PTS instead of 8 — caught by the length check, surfaced
    // as InvalidLength on lenient.
    let bytes = vec![2u8, 7, 0, 0, 0, 0, 0, 0, 1];
    let ls = decode(&bytes).unwrap();
    assert!(ls.precision_time_stamp.is_none());
    assert!(matches!(
        ls.field_errors.as_slice(),
        [
            KlvFieldError::InvalidLength {
                tag: 2,
                expected: 8,
                got: 7,
            },
            ..
        ] | [KlvFieldError::TruncatedField { tag: 2 }, ..]
    ));
}

#[test]
fn strict_decode_tag2_pts_wrong_length_rejected() {
    // 7-byte PTS — strict mode must Err, never panic.
    let bytes = vec![2u8, 7, 0, 0, 0, 0, 0, 0, 1];
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
            tag: 2,
            expected: 8,
            got: 7,
        }) | KlvDecodeError::FieldError(KlvFieldError::TruncatedField { tag: 2 })
    ));
}

#[test]
fn decode_tag2_pts_well_formed_still_works() {
    // The happy path still works: an 8-byte PTS decodes to the
    // expected u64.
    let mut bytes = vec![2u8, 8];
    bytes.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.precision_time_stamp, Some(1_700_000_000_000_000));
    assert!(ls.field_errors.is_empty());
}

#[test]
fn decode_with_one_target() {
    // Build a VTargetSeries (Tag 101) containing one pack.
    // Pack body: target_id=7, centroid_pixel=12345 (Tag 1 VarUint).
    // var_uint_min_len(12345) = 2 (since 12345 = 0x3039 fits in 2 bytes).
    // Tag 1 TLV inside pack: [0x01, 0x02, 0x30, 0x39].
    let mut pack_body = Vec::new();
    pack_body.push(7); // target_id BER-OID 1-byte
    pack_body.extend_from_slice(&[0x01, 0x02, 0x30, 0x39]);

    let mut series = Vec::new();
    // Each pack is BER-length-prefixed inside the series.
    series.push(pack_body.len() as u8);
    series.extend_from_slice(&pack_body);

    let mut bytes = minimal_ls_bytes();
    bytes.push(101); // tag 101 VTargetSeries
    bytes.push(series.len() as u8);
    bytes.extend_from_slice(&series);

    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.targets.len(), 1);
    assert_eq!(ls.targets[0].target_id, 7);
    assert_eq!(ls.targets[0].centroid_pixel, Some(12345));
}

#[test]
fn decode_version_v2_two_byte() {
    // Version Number as 2-byte VarUint: [0x04, 0x02, 0x01, 0x00] for value 256.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[1, 2, 0, 0]); // checksum
    bytes.extend_from_slice(&[2, 8]); // PTS
    bytes.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    bytes.extend_from_slice(&[4, 2, 0x01, 0x00]); // version = 256
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.version_number, Some(256));
}

#[test]
fn decode_with_two_targets() {
    // Two targets in series — verifies the series walker handles
    // multiple BER-prefixed packs in sequence.
    let mut series = Vec::new();
    // Target 1: target_id=1, centroid_pixel=100 (1-byte VarUint).
    let pack1: Vec<u8> = vec![1, 0x01, 0x01, 100];
    series.push(pack1.len() as u8);
    series.extend_from_slice(&pack1);
    // Target 2: target_id=2, priority=5.
    let pack2: Vec<u8> = vec![2, 4, 1, 5];
    series.push(pack2.len() as u8);
    series.extend_from_slice(&pack2);

    let mut bytes = minimal_ls_bytes();
    bytes.push(101);
    bytes.push(series.len() as u8);
    bytes.extend_from_slice(&series);

    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.targets.len(), 2);
    assert_eq!(ls.targets[0].target_id, 1);
    assert_eq!(ls.targets[0].centroid_pixel, Some(100));
    assert_eq!(ls.targets[1].target_id, 2);
    assert_eq!(ls.targets[1].priority, Some(5));
}

#[test]
fn decode_pass_through_tags() {
    // Tags 102 (Algorithm Series), 103 (Ontology Series), 13 (MIIS ID)
    // are pass-through bytes per the design.
    let mut bytes = minimal_ls_bytes();
    bytes.extend_from_slice(&[102, 2, 0xDE, 0xAD]);
    bytes.extend_from_slice(&[103, 2, 0xBE, 0xEF]);
    bytes.extend_from_slice(&[13, 3, 0xCA, 0xFE, 0x00]);
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.algorithm_series.as_deref(), Some(&[0xDEu8, 0xAD][..]));
    assert_eq!(ls.ontology_series.as_deref(), Some(&[0xBEu8, 0xEF][..]));
    assert_eq!(ls.miis_id.as_deref(), Some(&[0xCAu8, 0xFE, 0x00][..]));
}

#[test]
fn decode_zero_length_imapb_does_not_panic() {
    // Regression: Tag 11 with BER length 0 must not reach `decode_imapb`
    // (`read_signed_be(&[])` underflows `n*8-1` at n==0). A zero-length
    // item means "unknown" (ST 0107.4-17): the field stays unset and no
    // error is recorded.
    let mut bytes = minimal_ls_bytes();
    bytes.extend_from_slice(&[11, 0]); // tag 11, length 0
    let ls = decode(&bytes).unwrap();
    assert!(ls.horizontal_fov.is_none());
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
}

#[test]
fn decode_imapb_happy_path() {
    // FOV = 90.0° encoded as IMAPB(0, 180, 2) per ST 0903.6
    // §10.1.11 worked example. The spec-correct encoding for
    // 90.0° in this range is the byte pair 0x2D 0x00.
    // A signed-midpoint formula yields the wrong 0xAD 0x00 for the
    // same input; this test pins the spec result.
    let mut bytes = minimal_ls_bytes();
    bytes.extend_from_slice(&[11, 2, 0x2D, 0x00]);
    let ls = decode(&bytes).unwrap();
    let fov = ls.horizontal_fov.expect("horizontal_fov decoded");
    assert!((fov - 90.0).abs() < 0.01, "got fov={fov}, expected ~90.0");
    assert!(ls.field_errors.is_empty());
}

// ------------------------------------------------------------------
// `decode_strict` tests.
// ------------------------------------------------------------------

/// Build the minimum LS that satisfies `decode_strict`'s required-tag
/// gate: Tag 4 (Version) + Tag 6 (numTargetsReported).
/// Tags 1/2/11/12/13 are conditional and NOT enforced by `decode_strict`
/// (consumers needing carriage-aware validation post-validate).
fn minimal_strict_ls_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[4, 1, 6]); // version = 6
    out.extend_from_slice(&[6, 1, 0]); // num_targets_reported = 0
    out
}

#[test]
fn strict_decode_minimal_passes() {
    let bytes = minimal_strict_ls_bytes();
    let ls = decode_strict(&bytes).unwrap();
    assert_eq!(ls.version_number, Some(6));
    assert_eq!(ls.num_targets_reported, Some(0));
}

#[test]
fn strict_decode_with_optional_tags_passes() {
    // All required + several optional tags. Should pass strict.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[1, 2, 0, 0]); // checksum
    bytes.extend_from_slice(&[2, 8]);
    bytes.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    bytes.extend_from_slice(&[8, 2, 0x07, 0x80]); // frame_width = 1920
    bytes.extend_from_slice(&[9, 2, 0x04, 0x38]); // frame_height = 1080
    let ls = decode_strict(&bytes).unwrap();
    assert_eq!(ls.checksum, Some(0));
    assert_eq!(ls.precision_time_stamp, Some(1_700_000_000_000_000));
    assert_eq!(ls.frame_width, Some(1920));
    assert_eq!(ls.frame_height, Some(1080));
}

#[test]
fn strict_decode_missing_required_version_rejected() {
    // Tag 4 (Version) omitted, Tag 6 present. Strict should reject.
    let bytes = vec![6, 1, 0];
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::St0903MissingRequiredTag { tag: 4 }
    ));
}

#[test]
fn strict_decode_missing_required_num_targets_rejected() {
    // Tag 4 present, Tag 6 (numTargetsReported) omitted.
    let bytes = vec![4, 1, 6];
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::St0903MissingRequiredTag { tag: 6 }
    ));
}

#[test]
fn strict_decode_duplicate_tag_rejected() {
    let mut bytes = minimal_strict_ls_bytes();
    // Append a second Tag 4 (Version).
    bytes.extend_from_slice(&[4, 1, 7]);
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(err, KlvDecodeError::DuplicateTag { tag: 4, .. }));
}

#[test]
fn strict_decode_invalid_utf8_rejected() {
    let mut bytes = minimal_strict_ls_bytes();
    // Tag 3 (System Name) with bytes [0xFF, 0xFE] (invalid UTF-8).
    bytes.extend_from_slice(&[3, 2, 0xFF, 0xFE]);
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::FieldError(KlvFieldError::InvalidUtf8 { tag: 3 })
    ));
}

#[test]
fn strict_decode_unknown_tag_preserved() {
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[100, 3, 0xAA, 0xBB, 0xCC]);
    // ST 0107.5 §6 skip rule — unknown tags must round-trip through
    // strict mode too.
    let ls = decode_strict(&bytes).unwrap();
    assert_eq!(ls.unknown.len(), 1);
    assert_eq!(ls.unknown[0].tag, 100);
}

#[test]
fn strict_decode_zero_length_imapb_rejected() {
    // A zero-length item means "unknown" (ST 0107.4-17): strict mode
    // accepts it for an optional item and reads a required one as
    // missing. A wrong non-zero IMAPB length is still refused.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[11, 0]); // tag 11, BER length 0
    let ls = decode_strict(&bytes).unwrap();
    assert!(ls.horizontal_fov.is_none());
    assert!(matches!(
        decode_strict(&[4, 1, 6, 6, 0, 11, 2, 0x2D, 0x00]),
        Err(KlvDecodeError::St0903MissingRequiredTag { tag: 6 })
    ));
    let mut wrong = minimal_strict_ls_bytes();
    wrong.extend_from_slice(&[11, 1, 0x2D]);
    assert!(matches!(
        decode_strict(&wrong),
        Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
            tag: 11,
            expected: 2,
            got: 1
        }))
    ));
}

#[test]
fn strict_decode_truncated_input_rejected() {
    // Required Tag 4 with declared length 5 but only 1 byte present.
    let bytes = vec![4u8, 5, 0x01];
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(err, KlvDecodeError::Truncated { .. }));
}

#[test]
fn strict_decode_invalid_vtargetpack_rejected() {
    // Tag 101 with malformed pack body. Should route via
    // St0903InvalidVTargetPack typed variant. Pack body = [0x81]:
    // BER-OID continuation byte without a terminator → truncated
    // target_id.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[101, 2, 1, 0x81]);
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::St0903InvalidVTargetPack { .. }
    ));
}

#[test]
fn strict_vtarget_series_non_canonical_length_reports_buffer_offset() {
    // Tag 101 value = one valid pack (`04` + 4 body bytes:
    // target_id 1, tag 4 priority len 1 value 5) followed by `81 05`, a
    // long-form BER length whose value fits the short form. The
    // offending byte sits at series offset 5; the series value starts at
    // buffer offset 8 (`[4,1,6]`, `[6,1,0]`, then the `101, 8` header),
    // so the buffer-absolute offset is 13. It used to surface as the
    // slice-relative 0.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[101, 8, 0x04, 0x01, 4, 1, 0x05, 0x81, 0x05, 0x00]);
    let err = decode_strict(&bytes).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalLength { offset: 13 }),
        "{err:?}"
    );
}

#[test]
fn lenient_decode_keeps_a_target_pack_with_a_field_error() {
    // Pack = target_id 1, then tag 10 (centroid_lat_offset, 3-byte
    // IMAPB) with a 1-byte value. A per-field error must land in the
    // pack's `field_errors`, not drop the pack wholesale as
    // `TruncatedField { tag: 101 }` on the parent.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[101, 5, 0x04, 0x01, 0x0A, 0x01, 0x00]);
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.targets.len(), 1, "{ls:?}");
    assert_eq!(ls.targets[0].target_id, 1);
    assert_eq!(ls.targets[0].centroid_lat_offset, None);
    assert_eq!(
        ls.targets[0].field_errors,
        vec![KlvFieldError::InvalidLength {
            tag: 10,
            expected: 3,
            got: 1
        }]
    );
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
}

// ------------------------------------------------------------------
// `encode` + `encoded_len` round-trip + canonical-bytes.
// ------------------------------------------------------------------

#[test]
fn encode_round_trips_minimal() {
    let ls = VmtiLs {
        // `checksum` intentionally NOT set: encode_to_vec is the
        // embedded-VMTI entry which drops Tag 1 per ST 0903.6-120.
        // Use encode_to_vec_standalone for a self-checksummed
        // standalone-VMTI wire form.
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));

    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.checksum, None, "embedded-VMTI body has no checksum");
    assert_eq!(decoded.precision_time_stamp, Some(1_700_000_000_000_000));
    assert_eq!(decoded.version_number, Some(6));
    assert_eq!(decoded.num_targets_reported, Some(0));
}

#[test]
fn encode_round_trips_with_targets() {
    let ls = VmtiLs {
        // `checksum` intentionally NOT set per ST 0903.6-120
        // (embedded-VMTI omits Tag 1).
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(2),
        frame_width: Some(3840),
        frame_height: Some(2160),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        source_sensor: Some("EO/IR Camera 1".to_string()),
        targets: vec![
            VTargetPack {
                target_id: 1,
                centroid_pixel: Some(8_294_400),
                priority: Some(0),
                confidence_level: Some(95),
                target_color: Some([0xFF, 0x00, 0x00]),
                ..Default::default()
            },
            VTargetPack {
                target_id: 2,
                centroid_pixel: Some(4_147_200),
                priority: Some(1),
                confidence_level: Some(80),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();
    let decoded = decode(&bytes).unwrap();

    assert_eq!(decoded.frame_width, Some(3840));
    assert_eq!(decoded.frame_height, Some(2160));
    assert_eq!(decoded.source_sensor.as_deref(), Some("EO/IR Camera 1"));
    // FOV uses IMAPB(0, 180, 2) — precision is (180-0)/(2^16-1) ≈ 0.00275°
    assert!((decoded.horizontal_fov.unwrap() - 45.0).abs() < 0.01);
    assert!((decoded.vertical_fov.unwrap() - 30.0).abs() < 0.01);
    assert_eq!(decoded.targets.len(), 2);
    assert_eq!(decoded.targets[0].target_id, 1);
    assert_eq!(decoded.targets[0].confidence_level, Some(95));
    assert_eq!(decoded.targets[1].target_id, 2);
}

#[test]
fn encode_preserves_unknown_tags() {
    let ls = VmtiLs {
        // `checksum` intentionally NOT set per ST 0903.6-120
        // (embedded-VMTI omits Tag 1).
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 100,
            value: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }],
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 100);
    assert_eq!(decoded.unknown[0].value, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn encoded_len_matches_encode() {
    let ls = VmtiLs {
        // `checksum` intentionally NOT set per ST 0903.6-120
        // (embedded-VMTI omits Tag 1).
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        frame_width: Some(1920),
        frame_height: Some(1080),
        targets: vec![VTargetPack {
            target_id: 1,
            centroid_pixel: Some(123),
            ..Default::default()
        }],
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));
}

#[test]
fn encode_canonical_byte_layout() {
    // Locks in the wire format for a known LS shape. Catches
    // accidental field-order changes in `encode` (which round-trip
    // tests miss because `decode` is order-agnostic) and catches
    // `encoded_len` drift relative to `encode`.
    let ls = VmtiLs {
        version_number: Some(6),       // Tag 4, V2 → 1 byte [0x06]
        num_targets_reported: Some(0), // Tag 6, V3 → 1 byte [0x00]
        frame_width: Some(1920),       // Tag 8, V3 → 2 bytes [0x07, 0x80]
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();

    // Expected wire form (ascending tag order):
    let expected: Vec<u8> = vec![
        // Tag 4, len 1, value [0x06]
        0x04, 0x01, 0x06, // Tag 6, len 1, value [0x00]
        0x06, 0x01, 0x00, // Tag 8, len 2, value [0x07, 0x80]
        0x08, 0x02, 0x07, 0x80,
    ];
    assert_eq!(
        bytes, expected,
        "encode produced unexpected byte layout — \
         field order changed or TLV bytes are wrong"
    );

    assert_eq!(
        bytes.len(),
        encoded_len(&ls),
        "encoded_len disagrees with encode_to_vec output length"
    );
}

// --- Checksum-placement regression tests ---

#[test]
fn encode_omits_tag1_checksum_per_st0903_6_120() {
    // ST 0903.6-120: embedded-VMTI MUST omit Tag 1 (checkSum).
    // `encode_to_vec` is the body-only (embedded carriage) entry —
    // it must not emit Tag 1 even when the caller populates the field.
    let ls = VmtiLs {
        checksum: Some(0xDEAD), // caller-supplied; encoder must drop
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    // Walk the TLVs and assert none has tag == 1.
    let mut cursor = bytes.as_slice();
    while !cursor.is_empty() {
        let tag = cursor[0];
        assert_ne!(tag, 1, "encode_to_vec must omit Tag 1 per ST 0903.6-120");
        let (len, rest) = crate::klv::length::read_ber(&cursor[1..]).unwrap();
        cursor = &rest[len..];
    }
}

#[test]
fn encode_drops_caller_supplied_checksum() {
    // The two encode entry points have asymmetric contracts:
    // `encode_to_vec` (embedded) drops `ls.checksum`; the new
    // `encode_to_vec_standalone` computes its own.
    // Pin the embedded-side drop contract.
    let ls_with = VmtiLs {
        checksum: Some(0xABCD),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let ls_without = VmtiLs {
        checksum: None,
        num_targets_reported: Some(0),
        ..Default::default()
    };
    assert_eq!(
        encode_to_vec(&ls_with).unwrap(),
        encode_to_vec(&ls_without).unwrap(),
        "encode_to_vec must produce identical bytes regardless of ls.checksum"
    );
}

#[test]
fn encode_standalone_emits_tag1_last_per_st0903_4_17() {
    // ST 0903.4-17 / ST 0903.6-119: standalone-VMTI Tag 1 last.
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let bytes = encode_to_vec_standalone(&ls).unwrap();

    // Skip the 16-byte UL + outer BER length to find the body.
    assert_eq!(&bytes[..16], &VMTI_LS_UL);
    let (_outer_len, body) = crate::klv::length::read_ber(&bytes[16..]).unwrap();

    // Walk TLVs and collect tag IDs in emission order.
    let mut cursor = body;
    let mut tags = Vec::new();
    while !cursor.is_empty() {
        let tag = cursor[0];
        tags.push(tag);
        let (len, rest) = crate::klv::length::read_ber(&cursor[1..]).unwrap();
        cursor = &rest[len..];
    }
    // Tag 2 first, Tag 1 last.
    assert_eq!(tags.first(), Some(&2u8), "Tag 2 (PTS) must be first");
    assert_eq!(tags.last(), Some(&1u8), "Tag 1 (checkSum) must be last");
}

#[test]
fn encode_standalone_checksum_matches_running_sum_16() {
    // Computed checksum must equal running_sum_16 over [UL .. start of value].
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let bytes = encode_to_vec_standalone(&ls).unwrap();
    // Last 2 bytes of the wire record are the Tag 1 value.
    let cksum_value_offset = bytes.len() - 2;
    let expected = crate::klv::checksum::checksum_running_sum_16(&bytes[..cksum_value_offset]);
    let got = u16::from_be_bytes([bytes[cksum_value_offset], bytes[cksum_value_offset + 1]]);
    assert_eq!(got, expected, "checksum value must match running_sum_16");
}

#[test]
fn encode_standalone_round_trips_via_decode() {
    // Wrap, then unwrap (peel UL + outer BER length), decode the
    // body, and check the typed fields round-trip. `decode` does
    // not verify the checksum (the field is captured as-is for
    // observability) — the round-trip is asserted at the typed
    // layer only.
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        frame_width: Some(1920),
        frame_height: Some(1080),
        ..Default::default()
    };
    let bytes = encode_to_vec_standalone(&ls).unwrap();

    assert_eq!(&bytes[..16], &VMTI_LS_UL);
    let (outer_len, body) = crate::klv::length::read_ber(&bytes[16..]).unwrap();
    assert_eq!(outer_len, body.len(), "outer BER length covers full body");

    let decoded = decode(body).unwrap();
    assert_eq!(decoded.precision_time_stamp, Some(1_700_000_000_000_000));
    assert_eq!(decoded.version_number, Some(6));
    assert_eq!(decoded.num_targets_reported, Some(1));
    assert_eq!(decoded.frame_width, Some(1920));
    assert_eq!(decoded.frame_height, Some(1080));
    // The decoded checksum captures the Tag 1 value that was emitted
    // by encode_standalone — verifying it's the running-sum-16.
    let cksum_value_offset = bytes.len() - 2;
    let expected = crate::klv::checksum::checksum_running_sum_16(&bytes[..cksum_value_offset]);
    assert_eq!(decoded.checksum, Some(expected));
}

#[test]
fn encoded_len_standalone_matches_encode_standalone() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(2),
        frame_width: Some(3840),
        frame_height: Some(2160),
        ..Default::default()
    };
    let bytes = encode_to_vec_standalone(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len_standalone(&ls));
}

// ---------- BER-OID walker round-trip tests ----------
//
// The ST 0903 §10.1 typed universe (tags 1..=103) all fit in a single
// BER-OID byte (`write_ber_oid(N) == [N]` for `N ≤ 127`). The walker
// migration from `cursor[0]`-style single-byte reads to BER-OID
// decode is byte-identical for these. The tests below pin that
// invariant and exercise the multi-byte boundaries (128 / 16383 /
// 16384) that the migration unlocks for forward-compat tags. Mirrors
// the ST 0102 BER-OID test suite (`unknown_tag_*_round_trips_*_byte_ber_oid`).

/// Every defined tag (1 to 103) is written as its one BER-OID byte: a
/// maximally-populated LS starts `02 08`, as ST 0903.6 §10.1.2's example.
#[test]
fn defined_tags_are_written_as_one_byte_tags() {
    use crate::klv::st0903::vtarget_pack::VTargetPack;

    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        vmti_system_name: Some("test-cam".to_string()),
        version_number: Some(6),
        total_targets_in_frame: Some(4),
        num_targets_reported: Some(2),
        frame_width: Some(1920),
        frame_height: Some(1080),
        source_sensor: Some("EO".to_string()),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        miis_id: Some(vec![0x11, 0x22, 0x33]),
        targets: vec![VTargetPack {
            target_id: 1,
            ..Default::default()
        }],
        algorithm_series: Some(vec![0xAA]),
        ontology_series: Some(vec![0xBB]),
        ..Default::default()
    };

    let bytes = encode_to_vec(&ls).unwrap();
    // Baseline: each tag emits as a single byte. Spot-check
    // the first few wire bytes — tag 2 (PTS, U64Be) header is
    // `[0x02, 0x08, ...]`. If the walker ever regressed to a
    // 2-byte BER-OID for a < 128 value, this would catch it.
    assert_eq!(bytes[0], 0x02, "tag 2 must encode as single byte 0x02");
    assert_eq!(bytes[1], 0x08, "tag 2 BER length 8");
    assert_eq!(bytes.len(), encoded_len(&ls));

    // Round-trip through both lenient and strict decoders.
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.vmti_system_name.as_deref(), Some("test-cam"));
    assert_eq!(decoded.num_targets_reported, Some(2));
    assert!(decoded.unknown.is_empty());
}

/// The example values of ST 0903.6 §10.1.2 to §10.1.12 give the example
/// items printed there, and those items decode to the example values (the
/// two fields of view within one step of IMAPB(0, 180, 2), 2^-7 degrees).
#[test]
fn vmti_ls_items_match_the_standards_examples() {
    let ls = VmtiLs {
        precision_time_stamp: Some(987_654_321_000_000),
        vmti_system_name: Some("DSTO_ADSS_VMTI".to_string()),
        version_number: Some(6),
        total_targets_in_frame: Some(28),
        num_targets_reported: Some(14),
        frame_width: Some(1920),
        frame_height: Some(1080),
        source_sensor: Some("EO Nose".to_string()),
        horizontal_fov: Some(12.5),
        vertical_fov: Some(10.0),
        ..Default::default()
    };
    let items: &[&[u8]] = &[
        &[0x02, 0x08, 0x00, 0x03, 0x82, 0x44, 0x30, 0xF6, 0xCE, 0x40],
        &[
            0x03, 0x0E, 0x44, 0x53, 0x54, 0x4F, 0x5F, 0x41, 0x44, 0x53, 0x53, 0x5F, 0x56, 0x4D,
            0x54, 0x49,
        ],
        &[0x04, 0x01, 0x06],
        &[0x05, 0x01, 0x1C],
        &[0x06, 0x01, 0x0E],
        &[0x08, 0x02, 0x07, 0x80],
        &[0x09, 0x02, 0x04, 0x38],
        &[0x0A, 0x07, 0x45, 0x4F, 0x20, 0x4E, 0x6F, 0x73, 0x65],
        &[0x0B, 0x02, 0x06, 0x40],
        &[0x0C, 0x02, 0x05, 0x00],
    ];
    let body = items.concat();
    assert_eq!(encode_to_vec(&ls).unwrap(), body);
    let decoded = decode(&body).unwrap();
    assert!(
        decoded.field_errors.is_empty(),
        "{:?}",
        decoded.field_errors
    );
    let step = 1.0 / 128.0;
    for (got, want) in [(decoded.horizontal_fov, 12.5), (decoded.vertical_fov, 10.0)] {
        assert!((got.unwrap() - want).abs() <= step, "{got:?} vs {want}");
    }
    let fov_free = VmtiLs {
        horizontal_fov: None,
        vertical_fov: None,
        ..decoded
    };
    assert_eq!(
        fov_free,
        VmtiLs {
            horizontal_fov: None,
            vertical_fov: None,
            ..ls
        }
    );
}

crate::klv::ber_oid_unknown_tag_tests!(VmtiLs {
    precision_time_stamp: Some(1_700_000_000_000_000),
    version_number: Some(6),
    num_targets_reported: Some(0),
    ..Default::default()
});

/// Strict decode also preserves multi-byte BER-OID unknown tags per
/// ST 0107.5 §6 (strict mode is about codepoint legality, not
/// future-spec rejection). Mirrors `klv::st0102::tests::
/// strict_preserves_unknown_tag` extended to the BER-OID boundary.
#[test]
fn strict_decode_preserves_multi_byte_ber_oid_unknown() {
    let ls = VmtiLs {
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 200,
            value: b"forward-compat".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    let decoded = decode_strict(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 200);
    assert_eq!(decoded.unknown[0].value, b"forward-compat");
}

/// Strict mode catches a duplicate multi-byte BER-OID tag. A `[bool; 256]`
/// seen array would silently collide tag 256 with tag 0 (`as u8`-narrowing);
/// the dedup runs on the full `u32` tag value so multi-byte duplicates are
/// caught.
#[test]
fn strict_decode_rejects_duplicate_multi_byte_ber_oid_tag() {
    // Hand-craft a body with two copies of BER-OID tag 200 (encoded
    // `0x81 0x48`). Both have an empty value.
    let mut bytes = Vec::new();
    // Tag 4 (Version, required), length 1, value 6.
    bytes.extend_from_slice(&[4, 1, 6]);
    // Tag 6 (NumTargetsReported, required), length 1, value 0.
    bytes.extend_from_slice(&[6, 1, 0]);
    // Tag 200 (BER-OID = 0x81 0x48), length 0.
    bytes.extend_from_slice(&[0x81, 0x48, 0]);
    bytes.extend_from_slice(&[0x81, 0x48, 0]);
    let err = decode_strict(&bytes).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 200, .. }),
        "expected DuplicateTag(200), got {err:?}"
    );
}

// ------------------------------------------------------------------
// Unknown-bucket typed-tag guard tests.
// ------------------------------------------------------------------

#[test]
fn encode_rejects_typed_tag_in_vmtils_unknown() {
    // A caller who places a typed top-level tag (Tag 4 = Version Number)
    // in `unknown` would make the encoder emit Tag 4 twice — output that
    // ST 0903 decode_strict rejects as DuplicateTag. Guard it, mirroring
    // st0601::encode's is_reserved_or_typed_tag (ST 0107.5 §6.3.4 single-use).
    use crate::error::KlvEncodeError;
    let ls = VmtiLs {
        unknown: vec![OwnedRawField {
            tag: 4,
            value: vec![0x06],
        }],
        ..Default::default()
    };
    let err = encode_to_vec(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: 4 }),
        "got {err:?}"
    );
}

#[test]
fn encode_allows_truly_unknown_tag_in_vmtils_unknown() {
    // A forward-compat tag the typed model doesn't know (e.g. 200) must
    // still pass through. This is the legitimate use of `unknown`.
    let ls = VmtiLs {
        unknown: vec![OwnedRawField {
            tag: 200,
            value: vec![0xAB, 0xCD],
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).expect("forward-compat unknown tag must encode");
    assert!(!bytes.is_empty());
}

#[test]
fn encode_rejects_typed_tag_in_vtargetpack_unknown() {
    // A caller who places a typed pack tag (Tag 5 = Confidence Level)
    // in a VTargetPack's `unknown` would make write_pack emit Tag 5 twice
    // — output that decode would reject as DuplicateTag. Guard it,
    // mirroring the same pattern for top-level ST 0903 and ST 0102.
    use crate::error::KlvEncodeError;
    let ls = VmtiLs {
        targets: vec![VTargetPack {
            target_id: 1,
            unknown: vec![OwnedRawField {
                tag: 5,
                value: vec![0x50],
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = encode_to_vec(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: 5 }),
        "got {err:?}"
    );
}

#[test]
fn encode_allows_truly_unknown_tag_in_vtargetpack_unknown() {
    // A forward-compat pack tag (e.g. 200) must still pass through.
    let ls = VmtiLs {
        targets: vec![VTargetPack {
            target_id: 1,
            unknown: vec![OwnedRawField {
                tag: 200,
                value: vec![0xAB, 0xCD],
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).expect("forward-compat unknown pack tag must encode");
    assert!(!bytes.is_empty());
}

// ------------------------------------------------------------------
// encode_strict_compliance + encode_standalone_strict_compliance tests.
// ------------------------------------------------------------------

/// An MIIS Core Identifier `st1204::decode` accepts (34 bytes).
const CORE_ID: [u8; 34] = [
    0x01, 0x70, 0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F, 0x2E,
    0xB2, 0xDA, 0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A, 0xB9, 0x6A,
    0x36, 0x45,
];

/// Helper: a VmtiLs satisfying standalone-required fields (tags 2+4+6+11+12+13),
/// with the frame width its pixel number needs (ST 0903.6-121).
fn full_standalone_ls() -> VmtiLs {
    VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        frame_width: Some(1920),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        miis_id: Some(CORE_ID.to_vec()),
        targets: vec![VTargetPack {
            target_id: 1,
            centroid_pixel: Some(100),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn st0903_strict_rejects_empty_vtarget_pack() {
    // A VTargetPack with only target_id set (no TLV items) must be
    // rejected by both encode_strict_compliance and
    // encode_standalone_strict_compliance per ST 0903.4-10.
    let ls = VmtiLs {
        version_number: Some(6),
        num_targets_reported: Some(1),
        targets: vec![VTargetPack {
            target_id: 1,
            // all Option fields None, unknown empty — pack is TLV-empty
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = encode_strict_compliance(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::VTargetPackEmpty { target_id: 1 }),
        "expected VTargetPackEmpty{{1}}, got {err:?}"
    );
}

#[test]
fn st0903_strict_rejects_duplicate_target_id() {
    // Two packs with the same target_id (both non-empty) must be rejected
    // per ST 0903.6-126.
    let ls = VmtiLs {
        version_number: Some(6),
        num_targets_reported: Some(2),
        targets: vec![
            VTargetPack {
                target_id: 7,
                centroid_pixel: Some(100),
                ..Default::default()
            },
            VTargetPack {
                target_id: 7, // duplicate
                centroid_pixel: Some(200),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let err = encode_strict_compliance(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::DuplicateTargetId { target_id: 7 }),
        "expected DuplicateTargetId{{7}}, got {err:?}"
    );
}

#[test]
fn st0903_strict_rejects_missing_version() {
    // Tag 4 (version_number) is unconditionally required per ST 0903.5-99.
    let ls = VmtiLs {
        version_number: None, // missing
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let err = encode_strict_compliance(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 4, .. }),
        "expected MissingMandatoryItem{{tag:4}}, got {err:?}"
    );
}

#[test]
fn st0903_standalone_strict_rejects_missing_fov() {
    // Tag 11 (horizontal_fov) is required in standalone carriage per
    // ST 0903.6-122.
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        horizontal_fov: None, // missing
        vertical_fov: Some(30.0),
        miis_id: Some(vec![0x11]),
        ..Default::default()
    };
    let err = encode_standalone_strict_compliance(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 11, .. }),
        "expected MissingMandatoryItem{{tag:11}}, got {err:?}"
    );
}

#[test]
fn st0903_standalone_strict_rejects_offset_tag() {
    // VTargetPack with centroid_lat_offset (tag 10) set is forbidden in
    // standalone VMTI per ST 0903.6-116.
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        miis_id: Some(vec![0x11]),
        targets: vec![VTargetPack {
            target_id: 1,
            centroid_pixel: Some(100), // gives the pack at least one TLV
            centroid_lat_offset: Some(1.0), // forbidden in standalone
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = encode_standalone_strict_compliance(&ls).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ForbiddenStandaloneOffset { tag: 10 }),
        "expected ForbiddenStandaloneOffset{{tag:10}}, got {err:?}"
    );
}

#[test]
fn st0903_embedded_strict_allows_offset_tag() {
    // In embedded mode, parent-relative offset tags are ALLOWED per ST 0903.
    // encode_strict_compliance must NOT check for offset tags.
    let ls = VmtiLs {
        version_number: Some(6),
        num_targets_reported: Some(1),
        targets: vec![VTargetPack {
            target_id: 1,
            centroid_lat_offset: Some(1.5), // allowed in embedded
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = encode_strict_compliance(&ls);
    assert!(
        result.is_ok(),
        "embedded strict must allow offset tags; got {result:?}"
    );
}

#[test]
fn st0903_standalone_strict_full_round_trips() {
    // A fully-populated standalone VMTI LS passes strict encode and its
    // output passes decode_strict.
    let ls = full_standalone_ls();
    let bytes = encode_standalone_strict_compliance(&ls)
        .expect("full standalone ls must encode without error");

    // Peel UL + outer BER length and decode the body.
    assert_eq!(&bytes[..16], &VMTI_LS_UL);
    let (outer_len, body) = crate::klv::length::read_ber(&bytes[16..]).unwrap();
    assert_eq!(outer_len, body.len());

    let decoded = decode_strict(body).expect("decode_strict must accept valid strict-encoded body");
    assert_eq!(decoded.version_number, Some(6));
    assert_eq!(decoded.num_targets_reported, Some(1));
    assert_eq!(decoded.precision_time_stamp, Some(1_700_000_000_000_000));
}

#[test]
fn st0903_lenient_encode_still_accepts_partial() {
    // Regression pin: the default lenient encode/encode_standalone paths must
    // NOT reject sparse records — strict is opt-in only.
    let sparse = VmtiLs {
        version_number: None,       // no version
        num_targets_reported: None, // no count
        ..Default::default()
    };
    assert!(
        encode_to_vec(&sparse).is_ok(),
        "lenient encode must accept sparse records"
    );
    assert!(
        encode_to_vec_standalone(&sparse).is_ok(),
        "lenient encode_standalone must accept sparse records"
    );
}

/// PT-KLV-strictwire companion: VmtiLs::precision_time_stamp (u64) encodes
/// as 8 raw big-endian bytes — the full u64 range is always losslessly
/// representable. Probe at u64::MAX confirms no truncation on encode→decode.
#[test]
fn precision_time_stamp_u64_max_round_trips() {
    let ls = VmtiLs {
        precision_time_stamp: Some(u64::MAX),
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let encoded = encode_to_vec(&ls).unwrap();
    let decoded = super::decode::decode(&encoded).unwrap();
    assert_eq!(
        decoded.precision_time_stamp,
        Some(u64::MAX),
        "precision_time_stamp u64::MAX must round-trip losslessly as 8-byte BE"
    );
}

// ------------------------------------------------------------------
// The strict encoders write only what `decode_strict` accepts.
// ------------------------------------------------------------------

/// An encoder of a whole VMTI record.
type Encoder = fn(&VmtiLs) -> Result<Vec<u8>, KlvEncodeError>;

/// Both strict encoders, so a rule is checked on each.
fn strict_encoders() -> [Encoder; 2] {
    [
        encode_strict_compliance,
        encode_standalone_strict_compliance,
    ]
}

#[test]
fn strict_encode_refuses_names_strict_decode_refuses() {
    // ST 0903.6 §10.1.3 (V32) and §10.1.10 (V128) count characters.
    let name = |s: String| VmtiLs {
        vmti_system_name: Some(s),
        ..full_standalone_ls()
    };
    let sensor = |s: String| VmtiLs {
        source_sensor: Some(s),
        ..full_standalone_ls()
    };
    for encode in strict_encoders() {
        assert!(
            matches!(
                encode(&name("a".repeat(33))),
                Err(KlvEncodeError::StringTooLong { tag: 3, max: 32 })
            ),
            "33 characters"
        );
        assert!(
            matches!(
                encode(&sensor("a".repeat(129))),
                Err(KlvEncodeError::StringTooLong { tag: 10, max: 128 })
            ),
            "129 characters"
        );
        // 32 two-byte characters are 64 bytes and still 32 characters.
        encode(&name("é".repeat(32))).expect("32 characters pass");
        encode(&sensor("é".repeat(128))).expect("128 characters pass");
    }
    let body = encode_strict_compliance(&name("é".repeat(32))).unwrap();
    let decoded = decode_strict(&body).expect("strict decode accepts what strict encode wrote");
    assert_eq!(decoded.vmti_system_name, Some("é".repeat(32)));
}

#[test]
fn every_vmti_encoder_refuses_a_v3_value_above_24_bits() {
    // Items 5, 6, 8 and 9 are V3 (ST 0903.6 §10.1): at most three bytes,
    // which is what both decoders read.
    let set = |tag: u32, v: u32| {
        let mut ls = full_standalone_ls();
        ls.frame_width = Some(1920);
        match tag {
            5 => ls.total_targets_in_frame = Some(v),
            6 => ls.num_targets_reported = Some(v),
            8 => ls.frame_width = Some(v),
            9 => ls.frame_height = Some(v),
            _ => unreachable!(),
        }
        ls
    };
    let encoders: [Encoder; 4] = [
        encode_to_vec,
        encode_to_vec_standalone,
        encode_strict_compliance,
        encode_standalone_strict_compliance,
    ];
    for tag in [5u32, 6, 8, 9] {
        for encode in encoders {
            let err = encode(&set(tag, 0x0100_0000)).unwrap_err();
            assert!(
                matches!(err, KlvEncodeError::OutOfRange { tag: t, max, .. } if t == tag && max == 16_777_215.0),
                "tag {tag}: {err:?}"
            );
            let bytes = encode(&set(tag, 0x00FF_FFFF)).expect("2^24 - 1 fits three bytes");
            assert!(!bytes.is_empty());
        }
        let mut buf = vec![0u8; 256];
        assert!(matches!(
            super::encode::encode_standalone(&set(tag, 0x0100_0000), &mut buf),
            Err(KlvEncodeError::OutOfRange { tag: t, .. }) if t == tag
        ));
        let decoded = decode(&encode_to_vec(&set(tag, 0x00FF_FFFF)).unwrap()).unwrap();
        assert!(
            decoded.field_errors.is_empty(),
            "{:?}",
            decoded.field_errors
        );
    }
}

#[test]
fn strict_encode_mirrors_the_vtarget_u8_ranges() {
    // ST 0903.6 §10.2.2.5 (priority 1 to 255), §10.2.2.6 (confidence 0 to
    // 100), §10.2.2.8 (percentage 1 to 100), Table 5 (detection status 0
    // to 4).
    let with = |tag: u32, v: u8| {
        let mut pack = VTargetPack {
            target_id: 1,
            centroid_pixel: Some(100),
            target_location: Some(vec![0x01]),
            ..Default::default()
        };
        match tag {
            4 => pack.priority = Some(v),
            5 => pack.confidence_level = Some(v),
            7 => pack.percentage_of_target_pixels = Some(v),
            23 => pack.detection_status = Some(v),
            _ => unreachable!(),
        }
        VmtiLs {
            targets: vec![pack],
            ..full_standalone_ls()
        }
    };
    for (tag, bad) in [(4u32, 0u8), (5, 101), (7, 0), (7, 101), (23, 5)] {
        for encode in strict_encoders() {
            let err = encode(&with(tag, bad)).unwrap_err();
            assert!(
                matches!(err, KlvEncodeError::OutOfRange { tag: t, .. } if t == tag),
                "tag {tag} = {bad}: {err:?}"
            );
        }
        encode_to_vec(&with(tag, bad)).expect("the ordinary encoder still writes it");
    }
    for (tag, good) in [
        (4u32, 1u8),
        (4, 255),
        (5, 0),
        (5, 100),
        (7, 1),
        (7, 100),
        (23, 0),
        (23, 4),
    ] {
        for encode in strict_encoders() {
            encode(&with(tag, good)).unwrap_or_else(|e| panic!("tag {tag} = {good}: {e:?}"));
        }
    }
}

// ------------------------------------------------------------------
// Zero-length items and the empty string (ST 0107.4-17, ST 0107.5
// §6.3.3.2).
// ------------------------------------------------------------------

#[test]
fn vmti_zero_length_items_are_unknown() {
    // `03 00` and `08 00`: a zero-length item says "value unknown".
    let bytes = [0x03, 0x00, 0x04, 0x01, 0x06, 0x06, 0x01, 0x00, 0x08, 0x00];
    for ls in [decode(&bytes).unwrap(), decode_strict(&bytes).unwrap()] {
        assert_eq!(ls.vmti_system_name, None);
        assert_eq!(ls.frame_width, None);
        assert_eq!(ls.version_number, Some(6));
        assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
    }
}

#[test]
fn vmti_one_nul_string_is_empty() {
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[0x03, 0x01, 0x00, 0x0A, 0x01, 0x00]);
    for ls in [decode(&bytes).unwrap(), decode_strict(&bytes).unwrap()] {
        assert_eq!(ls.vmti_system_name.as_deref(), Some(""));
        assert_eq!(ls.source_sensor.as_deref(), Some(""));
    }
}

#[test]
fn vmti_empty_string_is_written_as_one_nul() {
    let ls = VmtiLs {
        vmti_system_name: Some(String::new()),
        source_sensor: Some(String::new()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes, [0x03, 0x01, 0x00, 0x0A, 0x01, 0x00]);
    assert_eq!(encoded_len(&ls), bytes.len());
    assert_eq!(decode(&bytes).unwrap(), ls);
}

#[test]
fn vmti_strict_zero_length_required_item_is_missing() {
    assert!(matches!(
        decode_strict(&[0x04, 0x00, 0x06, 0x01, 0x00]),
        Err(KlvDecodeError::St0903MissingRequiredTag { tag: 4 })
    ));
    assert!(matches!(
        decode_strict(&[0x04, 0x01, 0x06, 0x06, 0x00]),
        Err(KlvDecodeError::St0903MissingRequiredTag { tag: 6 })
    ));
}

#[test]
fn a_zero_length_duplicate_after_a_value_unsets_it() {
    // The last occurrence of a tag decides, a zero-length one too.
    let bytes = [
        0x03, 0x02, b'a', b'b', 0x08, 0x01, 0x05, 0x03, 0x00, 0x08, 0x00,
    ];
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.vmti_system_name, None);
    assert_eq!(ls.frame_width, None);
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
    assert!(matches!(
        decode_strict(&bytes),
        Err(KlvDecodeError::DuplicateTag { tag: 3, offset: 7 })
    ));
}

#[test]
fn a_zero_length_duplicate_before_a_value_keeps_it() {
    let bytes = [
        0x03, 0x00, 0x08, 0x00, 0x03, 0x02, b'a', b'b', 0x08, 0x01, 0x05,
    ];
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.vmti_system_name.as_deref(), Some("ab"));
    assert_eq!(ls.frame_width, Some(5));
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
    assert!(matches!(
        decode_strict(&bytes),
        Err(KlvDecodeError::DuplicateTag { tag: 3, offset: 4 })
    ));
}

// ------------------------------------------------------------------
// VTarget strict validation and the legal IMAP specials.
// ------------------------------------------------------------------

/// A strict-minimal VMTI body whose Item 101 holds `packs`, each
/// BER-length-prefixed as the series frames them.
fn with_packs(packs: &[&[u8]]) -> Vec<u8> {
    let mut series = Vec::new();
    for p in packs {
        series.push(p.len() as u8);
        series.extend_from_slice(p);
    }
    let mut bytes = minimal_strict_ls_bytes();
    bytes.push(101);
    bytes.push(series.len() as u8);
    bytes.extend_from_slice(&series);
    bytes
}

/// The `VTargetPackError` strict decode reports for `bytes`.
fn strict_pack_error(bytes: &[u8]) -> VTargetPackError {
    match decode_strict(bytes) {
        Err(KlvDecodeError::St0903InvalidVTargetPack { reason, .. }) => reason,
        other => panic!("expected a VTarget pack refusal, got {other:?}"),
    }
}

#[test]
fn vtarget_strict_refuses_a_repeated_item() {
    // ST 0903.5-98: an item at most once per Local Set instance.
    let bytes = with_packs(&[&[0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02]]);
    assert_eq!(
        strict_pack_error(&bytes),
        VTargetPackError::DuplicateTag { tag: 1 }
    );
    let ls = decode(&bytes).unwrap();
    assert_eq!(
        ls.targets[0].centroid_pixel,
        Some(2),
        "lenient: the last wins"
    );
}

#[test]
fn vtarget_strict_refuses_a_repeated_unknown_tag() {
    let bytes = with_packs(&[&[0x01, 0x7F, 0x01, 0xAA, 0x7F, 0x01, 0xBB]]);
    assert_eq!(
        strict_pack_error(&bytes),
        VTargetPackError::DuplicateTag { tag: 127 }
    );
    assert_eq!(decode(&bytes).unwrap().targets[0].unknown.len(), 2);
}

#[test]
fn vtarget_strict_refuses_a_non_canonical_length() {
    let bytes = with_packs(&[&[0x01, 0x01, 0x81, 0x01, 0x05]]);
    assert_eq!(
        strict_pack_error(&bytes),
        VTargetPackError::NonCanonicalLength { tag: 1 }
    );
    assert_eq!(decode(&bytes).unwrap().targets[0].centroid_pixel, Some(5));
}

#[test]
fn vtarget_strict_refuses_an_empty_pack() {
    // ST 0903.4-10: at least one item follows the targetId.
    let bytes = with_packs(&[&[0x01]]);
    assert_eq!(strict_pack_error(&bytes), VTargetPackError::Empty);
    assert_eq!(decode(&bytes).unwrap().targets[0].target_id, 1);
}

#[test]
fn vmti_strict_refuses_a_repeated_target_id() {
    // ST 0903.6-126: a targetId once per series.
    let bytes = with_packs(&[&[0x01, 0x01, 0x01, 0x05], &[0x01, 0x01, 0x01, 0x06]]);
    assert_eq!(
        strict_pack_error(&bytes),
        VTargetPackError::DuplicateTargetId { target_id: 1 }
    );
    assert_eq!(decode(&bytes).unwrap().targets.len(), 2);
}

#[test]
fn vtarget_strict_refuses_a_wide_or_padded_target_id() {
    // Table 10: the targetId is BER-OID V9, at most nine bytes; a leading
    // 0x80 group is padding.
    let ten = [
        0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01, 0x01, 0x01, 0x05,
    ];
    let wide = with_packs(&[&ten]);
    assert_eq!(strict_pack_error(&wide), VTargetPackError::TargetIdTooWide);
    assert_eq!(decode(&wide).unwrap().targets[0].target_id, (1 << 63) + 1);
    let padded = with_packs(&[&[0x80, 0x01, 0x01, 0x01, 0x05]]);
    assert_eq!(
        strict_pack_error(&padded),
        VTargetPackError::NonCanonicalLength { tag: 0 }
    );
    assert_eq!(decode(&padded).unwrap().targets[0].target_id, 1);
    // Nine bytes, no padding: the largest id the set can carry.
    let nine = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F, 0x04, 0x01, 0x05,
    ];
    let ls = decode_strict(&with_packs(&[&nine])).unwrap();
    assert_eq!(ls.targets[0].target_id, (1 << 63) - 1);
}

#[test]
fn vtarget_strict_refuses_out_of_range_u8_values() {
    for (tag, v) in [(4u8, 0u8), (5, 101), (7, 0), (7, 101), (23, 5)] {
        let bytes = with_packs(&[&[0x01, tag, 0x01, v]]);
        assert_eq!(
            strict_pack_error(&bytes),
            VTargetPackError::ValueOutOfRange {
                tag: u32::from(tag),
                value: v
            },
            "tag {tag} = {v}"
        );
        assert_eq!(decode(&bytes).unwrap().targets.len(), 1, "lenient keeps it");
    }
    for (tag, v) in [
        (4u8, 1u8),
        (4, 255),
        (5, 0),
        (5, 100),
        (7, 1),
        (7, 100),
        (23, 4),
    ] {
        let ls = decode_strict(&with_packs(&[&[0x01, tag, 0x01, v]]))
            .unwrap_or_else(|e| panic!("tag {tag} = {v}: {e:?}"));
        assert_eq!(ls.targets.len(), 1);
    }
}

#[test]
fn vmti_specials_decode_without_error_and_unset() {
    // ST 0903.6 §9.1.1: IMAP_ABOVE_MAXIMUM is sent in place of the
    // measurement, and 0xC1 padded to the item's length is "Invalid".
    let pack = [0x01, 0x0C, 0x02, 0xE1, 0x00, 0x0D, 0x03, 0xC1, 0x00, 0x00];
    let mut bytes = with_packs(&[&pack]);
    bytes.extend_from_slice(&[0x0B, 0x02, 0xE1, 0x00]);
    for ls in [decode(&bytes).unwrap(), decode_strict(&bytes).unwrap()] {
        assert_eq!(ls.horizontal_fov, None);
        assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
        let t = &ls.targets[0];
        assert_eq!(t.centroid_hae, None);
        assert_eq!(t.bbox_top_left_lat_offset, None);
        assert!(t.field_errors.is_empty(), "{:?}", t.field_errors);
    }
}

#[test]
fn a_special_after_a_value_unsets_it() {
    // The last occurrence decides; a special leaves no measurement.
    let bytes = [0x0B, 0x02, 0x2D, 0x00, 0x0B, 0x02, 0xE1, 0x00];
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.horizontal_fov, None);
    let pack = [0x01, 0x0C, 0x02, 0x10, 0x00, 0x0C, 0x02, 0xE0, 0x00];
    let ls = decode(&with_packs(&[&pack])).unwrap();
    assert_eq!(ls.targets[0].centroid_hae, None);
}

#[test]
fn the_reserved_special_pattern_is_still_refused() {
    let pack = [0x01, 0x0C, 0x02, 0xE7, 0x00];
    let bytes = with_packs(&[&pack]);
    assert_eq!(
        strict_pack_error(&bytes),
        VTargetPackError::MalformedImapb { tag: 12 }
    );
    let ls = decode(&bytes).unwrap();
    assert_eq!(ls.targets[0].field_errors.len(), 1);
    let mut top = minimal_strict_ls_bytes();
    top.extend_from_slice(&[0x0B, 0x02, 0xE7, 0x00]);
    assert!(decode_strict(&top).is_err());
    assert_eq!(decode(&top).unwrap().field_errors.len(), 1);
}

#[test]
fn vtarget_encode_refuses_a_target_id_of_2_pow_63() {
    // Table 10: V9 is 63 bits; the JVM `long` cannot carry more either.
    let with_id = |target_id: u64| VmtiLs {
        targets: vec![VTargetPack {
            target_id,
            centroid_pixel: Some(100),
            ..Default::default()
        }],
        ..full_standalone_ls()
    };
    let encoders: [Encoder; 4] = [
        encode_to_vec,
        encode_to_vec_standalone,
        encode_strict_compliance,
        encode_standalone_strict_compliance,
    ];
    for encode in encoders {
        for id in [1u64 << 63, u64::MAX] {
            let err = encode(&with_id(id)).unwrap_err();
            assert!(
                matches!(err, KlvEncodeError::OutOfRange { tag: 0, .. }),
                "{id}: {err:?}"
            );
        }
        encode(&with_id((1 << 63) - 1)).expect("2^63 - 1 is nine BER-OID bytes");
    }
}

#[test]
fn a_decoded_target_id_of_2_pow_63_is_refused_on_reencode_and_the_original_bytes_still_decode() {
    // The lenient decoder keeps a ten-byte targetId; no encoder writes it
    // back. A relay forwards the original bytes, or drops the target.
    let ten = [
        0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x01, 0x01, 0x05,
    ];
    let bytes = with_packs(&[&ten]);
    let mut ls = decode(&bytes).unwrap();
    assert_eq!(ls.targets[0].target_id, 1 << 63);
    assert!(matches!(
        encode_to_vec(&ls),
        Err(KlvEncodeError::OutOfRange { tag: 0, .. })
    ));
    assert_eq!(decode(&bytes).unwrap(), ls);
    ls.targets.clear();
    encode_to_vec(&ls).expect("without the target the record re-encodes");
}

// ------------------------------------------------------------------
// The relational rules, in both strict encoders and `decode_strict`.
// ------------------------------------------------------------------

/// `full_standalone_ls` with one pack built by `f`.
fn with_target(f: impl FnOnce(&mut VTargetPack)) -> VmtiLs {
    let mut pack = VTargetPack {
        target_id: 1,
        ..Default::default()
    };
    f(&mut pack);
    VmtiLs {
        targets: vec![pack],
        ..full_standalone_ls()
    }
}

/// Both strict encoders refuse `ls` with `InvalidFieldValue { tag }`, and
/// `decode_strict` refuses what the ordinary encoder writes of it with
/// `ComplianceViolation { tag }`.
fn assert_strict_refuses(ls: &VmtiLs, tag: u32, what: &str) {
    for encode in strict_encoders() {
        let err = encode(ls).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::InvalidFieldValue { tag: t, .. } if t == tag),
            "{what}: {err:?}"
        );
    }
    let err = decode_strict(&encode_to_vec(ls).unwrap()).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::ComplianceViolation { tag: t, .. } if t == tag),
        "{what}: {err:?}"
    );
}

/// Both strict encoders and `decode_strict` accept `ls`.
fn assert_strict_accepts(ls: &VmtiLs, what: &str) {
    for encode in strict_encoders() {
        encode(ls).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    }
    let decoded =
        decode_strict(&encode_to_vec(ls).unwrap()).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    assert_eq!(decoded.targets, ls.targets, "{what}");
}

#[test]
fn strict_vmti_refuses_a_pixel_number_without_a_frame_width() {
    // ST 0903.6-121.
    for tag in [1u32, 2, 3] {
        let mut ls = with_target(|t| match tag {
            1 => t.centroid_pixel = Some(100),
            2 => t.bbox_top_left_pixel = Some(100),
            _ => t.bbox_bottom_right_pixel = Some(100),
        });
        ls.frame_width = None;
        assert_strict_refuses(&ls, tag, "no frame width");
        ls.frame_width = Some(1920);
        assert_strict_accepts(&ls, "with a frame width");
    }
}

#[test]
fn strict_vmti_refuses_a_row_without_its_column() {
    // ST 0903.4-37 and -38.
    let row = with_target(|t| t.centroid_pix_row = Some(5));
    assert_strict_refuses(&row, 19, "a row alone");
    let col = with_target(|t| t.centroid_pix_col = Some(5));
    assert_strict_refuses(&col, 20, "a column alone");
    let both = with_target(|t| {
        t.centroid_pix_row = Some(5);
        t.centroid_pix_col = Some(6);
    });
    assert_strict_accepts(&both, "row and column");
    // The offset is the pack's, buffer-relative: the series value starts
    // at 8 and its first pack after one length byte.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[101, 5, 4, 0x01, 19, 1, 5]);
    assert!(matches!(
        decode_strict(&bytes),
        Err(KlvDecodeError::ComplianceViolation {
            tag: 19,
            offset: 9,
            ..
        })
    ));
}

#[test]
fn strict_vmti_refuses_an_active_target_without_a_position() {
    // ST 0903.6-127 (Active-Moving) and -128 (Active-Stopped).
    for status in [1u8, 3] {
        let bare = with_target(|t| {
            t.detection_status = Some(status);
            t.target_location = Some(vec![0x01]);
        });
        assert_strict_refuses(&bare, 23, "no position");
        let one_corner = with_target(|t| {
            t.detection_status = Some(status);
            t.target_location = Some(vec![0x01]);
            t.bbox_top_left_pixel = Some(10);
        });
        assert_strict_refuses(&one_corner, 23, "one corner");
        let forms: [fn(&mut VTargetPack); 3] = [
            |t| t.centroid_pixel = Some(10),
            |t| {
                t.centroid_pix_row = Some(2);
                t.centroid_pix_col = Some(3);
            },
            |t| {
                t.bbox_top_left_pixel = Some(10);
                t.bbox_bottom_right_pixel = Some(20);
            },
        ];
        for form in forms {
            let ls = with_target(|t| {
                t.detection_status = Some(status);
                t.target_location = Some(vec![0x01]);
                form(t);
            });
            assert_strict_accepts(&ls, "a position form");
        }
    }
    // Inactive (0), Dropped (2) and Active-Coasting (4) need no position.
    for status in [0u8, 2, 4] {
        let ls = with_target(|t| {
            t.detection_status = Some(status);
            t.target_location = Some(vec![0x01]);
        });
        assert_strict_accepts(&ls, "a status that needs no position");
    }
}

#[test]
fn strict_standalone_vmti_refuses_an_active_target_without_a_location() {
    // ST 0903.6-130: standalone carriage only.
    for status in [1u8, 3, 4] {
        let ls = with_target(|t| {
            t.detection_status = Some(status);
            t.centroid_pixel = Some(10);
        });
        let err = encode_standalone_strict_compliance(&ls).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::InvalidFieldValue { tag: 23, .. }),
            "status {status}: {err:?}"
        );
        encode_strict_compliance(&ls).expect("embedded carriage has no such rule");
        let empty = with_target(|t| {
            t.detection_status = Some(status);
            t.centroid_pixel = Some(10);
            t.target_location = Some(vec![]);
        });
        assert!(matches!(
            encode_standalone_strict_compliance(&empty),
            Err(KlvEncodeError::InvalidFieldValue { tag: 23, .. })
        ));
        let located = with_target(|t| {
            t.detection_status = Some(status);
            t.centroid_pixel = Some(10);
            t.target_location = Some(vec![0x01]);
        });
        encode_standalone_strict_compliance(&located).expect("with a location");
    }
}

#[test]
fn strict_standalone_vmti_refuses_a_core_identifier_st1204_does_not_accept() {
    for id in [vec![0x01, 0x00], vec![]] {
        let ls = VmtiLs {
            miis_id: Some(id.clone()),
            ..full_standalone_ls()
        };
        let err = encode_standalone_strict_compliance(&ls).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::InvalidFieldValue { tag: 13, .. }),
            "{id:02x?}: {err:?}"
        );
    }
    encode_standalone_strict_compliance(&full_standalone_ls()).expect("an ST 1204 identifier");
}

#[test]
fn strict_vmti_refuses_zero_where_the_range_starts_at_one() {
    // Version (§10.1.4), frame width and height (§10.1.8, .9: "do not use
    // a value of zero"), pixel numbers, row and column (§10.2.2.2 to .4,
    // .20, .21).
    type Set = fn(&mut VmtiLs);
    let cases: [(u32, Set); 8] = [
        (4, |ls| ls.version_number = Some(0)),
        (8, |ls| ls.frame_width = Some(0)),
        (9, |ls| ls.frame_height = Some(0)),
        (1, |ls| ls.targets[0].centroid_pixel = Some(0)),
        (2, |ls| ls.targets[0].bbox_top_left_pixel = Some(0)),
        (3, |ls| ls.targets[0].bbox_bottom_right_pixel = Some(0)),
        (19, |ls| {
            ls.targets[0].centroid_pix_row = Some(0);
            ls.targets[0].centroid_pix_col = Some(1);
        }),
        (20, |ls| {
            ls.targets[0].centroid_pix_row = Some(1);
            ls.targets[0].centroid_pix_col = Some(0);
        }),
    ];
    for (tag, set) in cases {
        let mut ls = full_standalone_ls();
        set(&mut ls);
        for encode in strict_encoders() {
            let err = encode(&ls).unwrap_err();
            assert!(
                matches!(err, KlvEncodeError::OutOfRange { tag: t, min, .. } if t == tag && min == 1.0),
                "tag {tag}: {err:?}"
            );
        }
        let err = decode_strict(&encode_to_vec(&ls).unwrap()).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::ComplianceViolation { tag: t, .. } if t == tag),
            "tag {tag}: {err:?}"
        );
    }
    // Priority and percentage (§10.2.2.5, .8) through the one-byte table.
    for (tag, set) in [
        (
            4u32,
            (|t: &mut VTargetPack| t.priority = Some(0)) as fn(&mut VTargetPack),
        ),
        (7, |t| t.percentage_of_target_pixels = Some(0)),
    ] {
        let ls = with_target(|t| {
            t.centroid_pixel = Some(10);
            set(t);
        });
        for encode in strict_encoders() {
            assert!(matches!(
                encode(&ls),
                Err(KlvEncodeError::OutOfRange { tag: t, min, .. }) if t == tag && min == 1.0
            ));
        }
        assert!(matches!(
            decode_strict(&encode_to_vec(&ls).unwrap()),
            Err(KlvDecodeError::St0903InvalidVTargetPack {
                reason: VTargetPackError::ValueOutOfRange { tag: t, value: 0 },
                ..
            }) if t == tag
        ));
    }
    // One is in range everywhere.
    let mut ones = full_standalone_ls();
    ones.version_number = Some(1);
    ones.frame_width = Some(1);
    ones.frame_height = Some(1);
    ones.targets[0] = VTargetPack {
        target_id: 1,
        centroid_pixel: Some(1),
        bbox_top_left_pixel: Some(1),
        bbox_bottom_right_pixel: Some(1),
        centroid_pix_row: Some(1),
        centroid_pix_col: Some(1),
        priority: Some(1),
        percentage_of_target_pixels: Some(1),
        ..Default::default()
    };
    assert_strict_accepts(&ones, "ones");
}

#[test]
fn strict_encode_refuses_a_repeated_unknown_tag() {
    // ST 0903.5-98: an item once per Local Set instance, in a VTarget pack
    // and at the top level alike; strict decode refuses the repeat.
    let twice = |tag| {
        vec![
            OwnedRawField {
                tag,
                value: vec![0xAA],
            },
            OwnedRawField {
                tag,
                value: vec![0xBB],
            },
        ]
    };
    let mut in_pack = full_standalone_ls();
    in_pack.targets[0].unknown = twice(127);
    let mut at_top = full_standalone_ls();
    at_top.unknown = twice(100);
    for (ls, tag) in [(&in_pack, 127u32), (&at_top, 100)] {
        for encode in strict_encoders() {
            let err = encode(ls).unwrap_err();
            assert!(
                matches!(err, KlvEncodeError::InvalidFieldValue { tag: t, .. } if t == tag),
                "tag {tag}: {err:?}"
            );
        }
        let body = encode_to_vec(ls).expect("the ordinary encoder still writes it");
        assert!(
            decode_strict(&body).is_err(),
            "tag {tag}: strict decode refuses it"
        );
    }
    // Two different unknown tags are fine.
    let mut two = full_standalone_ls();
    two.targets[0].unknown = vec![
        OwnedRawField {
            tag: 126,
            value: vec![0xAA],
        },
        OwnedRawField {
            tag: 127,
            value: vec![0xBB],
        },
    ];
    for encode in strict_encoders() {
        encode(&two).expect("distinct unknown tags");
    }
}

#[test]
fn vmti_encoders_name_the_item_of_an_out_of_range_imapb_value() {
    // The shared IMAPB encoder knows no tag; the VMTI emitter names the
    // item, so tag 0 means only the targetId.
    type Set = fn(&mut VmtiLs);
    let cases: [(u32, Set); 9] = [
        (11, |ls| ls.horizontal_fov = Some(181.0)),
        (12, |ls| ls.vertical_fov = Some(f64::NAN)),
        (10, |ls| ls.targets[0].centroid_lat_offset = Some(20.0)),
        (11, |ls| ls.targets[0].centroid_lon_offset = Some(-20.0)),
        (12, |ls| ls.targets[0].centroid_hae = Some(19_001.0)),
        (13, |ls| {
            ls.targets[0].bbox_top_left_lat_offset = Some(f64::INFINITY)
        }),
        (14, |ls| ls.targets[0].bbox_top_left_lon_offset = Some(20.0)),
        (15, |ls| {
            ls.targets[0].bbox_bottom_right_lat_offset = Some(20.0)
        }),
        (16, |ls| {
            ls.targets[0].bbox_bottom_right_lon_offset = Some(20.0)
        }),
    ];
    for (tag, set) in cases {
        let mut ls = full_standalone_ls();
        set(&mut ls);
        let err = encode_to_vec(&ls).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::OutOfRange { tag: t, .. } if t == tag),
            "tag {tag}: {err:?}"
        );
    }
}
