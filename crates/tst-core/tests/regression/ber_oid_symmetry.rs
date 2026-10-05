//! BER + BER-OID encode/decode symmetry across all three typed KLV sets
//! (ST 0601, ST 0102, ST 0903). Covered behaviour:
//!
//!   - ST 0601 strict-mode duplicate-tag + canonical BER
//!   - ST 0601 encode reserved-tag filter
//!   - ST 0102 BER-OID encode
//!   - ST 0903 BER-OID walker + VTargetPack inner walk
//!
//! Spec references:
//!   ST 0107.5 §6.3.1  BER-OID canonical encoding ("0x80 forbidden as first byte")
//!   ST 0107.5 §6.3.2  BER length canonical encoding ("fewest bytes")
//!   ST 0601.13 §6.2 + ST 0601.24 §6  reserved structural tags 1, 2, 65
//!   ST 0903.6  §10.1  typed VMTI tag table
//!   ST 0102.12 §6.7   Security Metadata LS required-tag table

use tst_core::error::{KlvDecodeError, KlvEncodeError};
use tst_core::klv::length::{
    ber_oid_len, read_ber, read_ber_oid, read_ber_oid_strict, read_ber_strict, write_ber,
    write_ber_oid,
};
use tst_core::klv::st0102::{
    self, ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
    SecurityLs,
};
use tst_core::klv::st0601::{self, UasDatalinkLs};
use tst_core::klv::st0903::{self, VmtiLs};
use tst_core::klv::{OwnedRawField, UniversalLabel};

// ============================================================================
// Substrate-level BER + BER-OID round-trip and canonical-form checks
// ============================================================================
//
// These vector-tests reproduce ST 0107.5 §6.3.1 + §6.3.2 examples
// against the substrate's strict and permissive readers. The intent
// here is "what bytes does the spec mandate / forbid" rather than
// "does the round-trip close" — the substrate already proptests
// round-trip in `crates/tst-core/src/klv/length.rs::tests`.

/// ST 0107.5 §6.3.2 example: value 16 has TWO valid byte sequences:
/// canonical short form `[0x10]` (1 byte) and non-canonical long
/// form `[0x81 0x10]` (2 bytes). The strict reader rejects the
/// long form per §6.3.2 "fewest bytes"; the permissive reader
/// accepts both for legacy capture interop.
#[test]
fn ber_strict_rejects_non_canonical_long_for_value_16_st0107_5_6_3_2() {
    // Canonical short form — accepted by both.
    let short = [0x10];
    assert_eq!(read_ber_strict(&short).unwrap().0, 16);
    assert_eq!(read_ber(&short).unwrap().0, 16);
    // Non-canonical long form — accepted by permissive, rejected by strict.
    let long_non_canonical = [0x81, 0x10];
    assert_eq!(read_ber(&long_non_canonical).unwrap().0, 16);
    let err = read_ber_strict(&long_non_canonical).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalLength { .. }),
        "expected NonCanonicalLength, got {err:?}"
    );
}

/// ST 0107.5 §6.3.2 leading-zero variant: `[0x82, 0x00, 0x10]` is
/// overlong long-form for value 16; strict reader rejects.
#[test]
fn ber_strict_rejects_leading_zero_long_form_st0107_5_6_3_2() {
    let buf = [0x82, 0x00, 0x10];
    assert_eq!(read_ber(&buf).unwrap().0, 16);
    let err = read_ber_strict(&buf).unwrap_err();
    assert!(matches!(err, KlvDecodeError::NonCanonicalLength { .. }));
}

/// ST 0107.5 §6.3.1: BER-OID value 0 has TWO representations:
/// canonical `[0x00]` (1 byte) and non-canonical `[0x80, 0x00]`
/// (continuation-byte for zero). §6.3.1 forbids leading `0x80`;
/// strict reader rejects, permissive accepts.
#[test]
fn ber_oid_strict_rejects_leading_0x80_st0107_5_6_3_1() {
    let canonical = [0x00];
    assert_eq!(read_ber_oid_strict(&canonical).unwrap().0, 0);

    let non_canonical = [0x80, 0x00];
    assert_eq!(read_ber_oid(&non_canonical).unwrap().0, 0);
    let err = read_ber_oid_strict(&non_canonical).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalTag { .. }),
        "expected NonCanonicalTag, got {err:?}"
    );
}

/// Encoder MUST emit canonical (shortest) BER-OID for the tag space.
/// Sweep over key boundary values: 0, 0x7F (single-byte max),
/// 0x80 (two-byte min), 0x3FFF (two-byte max), 0x4000 (three-byte min).
#[test]
fn ber_oid_encoder_emits_canonical_shortest_form_st0107_5_6_3_1() {
    let pairs: &[(u32, &[u8])] = &[
        (0, &[0x00]),
        (0x7F, &[0x7F]),
        (0x80, &[0x81, 0x00]),
        (0x3FFF, &[0xFF, 0x7F]),
        (0x4000, &[0x81, 0x80, 0x00]),
    ];
    for &(value, expected) in pairs {
        let mut buf = [0u8; 8];
        let n = write_ber_oid(value, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            expected,
            "value {value:#x}: expected {expected:02X?}, got {:02X?}",
            &buf[..n]
        );
        // ber_oid_len must predict the encode width exactly.
        assert_eq!(ber_oid_len(value), n);
    }
}

/// Encoder MUST emit canonical (shortest) BER for the length space.
/// Sweep boundary values: 0, 0x7F (short-form max), 0x80 (long-form
/// min), 0xFFFF (2-byte long max), 0x10000 (3-byte long min).
#[test]
fn ber_encoder_emits_canonical_shortest_form_st0107_5_6_3_2() {
    let pairs: &[(usize, &[u8])] = &[
        (0, &[0x00]),
        (0x7F, &[0x7F]),
        (0x80, &[0x81, 0x80]),
        (0xFFFF, &[0x82, 0xFF, 0xFF]),
        (0x10000, &[0x83, 0x01, 0x00, 0x00]),
    ];
    for &(value, expected) in pairs {
        let mut buf = [0u8; 16];
        let n = write_ber(value, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            expected,
            "value {value:#x}: expected {expected:02X?}, got {:02X?}",
            &buf[..n]
        );
    }
}

// ============================================================================
// ST 0601 — round-trip + reserved-tag filter + duplicate-tag
// ============================================================================

/// ST 0601 strict-compliance decode rejects duplicate occurrences
/// of typed tags per ST 0601.13-24 ("once per packet" for defined items).
/// Hand-build a record with Tag 5 (`platform_heading_deg`, IMAPB(0, 360, 2))
/// appearing twice in the body and confirm `decode_strict_compliance`
/// returns `DuplicateTag { tag: 5, .. }`.
#[test]
fn st0601_strict_compliance_rejects_duplicate_typed_tag_st0601_13_24() {
    // Build a normal record first, then surgically duplicate Tag 5.
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_heading_deg: Some(125.5),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let mut bytes = st0601::encode_to_vec(&record).unwrap();

    // Locate Tag 5 in the body (encode is in tag order so 5 follows 2).
    // The wire pattern for Tag 5 with len=2 is `0x05 0x02 hi lo`. Insert
    // a duplicate immediately after.
    let tag5_off = bytes
        .windows(2)
        .position(|w| w[0] == 0x05 && w[1] == 0x02)
        .expect("Tag 5 must be present");
    let tag5_tlv: [u8; 4] = bytes[tag5_off..tag5_off + 4].try_into().unwrap();
    // Insert a clone of the Tag 5 TLV right after the original.
    bytes.splice(tag5_off + 4..tag5_off + 4, tag5_tlv.iter().copied());
    // Recompute outer BER length so the modified body is still framed.
    // For this fixture we rebuild via the strict-walker directly on
    // body bytes — `decode_strict_compliance` enforces structural BER
    // before duplicate detection so we need a self-consistent record.
    // Re-write the outer length (byte 16 onward) per `ber_len`.
    let body_len = body_len_after_outer(&bytes);
    let new_body_len = body_len + 4; // we added 4 bytes
    rewrite_outer_ber_length(&mut bytes, new_body_len);
    // Recompute the running-sum checksum to keep `decode_strict_compliance`'s
    // checksum gate happy (it would otherwise fail with ChecksumMismatch
    // BEFORE reaching the duplicate-tag check — masking the real intent
    // of this test).
    recompute_st0601_checksum(&mut bytes);

    let err = st0601::decode_strict_compliance(&bytes).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 5, .. }),
        "expected DuplicateTag {{ tag: 5 }}, got {err:?}"
    );
    // Lenient decode does NOT enforce uniqueness — confirm it still
    // succeeds (second Tag 5 just overwrites the first).
    let _ok = st0601::decode(&bytes).expect("lenient decode tolerates duplicates");
}

/// Helper: read the outer BER length declared at byte 16 and return
/// the **declared body length** (not including the BER length bytes
/// themselves). Used by [`st0601_strict_compliance_rejects_duplicate_typed_tag_st0601_13_24`].
fn body_len_after_outer(buf: &[u8]) -> usize {
    let (len, _) = read_ber(&buf[16..]).unwrap();
    len
}

/// Helper: overwrite the outer BER length in-place. Caller must ensure
/// the new length encodes in the same number of bytes as the old (this
/// is true for the duplicate-Tag-5 fixture above where the body grows
/// by only 4 bytes).
fn rewrite_outer_ber_length(buf: &mut [u8], new_body_len: usize) {
    use tst_core::klv::length::ber_len;
    let old_bytes_used = ber_len(body_len_after_outer(buf));
    let new_bytes_used = ber_len(new_body_len);
    assert_eq!(
        old_bytes_used, new_bytes_used,
        "test helper requires same-width BER length re-encode; old={old_bytes_used} new={new_bytes_used}"
    );
    write_ber(new_body_len, &mut buf[16..16 + new_bytes_used]).unwrap();
}

/// Helper: recompute the running-sum 16 checksum and overwrite Tag 1's
/// value at the tail of the buffer. Mirrors `st0601::encode`'s checksum
/// emission step (`crates/tst-core/src/klv/st0601/encode.rs` lines
/// 65-72).
fn recompute_st0601_checksum(buf: &mut [u8]) {
    use tst_core::klv::checksum::checksum_running_sum_16;
    // Tag 1 TLV is the trailing 4 bytes: [0x01 0x02 hi lo]. Re-checksum
    // [0 .. value_offset].
    let n = buf.len();
    assert_eq!(buf[n - 4], 0x01, "trailing TLV must be Tag 1");
    assert_eq!(buf[n - 3], 0x02, "Tag 1 length must be 2");
    let value_offset = n - 2;
    let cksum = checksum_running_sum_16(&buf[..value_offset]);
    buf[value_offset] = (cksum >> 8) as u8;
    buf[value_offset + 1] = cksum as u8;
}

/// The encoder rejects a structural / typed tag in `record.unknown`
/// with [`KlvEncodeError::ReservedTagInUnknown`]. The `unknown` vec
/// is for forward-compat pass-through ONLY — any tag in the typed
/// table (or the 3 reserved structural tags 1/2/65) must come through
/// its dedicated struct field instead.
#[test]
fn st0601_encode_rejects_reserved_tag_in_unknown_e3_filter() {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        // Tag 5 is in the typed table.
        unknown: vec![OwnedRawField {
            tag: 5,
            value: vec![0x00, 0x00],
        }],
        ..Default::default()
    };
    let err = st0601::encode_to_vec(&record).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: 5 }),
        "expected ReservedTagInUnknown {{ tag: 5 }}, got {err:?}"
    );
}

/// The same filter blocks the reserved structural tags 1 (Checksum),
/// 2 (PTS), and 65 (UAS LS Version). Sweep all three.
#[test]
fn st0601_encode_rejects_reserved_structural_tags_in_unknown_e3_filter() {
    for reserved_tag in [1u32, 2, 65] {
        let record = UasDatalinkLs {
            timestamp_us: Some(0),
            unknown: vec![OwnedRawField {
                tag: reserved_tag,
                value: vec![0xAB; 4],
            }],
            ..Default::default()
        };
        let err = st0601::encode_to_vec(&record).unwrap_err();
        assert!(
            matches!(
                err,
                KlvEncodeError::ReservedTagInUnknown { tag } if tag == reserved_tag
            ),
            "tag {reserved_tag}: expected ReservedTagInUnknown, got {err:?}"
        );
    }
}

/// Cross-check at ST 0601: a multi-byte BER-OID tag (e.g.
/// 200, which encodes as `[0x81, 0x48]`) put in `record.unknown`
/// should round-trip through encode→decode losslessly. ST 0601's
/// `is_reserved_or_typed_tag` returns `false` for tag > 0xFF (the
/// typed table is u8-keyed), so the filter doesn't block it.
#[test]
fn st0601_unknown_multibyte_ber_oid_tag_round_trips_e5_cross_check() {
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        unknown: vec![OwnedRawField {
            tag: 200, // outside typed table (u8 cap 91) but inside u8 universe
            value: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }],
        ..Default::default()
    };
    let bytes = st0601::encode_to_vec(&record).unwrap();
    let decoded = st0601::decode(&bytes).unwrap();
    let preserved = decoded
        .unknown
        .iter()
        .find(|f| f.tag == 200)
        .expect("tag 200 round-trips through unknown");
    assert_eq!(preserved.value, &[0xDE, 0xAD, 0xBE, 0xEF]);
}

/// ST 0601 strict_compliance rejects a non-canonical BER length in
/// the BODY (not just the outer). Hand-build a Tag 5 TLV with
/// `[0x05 0x81 0x02 hi lo]` (length 2 encoded as long-form 0x81 0x02
/// instead of canonical 0x02). Strict-compliance trips
/// `NonCanonicalLength`. Demonstrates the body-level strict reader
/// integration.
#[test]
fn st0601_strict_compliance_rejects_non_canonical_length_in_body_st0107_5_6_3_2() {
    // Build via encode first then surgically mutate Tag 5's length.
    let record = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_heading_deg: Some(0.0),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let mut bytes = st0601::encode_to_vec(&record).unwrap();
    // Locate Tag 5 TLV. Pattern `[0x05 0x02 hi lo]` → splice in
    // `[0x05 0x81 0x02 hi lo]` (one extra byte for the long-form
    // length prefix). Pre-compute helper offsets carefully.
    let tag5_off = bytes
        .windows(2)
        .position(|w| w[0] == 0x05 && w[1] == 0x02)
        .expect("Tag 5 present");
    // Splice in one extra byte: change `0x02` to `0x81 0x02`.
    bytes.splice(tag5_off + 1..tag5_off + 2, [0x81, 0x02]);
    // Outer length grew by 1.
    let body_len = body_len_after_outer(&bytes);
    rewrite_outer_ber_length(&mut bytes, body_len + 1);
    recompute_st0601_checksum(&mut bytes);

    let err = st0601::decode_strict_compliance(&bytes).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalLength { .. }),
        "expected NonCanonicalLength, got {err:?}"
    );
}

// ============================================================================
// ST 0102 — BER-OID encode + round-trip
// ============================================================================

/// The ST 0102 encoder MUST round-trip a multi-byte BER-OID `unknown`
/// tag (tag ≥ 128); a hard-coded single-byte tag write would silently
/// truncate tag > 127. Construct a SecurityLs with required tags + an
/// unknown tag at id 200 and confirm round-trip preserves it.
#[test]
fn st0102_encode_round_trips_multibyte_ber_oid_unknown_tag_e4_fix() {
    let mut security = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };
    // Forward-compat tag at id 200 (multi-byte BER-OID: [0x81, 0x48]).
    security.unknown.push(OwnedRawField {
        tag: 200,
        value: vec![0xCA, 0xFE, 0xBA, 0xBE],
    });
    let bytes = st0102::encode_to_vec(&security).unwrap();
    let decoded = st0102::decode(&bytes).unwrap();
    let preserved = decoded
        .unknown
        .iter()
        .find(|f| f.tag == 200)
        .expect("tag 200 round-trips");
    assert_eq!(preserved.value, &[0xCA, 0xFE, 0xBA, 0xBE]);
    // And the typed fields survive.
    assert_eq!(decoded.version, Some(12));
}

/// ST 0102 round-trip across all defined tags. Single fixture
/// exercising every typed field — verifies decode + encode are
/// bit-identical for spec-conformant input.
#[test]
fn st0102_round_trip_all_typed_fields_st0102_12() {
    let security = SecurityLs {
        security_classification: Some(SecurityClassification::Secret),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        sci_shi_info: Some("HCS-O".to_string()),
        caveats: Some("FOUO".to_string()),
        releasing_instructions: Some("USA CAN GBR".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let bytes = st0102::encode_to_vec(&security).unwrap();
    let decoded = st0102::decode_strict(&bytes).unwrap();
    assert_eq!(decoded, security);
}

/// ST 0102 strict decode rejects duplicate occurrences of typed tags.
#[test]
fn st0102_strict_rejects_duplicate_typed_tag() {
    let security = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let mut bytes = st0102::encode_to_vec(&security).unwrap();
    // Append a duplicate Tag 1 TLV at the end. `[0x01, 0x01, 0x00]`
    // = Tag 1 (Security Classification), len 1, value 0 (Unclassified).
    bytes.extend_from_slice(&[0x01, 0x01, 0x00]);

    let err = st0102::decode_strict(&bytes).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 1, .. }),
        "expected DuplicateTag {{ tag: 1 }}, got {err:?}"
    );
    // Lenient decode tolerates duplicates (the later TLV overwrites).
    let _ok = st0102::decode(&bytes).expect("lenient tolerates duplicates");
}

// ============================================================================
// ST 0903 — BER-OID walker symmetry on top-level and pack-level
// ============================================================================

/// The top-level VMTI walker rejects a non-canonical BER-OID tag
/// (leading 0x80) per ST 0107.5 §6.3.1. Confirms the top-level
/// `decode_strict` is routed through `read_ber_oid_strict`.
#[test]
fn st0903_strict_walker_rejects_non_canonical_ber_oid_tag_st0107_5_6_3_1() {
    // Hand-build a body with leading 0x80 BER-OID encoding of tag 0.
    // `[0x80 0x00 0x01 0x00]` = non-canonical "tag 0" + len 1 + value 0.
    // The strict walker SHOULD reject before even attempting decode.
    let body = [0x80, 0x00, 0x01, 0x00];
    let err = st0903::decode_strict(&body).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalTag { .. }),
        "expected NonCanonicalTag, got {err:?}"
    );
}

/// The top-level VMTI walker rejects a non-canonical BER length
/// per ST 0107.5 §6.3.2.
#[test]
fn st0903_strict_walker_rejects_non_canonical_ber_length_st0107_5_6_3_2() {
    // `[0x02 0x81 0x08 ...]` = tag 2 (PTS) + long-form len 8 (non-
    // canonical for value 8). The strict walker returns
    // NonCanonicalLength.
    let mut body = vec![0x02, 0x81, 0x08];
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    let err = st0903::decode_strict(&body).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::NonCanonicalLength { .. }),
        "expected NonCanonicalLength, got {err:?}"
    );
}

/// The VMTI walker preserves a multi-byte BER-OID unknown tag in the
/// top-level `unknown` field. Same forward-compat invariant as ST 0102 / ST 0601.
#[test]
fn st0903_round_trips_multibyte_ber_oid_unknown_tag_e5() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(0),
        // Tag 200 — multi-byte BER-OID; a single-byte tag write would
        // silently mis-encode it.
        unknown: vec![OwnedRawField {
            tag: 200,
            value: vec![0x11, 0x22, 0x33],
        }],
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    let decoded = st0903::decode(&bytes).unwrap();
    let preserved = decoded
        .unknown
        .iter()
        .find(|f| f.tag == 200)
        .expect("tag 200 round-trips");
    assert_eq!(preserved.value, &[0x11, 0x22, 0x33]);
}

/// VTargetPack inner walker preserves
/// multi-byte BER-OID unknown tags on each per-target pack.
#[test]
fn st0903_vtarget_pack_round_trips_multibyte_ber_oid_unknown_tag_e5_followup() {
    let pack = st0903::VTargetPack {
        target_id: 42,
        priority: Some(1),
        confidence_level: Some(80),
        // Tag 200 inside the pack — exercises the pack-level walker.
        unknown: vec![OwnedRawField {
            tag: 200,
            value: vec![0xDE, 0xAD],
        }],
        ..Default::default()
    };
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        num_targets_reported: Some(1),
        targets: vec![pack.clone()],
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    let decoded = st0903::decode(&bytes).unwrap();
    let preserved = decoded.targets[0]
        .unknown
        .iter()
        .find(|f| f.tag == 200)
        .expect("pack-level tag 200 round-trips");
    assert_eq!(preserved.value, &[0xDE, 0xAD]);
}

/// VMTI walker symmetry — decode → re-encode → decode produces
/// byte-identical wire output (modulo IMAPB quantization, which a
/// pure-integer fixture sidesteps). The walker preserves
/// field order so the inner bytes round-trip stably.
#[test]
fn st0903_decode_reencode_byte_identical_e5_walker_symmetry() {
    let ls = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        vmti_system_name: Some("SystemA".to_string()),
        version_number: Some(6),
        total_targets_in_frame: Some(3),
        num_targets_reported: Some(2),
        frame_width: Some(1920),
        frame_height: Some(1080),
        ..Default::default()
    };
    let bytes1 = st0903::encode_to_vec(&ls).unwrap();
    let decoded = st0903::decode(&bytes1).unwrap();
    let bytes2 = st0903::encode_to_vec(&decoded).unwrap();
    assert_eq!(
        bytes1, bytes2,
        "decode→encode must be byte-identical for integer-only fixtures"
    );
}

// ============================================================================
// Cross-set composition (ST 0102 inside ST 0601 Tag 48,
//               ST 0903 inside ST 0601 Tag 74)
// ============================================================================

/// Cross-set: ST 0102 SecurityLs nested inside ST 0601 Tag 48. Mirrors
/// `tests/klv_st0102_via_st0601.rs` but explicitly checks the outer
/// walker descends correctly across both BER-OID layers.
#[test]
fn cross_st0601_tag_48_carries_st0102_security_ls() {
    let security = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let security_bytes = st0102::encode_to_vec(&security).unwrap();

    let parent = UasDatalinkLs {
        universal_label: UniversalLabel::default(),
        declared_version: 19,
        timestamp_us: Some(1_700_000_000_000_000),
        security_local_set: Some(security_bytes.clone()),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let parent_bytes = st0601::encode_to_vec(&parent).unwrap();
    let decoded_parent = st0601::decode(&parent_bytes).unwrap();
    let inner = decoded_parent
        .security_local_set
        .as_deref()
        .expect("Tag 48 present");
    // Sibling-layer decode of the inner Security LS.
    let decoded_security = st0102::decode(inner).unwrap();
    assert_eq!(decoded_security, security);
}

/// Cross-set: empty SecurityLs bytes inside ST 0601 Tag 48. The outer
/// walker should NOT misinterpret an empty Tag 48 as "absent." This is
/// pathological input (an empty Security LS is non-conformant per
/// ST 0102.12 §6.7 minimum requirements), but the outer walker must
/// preserve the empty bytes verbatim.
#[test]
fn cross_st0601_tag_48_empty_security_bytes_preserved() {
    let parent = UasDatalinkLs {
        timestamp_us: Some(0),
        security_local_set: Some(Vec::new()),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let bytes = st0601::encode_to_vec(&parent).unwrap();
    let decoded = st0601::decode(&bytes).unwrap();
    assert_eq!(
        decoded.security_local_set.as_deref(),
        Some(&[][..]),
        "empty Tag 48 round-trips as Some(empty), not None"
    );
}

/// Cross-set: ST 0903 VmtiLs nested inside ST 0601 Tag 74. Same
/// pattern as Tag 48 / ST 0102. ST 0903.6-120 mandates omitted Tag 1
/// for embedded-VMTI; `encode` honors this without prompting.
#[test]
fn cross_st0601_tag_74_carries_st0903_vmti_ls() {
    let vmti = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        horizontal_fov: Some(12.5),
        vertical_fov: Some(10.0),
        targets: vec![st0903::VTargetPack {
            target_id: 7,
            priority: Some(1),
            confidence_level: Some(90),
            ..Default::default()
        }],
        ..Default::default()
    };
    let vmti_bytes = st0903::encode_to_vec(&vmti).unwrap();

    let parent = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        vmti: Some(vmti_bytes.clone()),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let parent_bytes = st0601::encode_to_vec(&parent).unwrap();
    let decoded_parent = st0601::decode(&parent_bytes).unwrap();
    let inner = decoded_parent.vmti.as_deref().expect("Tag 74 present");
    assert_eq!(inner, vmti_bytes.as_slice());
    let decoded_vmti = st0903::decode(inner).unwrap();
    assert!(decoded_vmti.field_errors.is_empty());
    assert_eq!(decoded_vmti.targets.len(), 1);
    assert_eq!(decoded_vmti.targets[0].target_id, 7);
}

/// Cross-set: truncated Security LS inside ST 0601 Tag 48 — the outer
/// ST 0601 walker should preserve the truncated bytes verbatim (Tag 48
/// is `RawBytes` pass-through; the outer walker doesn't validate the
/// inner structure). When the consumer then runs `st0102::decode` on
/// the truncated bytes, the inner error is localized to that layer
/// without poisoning the outer record. Demonstrates the
/// sibling-layer error-isolation property.
#[test]
fn cross_st0601_tag_48_malformed_security_inner_errors_dont_break_outer() {
    // Build valid SecurityLs bytes, then truncate to mid-TLV.
    let security = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let mut sec_bytes = st0102::encode_to_vec(&security).unwrap();
    sec_bytes.truncate(sec_bytes.len() / 2); // truncate mid-TLV

    let parent = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        security_local_set: Some(sec_bytes.clone()),
        uas_ls_version: Some(19),
        ..Default::default()
    };
    let parent_bytes = st0601::encode_to_vec(&parent).unwrap();
    // Outer ST 0601 decode succeeds — Tag 48 is pass-through bytes.
    let decoded_parent = st0601::decode(&parent_bytes).unwrap();
    assert!(decoded_parent.field_errors.is_empty(), "outer record clean");
    let inner = decoded_parent
        .security_local_set
        .as_deref()
        .expect("Tag 48 preserved verbatim including truncation");
    assert_eq!(inner, sec_bytes.as_slice());
    // Inner ST 0102 decode SURFACES the truncation but doesn't poison
    // the outer record. The sibling-layer composition keeps errors
    // localized.
    let inner_result = st0102::decode(inner);
    // Either Err (truncation hit during iter) or Ok with partial state —
    // both demonstrate the error stays inside the inner layer. The
    // st0102 lenient decoder is structurally tolerant; truncated
    // mid-TLV typically yields Err(Truncated) via `Iter::local_set`.
    match inner_result {
        Ok(_) | Err(KlvDecodeError::Truncated { .. }) => {}
        Err(other) => panic!("unexpected inner error variant: {other:?}"),
    }
}
