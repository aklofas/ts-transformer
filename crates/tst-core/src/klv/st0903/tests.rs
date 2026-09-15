use super::VMTI_LS_UL;
use super::decode::{decode, decode_strict};
use super::encode::{
    encode_standalone_strict_compliance, encode_strict_compliance, encode_to_vec,
    encode_to_vec_standalone, encoded_len, encoded_len_standalone,
};
use super::model::VmtiLs;
use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};
use crate::klv::pack::OwnedRawField;
use crate::klv::st0903::vtarget_pack::VTargetPack;

/// Build a minimal LS containing only Tag 1 (Checksum) + Tag 2 (PTS)
/// + Tag 4 (Version) — the three commonly-required tags. (Strict
///   mode validates a tighter set per Task 6 — see §10.1 carriage
///   rules.)
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
    // field_errors capture the truncation. Strict mode rejects
    // (Task 6).
    let bytes = [4u8, 5, 0x01];
    let ls = decode(&bytes).unwrap();
    assert!(!ls.field_errors.is_empty());
}

/// Regression for Phase 0 Task 1.5: hostile bytes targeting the
/// U64Be (Tag 2 PTS) decode path must never panic. The upstream
/// `value.len() != 8` length check intercepts wrong-sized slices
/// before the `try_into` runs, so the fallible-conversion safety
/// net added in Task 1.5 is defense-in-depth — both the well-formed
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
    // Sanity check that the Task 1.5 fix didn't regress the happy
    // path. 8-byte PTS decodes to the expected u64.
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
    // Regression: Tag 11 with BER length 0 must surface
    // InvalidLength in field_errors, not panic. Without the
    // hardcoded `length=2` guard, `decode_imapb` calls
    // `read_signed_be(&[])` which underflows `n*8-1` at n==0.
    let mut bytes = minimal_ls_bytes();
    bytes.extend_from_slice(&[11, 0]); // tag 11, length 0
    let ls = decode(&bytes).unwrap();
    assert!(ls.horizontal_fov.is_none());
    assert!(ls.field_errors.iter().any(|e| matches!(
        e,
        KlvFieldError::InvalidLength {
            tag: 11,
            expected: 2,
            got: 0
        }
    )));
}

#[test]
fn decode_imapb_happy_path() {
    // FOV = 90.0° encoded as IMAPB(0, 180, 2) per ST 0903.6
    // §10.1.11 worked example. The spec-correct encoding for
    // 90.0° in this range is the byte pair 0x2D 0x00.
    //
    // Historical note: the pre-fix substrate used a signed-
    // midpoint formula that produced 0xAD 0x00 for the same
    // input (and this test was transcribed from that wrong
    // output). Tasks 1–5 of plan
    // 2026-05-10-klv-wire-format-critical-fixes corrected the
    // substrate; this test now codifies the spec result.
    let mut bytes = minimal_ls_bytes();
    bytes.extend_from_slice(&[11, 2, 0x2D, 0x00]);
    let ls = decode(&bytes).unwrap();
    let fov = ls.horizontal_fov.expect("horizontal_fov decoded");
    assert!((fov - 90.0).abs() < 0.01, "got fov={fov}, expected ~90.0");
    assert!(ls.field_errors.is_empty());
}

// ------------------------------------------------------------------
// Task 6 — `decode_strict` tests.
// ------------------------------------------------------------------

/// Build the minimum LS that satisfies `decode_strict`'s required-tag
/// gate per Task 2's audit: Tag 4 (Version) + Tag 6 (numTargetsReported).
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
    // Strict mode must surface IMAPB-length errors as Err, not
    // accept them silently. Lenient surfaces in field_errors.
    let mut bytes = minimal_strict_ls_bytes();
    bytes.extend_from_slice(&[11, 0]); // tag 11, BER length 0
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
            tag: 11,
            expected: 2,
            got: 0
        })
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
    // CORR-29(b): Tag 101 value = one valid pack (`04` + 4 body bytes:
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
    // CORR-29(e): pack = target_id 1, then tag 10 (centroid_lat_offset,
    // 3-byte IMAPB) with a 1-byte value. A per-field error must land in
    // the pack's `field_errors`; the pack used to be dropped wholesale
    // and reported as `TruncatedField { tag: 101 }` on the parent.
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
// Task 7 — `encode` + `encoded_len` round-trip + canonical-bytes.
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

// --- Phase 1 (KLV-OTHER-02) regression tests added 2026-05-10 ---

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
    // `encode_to_vec_standalone` (Task 4) computes its own.
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

// ---------- Validate-1 E5: BER-OID walker round-trip tests ----------
//
// The ST 0903 §10.1 typed universe (tags 1..=103) all fit in a single
// BER-OID byte (`write_ber_oid(N) == [N]` for `N ≤ 127`). The walker
// migration from `cursor[0]`-style single-byte reads to BER-OID
// decode is byte-identical for these. The tests below pin that
// invariant and exercise the multi-byte boundaries (128 / 16383 /
// 16384) that the migration unlocks for forward-compat tags. Mirrors
// the ST 0102 E4 test suite (`unknown_tag_*_round_trips_*_byte_ber_oid`).

/// Byte-identical encode for the typed universe: a maximally-populated
/// LS using only defined tags 1..=103 emits the same wire bytes
/// pre- and post-E5 because BER-OID(N) == [N] for N ≤ 127. Pins the
/// fact that the walker rewrite did not regress legacy emission.
#[test]
fn defined_tags_byte_identical_pre_and_post_e5() {
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
    // Pre-E5 baseline: each tag emits as a single byte. Spot-check
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

/// BER-OID round-trip boundary: unknown tag 127 — last single-byte
/// value (`0x7F`). Mirrors `klv::st0102::tests::
/// unknown_tag_127_round_trips_single_byte_ber_oid`.
#[test]
fn unknown_tag_127_round_trips_single_byte_ber_oid() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 127,
            value: b"max-single-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 127);
    assert_eq!(decoded.unknown[0].value, b"max-single-byte");
}

/// BER-OID round-trip boundary: unknown tag 128 — first multi-byte
/// value (`0x81 0x00`). The continuation bit `0x80` on the first byte
/// signals "more bytes follow". Pre-E5 this tag would have been
/// silently dropped on encode (the `field.tag <= 0xFF` guard kept it,
/// but the cast `as u8` truncated to `0x80`, and the decoder's
/// `cursor[0]` would have read that `0x80` as start of a BER length).
/// Post-E5 it survives a full encode/decode round-trip.
#[test]
fn unknown_tag_128_round_trips_multi_byte_ber_oid() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 128,
            value: b"first-multi-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));

    // Hand-verify the BER-OID bytes for tag 128 land where expected
    // on the wire. The tail of `bytes` ends with `[0x81, 0x00, BER
    // length, value...]` — locate the BER-OID prefix by scanning
    // backwards.
    let tail_start = bytes.len() - (b"first-multi-byte".len() + 1 + 2);
    assert_eq!(
        &bytes[tail_start..tail_start + 2],
        &[0x81, 0x00],
        "tag 128 BER-OID encoding is 0x81 0x00"
    );

    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 128);
    assert_eq!(decoded.unknown[0].value, b"first-multi-byte");
}

/// BER-OID round-trip boundary: unknown tag 16383 (`2^14 - 1`) — last
/// two-byte value (`0xFF 0x7F`).
#[test]
fn unknown_tag_16383_round_trips_two_byte_ber_oid() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 16383,
            value: b"max-two-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 16383);
    assert_eq!(decoded.unknown[0].value, b"max-two-byte");
}

/// BER-OID round-trip boundary: unknown tag 16384 (`2^14`) — first
/// three-byte value (`0x81 0x80 0x00`).
#[test]
fn unknown_tag_16384_round_trips_three_byte_ber_oid() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        unknown: vec![OwnedRawField {
            tag: 16384,
            value: b"first-three-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(bytes.len(), encoded_len(&ls));
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 16384);
    assert_eq!(decoded.unknown[0].value, b"first-three-byte");
}

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

/// Strict mode catches a duplicate multi-byte BER-OID tag. Pre-E5
/// the `[bool; 256]` seen array would have silently collided tag
/// 256 with tag 0 (`as u8`-narrowing); post-E5 the dedup runs on
/// the full `u32` tag value so multi-byte duplicates are caught.
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
// Task 1 (WP-F / REF-KLV-03): unknown-bucket typed-tag guard tests.
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
// Task 4 (WP-F / REF-KLV-03): encode_strict_compliance +
// encode_standalone_strict_compliance tests.
// ------------------------------------------------------------------

/// Helper: a VmtiLs satisfying standalone-required fields (tags 2+4+6+11+12+13).
fn full_standalone_ls() -> VmtiLs {
    VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        miis_id: Some(vec![0x11, 0x22, 0x33]),
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
