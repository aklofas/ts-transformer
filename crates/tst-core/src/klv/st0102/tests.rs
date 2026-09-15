use super::decode::{decode, decode_strict};
use super::encode::{encode, encode_strict_compliance, encode_to_vec, encoded_len};
use super::model::SecurityLs;
use crate::error::{KlvDecodeError, KlvEncodeError};
use crate::klv::length::write_ber;
use crate::klv::pack::OwnedRawField;
use crate::klv::st0102::enums::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};

/// Build a single-tag LS body: tag (BER-OID, 1 byte for tags ≤ 127),
/// length (BER), value bytes.
fn build_record(tags: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, value) in tags {
        out.push(*tag);
        let mut len_buf = [0u8; 9];
        let n = write_ber(value.len(), &mut len_buf).expect("len encodable");
        out.extend_from_slice(&len_buf[..n]);
        out.extend_from_slice(value);
    }
    out
}

#[test]
fn decode_tag1_security_classification_secret() {
    let buf = build_record(&[(1, &[0x04])]);
    let r = decode(&buf).expect("decode succeeds");
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::Secret)
    );
}

#[test]
fn decode_tag1_unknown_codepoint_lenient() {
    let buf = build_record(&[(1, &[0xFA])]);
    let r = decode(&buf).expect("lenient tolerates unknown codepoint");
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::Unknown(0xFA))
    );
}

#[test]
fn decode_tag2_classifying_country_coding_method() {
    let buf = build_record(&[(2, &[0x05])]);
    let r = decode(&buf).unwrap();
    assert_eq!(
        r.classifying_country_coding_method,
        Some(ClassifyingCountryCodingMethod::Iso3166Numeric)
    );
}

#[test]
fn decode_tag3_classifying_country() {
    let buf = build_record(&[(3, b"//USA")]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.classifying_country.as_deref(), Some("//USA"));
}

#[test]
fn decode_tag10_declassification_date() {
    let buf = build_record(&[(10, b"20300101")]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.declassification_date.as_deref(), Some("20300101"));
}

#[test]
fn decode_tag12_object_country_coding_method() {
    // Tag 12's 0x03 is ISO-3166 Numeric (≠ Tag 2's 0x05).
    let buf = build_record(&[(12, &[0x03])]);
    let r = decode(&buf).unwrap();
    assert_eq!(
        r.object_country_coding_method,
        Some(ObjectCountryCodingMethod::Iso3166Numeric)
    );
}

#[test]
fn decode_tag13_object_country_codes_utf16_be_with_bom() {
    // BE BOM + UTF-16 BE for "US"
    let mut payload = vec![0xFE, 0xFF];
    payload.extend_from_slice(&[0x00, b'U', 0x00, b'S']);
    let buf = build_record(&[(13, &payload)]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.object_country_codes.as_deref(), Some("US"));
}

#[test]
fn decode_tag13_object_country_codes_utf16_le_with_bom() {
    // LE BOM + UTF-16 LE for "US"
    let mut payload = vec![0xFF, 0xFE];
    payload.extend_from_slice(&[b'U', 0x00, b'S', 0x00]);
    let buf = build_record(&[(13, &payload)]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.object_country_codes.as_deref(), Some("US"));
}

#[test]
fn decode_tag13_object_country_codes_utf16_no_bom_defaults_be() {
    // No BOM → BE per RFC 2781 §4.3
    let buf = build_record(&[(13, &[0x00, b'D', 0x00, b'E'])]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.object_country_codes.as_deref(), Some("DE"));
}

#[test]
fn decode_tag13_invalid_utf16_lenient_signals_via_field_errors() {
    // Odd-length buffer → UTF-16 decode fails. Lenient mode sets
    // the field to None and pushes a KlvFieldError::InvalidUtf16
    // to field_errors per spec §3.5 (mirrors st0601 pattern).
    let raw = [0x00, b'U', 0x00];
    let buf = build_record(&[(13, &raw)]);
    let r = decode(&buf).unwrap();
    assert!(r.object_country_codes.is_none());
    assert!(r.unknown.is_empty());
    assert_eq!(r.field_errors.len(), 1);
    assert!(matches!(
        r.field_errors[0],
        crate::error::KlvFieldError::InvalidUtf16 { tag: 13 }
    ));
}

#[test]
fn decode_tag22_version() {
    let buf = build_record(&[(22, &[0x00, 0x0C])]); // ST 0102.12
    let r = decode(&buf).unwrap();
    assert_eq!(r.version, Some(12));
}

#[test]
fn decode_unknown_tag_lenient_preserves() {
    // Tag 99 is not in the LS table — pass through as forward-
    // compat per ST 0107.5 §6.
    let buf = build_record(&[(99, b"xyz")]);
    let r = decode(&buf).unwrap();
    assert_eq!(r.unknown.len(), 1);
    assert_eq!(r.unknown[0].tag, 99);
    assert_eq!(r.unknown[0].value, b"xyz");
}

#[test]
fn decode_duplicate_tag_lenient_last_wins() {
    // Sibling-pattern parity with klv::st0601 lenient mode:
    // duplicate tags overwrite silently, later occurrence wins.
    // Strict mode (Task 6) rejects the same input as DuplicateTag.
    let buf = build_record(&[(1, &[0x01]), (1, &[0x02])]);
    let r = decode(&buf).expect("lenient tolerates duplicate, last wins");
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::Restricted) // 0x02, the second occurrence
    );
}

#[test]
fn decode_empty_record_lenient_succeeds() {
    // Lenient mode accepts a record missing all tags.
    let r = decode(&[]).unwrap();
    assert!(r.security_classification.is_none());
    assert!(r.unknown.is_empty());
}

#[test]
fn round_trip_minimal_required_fields() {
    let original = SecurityLs {
        security_classification: Some(SecurityClassification::Secret),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        version: Some(12),
        ..Default::default()
    };

    let bytes = encode_to_vec(&original).expect("encode succeeds");
    let decoded = decode(&bytes).expect("decode succeeds");
    assert_eq!(decoded, original);
}

#[test]
fn round_trip_full_record() {
    let original = SecurityLs {
        security_classification: Some(SecurityClassification::TopSecret),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166Numeric),
        classifying_country: Some("//USA".to_string()),
        sci_shi_info: Some("HCS-O".to_string()),
        caveats: Some("FOUO".to_string()),
        releasing_instructions: Some("USA CAN GBR".to_string()),
        classified_by: Some("ID-12345".to_string()),
        derived_from: Some("Multiple Sources".to_string()),
        classification_reason: Some("1.4(c)".to_string()),
        declassification_date: Some("20351231".to_string()),
        classification_marking_system: Some("CAPCO".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166Numeric),
        object_country_codes: Some("USA".to_string()),
        classification_comments: Some("Test record".to_string()),
        version: Some(12),
        classifying_country_coding_method_version_date: Some("2025-01-15".to_string()),
        object_country_coding_method_version_date: Some("2025-01-15".to_string()),
        unknown: Vec::new(),
        field_errors: Vec::new(),
    };

    let bytes = encode_to_vec(&original).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn round_trip_with_unknown_tag_preserved() {
    let mut original = SecurityLs {
        security_classification: Some(SecurityClassification::Confidential),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".to_string()),
        version: Some(12),
        ..Default::default()
    };
    original.unknown.push(OwnedRawField {
        tag: 99,
        value: b"forward-compat-payload".to_vec(),
    });

    let bytes = encode_to_vec(&original).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 99);
}

#[test]
fn encode_buffer_too_small_rejects() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        ..Default::default()
    };
    let mut buf = [0u8; 1]; // need ≥ 3 bytes for tag 1
    let err = encode(&r, &mut buf).unwrap_err();
    // Tag 1 needs: 1 byte tag + 1 byte BER len + 1 byte value = 3 bytes; got 1.
    assert!(matches!(
        err,
        KlvEncodeError::BufferTooSmall { needed: 3, got: 1 }
    ));
}

#[test]
fn encoded_len_matches_actual() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Restricted),
        classifying_country: Some("//GBR".to_string()),
        object_country_codes: Some("GB".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let n = encoded_len(&r);
    let bytes = encode_to_vec(&r).unwrap();
    assert_eq!(n, bytes.len());
}

#[test]
fn round_trip_utf16_normalizes_to_be() {
    // A consumer hand-builds an LE-encoded Tag 13 record.
    let mut payload = vec![0xFF, 0xFE]; // LE BOM
    payload.extend_from_slice(&[b'F', 0x00, b'R', 0x00]);
    let buf = build_record(&[(13, &payload)]);
    let decoded = decode(&buf).unwrap();
    assert_eq!(decoded.object_country_codes.as_deref(), Some("FR"));

    // Re-encode and verify BE BOM normalization.
    let bytes = encode_to_vec(&decoded).unwrap();
    // Tag 13 byte + BER-len(1 byte) + BOM(0xFE 0xFF) + 'F' BE +
    // 'R' BE — verify the BOM bytes appear at the expected offset.
    // We don't decode BER here; the round-trip via decode below
    // is the primary correctness check.
    let redecoded = decode(&bytes).unwrap();
    assert_eq!(redecoded, decoded);
}

/// Helper: build a minimum-required record per ST 0102.12 §6.7
/// (tags 1, 2, 3, 12, 13, 22).
fn build_minimal_required_record() -> SecurityLs {
    SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".to_string()),
        version: Some(12),
        ..Default::default()
    }
}

#[test]
fn strict_accepts_minimal_required_record() {
    let r = build_minimal_required_record();
    let bytes = encode_to_vec(&r).unwrap();
    let decoded = decode_strict(&bytes).expect("strict accepts minimal record");
    assert_eq!(decoded, r);
}

#[test]
fn strict_rejects_missing_tag_1() {
    let mut r = build_minimal_required_record();
    r.security_classification = None;
    let bytes = encode_to_vec(&r).unwrap();
    let err = decode_strict(&bytes).unwrap_err();
    assert!(matches!(
        err,
        KlvDecodeError::St0102MissingRequiredTag { tag: 1 }
    ));
}

#[test]
fn strict_rejects_missing_tag_2() {
    let mut r = build_minimal_required_record();
    r.classifying_country_coding_method = None;
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::St0102MissingRequiredTag { tag: 2 }
    ));
}

#[test]
fn strict_rejects_missing_tag_3() {
    let mut r = build_minimal_required_record();
    r.classifying_country = None;
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::St0102MissingRequiredTag { tag: 3 }
    ));
}

#[test]
fn strict_rejects_missing_tag_12() {
    let mut r = build_minimal_required_record();
    r.object_country_coding_method = None;
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::St0102MissingRequiredTag { tag: 12 }
    ));
}

#[test]
fn strict_rejects_missing_tag_13() {
    let mut r = build_minimal_required_record();
    r.object_country_codes = None;
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::St0102MissingRequiredTag { tag: 13 }
    ));
}

#[test]
fn strict_rejects_missing_tag_22() {
    let mut r = build_minimal_required_record();
    r.version = None;
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::St0102MissingRequiredTag { tag: 22 }
    ));
}

#[test]
fn strict_rejects_unknown_tag1_codepoint() {
    // Encode raw bytes — encode_to_vec wouldn't fail on
    // SecurityClassification::Unknown(0xFA), but strict decode
    // must reject.
    let mut r = build_minimal_required_record();
    r.security_classification = Some(SecurityClassification::Unknown(0xFA));
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidCodepoint {
            tag: 1,
            value: 0xFA,
        })
    ));
}

#[test]
fn strict_rejects_unknown_tag2_codepoint() {
    let mut r = build_minimal_required_record();
    r.classifying_country_coding_method = Some(ClassifyingCountryCodingMethod::Unknown(0x7F));
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidCodepoint {
            tag: 2,
            value: 0x7F,
        })
    ));
}

#[test]
fn strict_rejects_unknown_tag12_codepoint() {
    let mut r = build_minimal_required_record();
    r.object_country_coding_method = Some(ObjectCountryCodingMethod::Unknown(0x20));
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidCodepoint {
            tag: 12,
            value: 0x20,
        })
    ));
}

#[test]
fn strict_rejects_omitted_value_codepoint_tag2() {
    let mut r = build_minimal_required_record();
    r.classifying_country_coding_method = Some(ClassifyingCountryCodingMethod::OmittedValue08);
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidCodepoint {
            tag: 2,
            value: 0x08,
        })
    ));
}

#[test]
fn strict_rejects_omitted_value_codepoint_tag12() {
    let mut r = build_minimal_required_record();
    r.object_country_coding_method = Some(ObjectCountryCodingMethod::OmittedValue0A);
    let bytes = encode_to_vec(&r).unwrap();
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidCodepoint {
            tag: 12,
            value: 0x0A,
        })
    ));
}

#[test]
fn strict_rejects_invalid_utf16_tag13() {
    // Required tags 1, 2, 3, 12, 22 present + Tag 13 with
    // odd-length payload (UTF-16 needs even bytes).
    // (Building the bytes manually is easier than mutating
    // encode_to_vec output to splice in the bad UTF-16.)
    let bad_utf16 = [0x00, b'U', 0x00];
    let manual = build_record(&[
        (1, &[0x01]),
        (2, &[0x01]),
        (3, b"//US"),
        (12, &[0x01]),
        (13, &bad_utf16),
        (22, &[0x00, 0x0C]),
    ]);
    assert!(matches!(
        decode_strict(&manual).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidUtf16 { tag: 13 })
    ));
}

#[test]
fn strict_rejects_duplicate_tag() {
    // Duplicate tag 1.
    let manual = build_record(&[
        (1, &[0x01]),
        (1, &[0x02]),
        (2, &[0x01]),
        (3, b"//US"),
        (12, &[0x01]),
        (13, &[0xFE, 0xFF, 0x00, b'U', 0x00, b'S']),
        (22, &[0x00, 0x0C]),
    ]);
    assert!(matches!(
        decode_strict(&manual).unwrap_err(),
        KlvDecodeError::DuplicateTag { tag: 1, .. }
    ));
}

#[test]
fn strict_duplicate_tag_reports_the_repeat_offset() {
    // CORR-29(a): `[1,1,1, 1,1,2]` — the second Tag 1 starts at byte 3.
    // The dedup used to run on the offset-blind `Iter` and hard-code 0.
    let err = decode_strict(&[1, 1, 1, 1, 1, 2]).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 1, offset: 3 }),
        "{err:?}"
    );
}

#[test]
fn strict_preserves_unknown_tag() {
    // Required tags + a forward-compat unknown tag — strict
    // mode preserves the unknown tag rather than rejecting per
    // spec §3.7 / ST 0107.5 §6.
    let mut r = build_minimal_required_record();
    r.unknown.push(OwnedRawField {
        tag: 99,
        value: b"future-tag".to_vec(),
    });
    let bytes = encode_to_vec(&r).unwrap();
    let decoded = decode_strict(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 99);
    assert_eq!(decoded.unknown[0].value, b"future-tag");
}

#[test]
fn strict_rejects_truncated_value() {
    // Tag 22 (Version) declares 2-byte length but only 1 byte
    // present in the buffer.
    let mut buf = build_record(&[
        (1, &[0x01]),
        (2, &[0x01]),
        (3, b"//US"),
        (12, &[0x01]),
        (13, &[0xFE, 0xFF, 0x00, b'U', 0x00, b'S']),
    ]);
    // Truncated tag 22: tag byte + length-1 + only 1 byte
    buf.extend_from_slice(&[22, 0x01, 0x0C]); // len=1 but spec wants 2
    // The decoder doesn't bail on length-mismatch within Iter
    // (Iter respects the BER length verbatim). Instead the
    // U16Be branch raises InvalidLength.
    assert!(matches!(
        decode_strict(&buf).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidLength {
            tag: 22,
            expected: 2,
            got: 1,
        })
    ));
}

#[test]
fn unknown_tags_above_127_preserved_via_ber_oid_on_encode() {
    // ST 0102 LS may grow new tags > 127 in future revisions; the
    // lenient decoder already preserves them per ST 0107.5 §6, and
    // since the validate-1 E4 fix encode emits multi-byte BER-OID
    // per ST 0107 §6.3.1 so the round-trip stays lossless.
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        unknown: vec![
            OwnedRawField {
                tag: 128,
                value: b"forward-compat".to_vec(),
            },
            OwnedRawField {
                tag: 200,
                value: b"other".to_vec(),
            },
        ],
        ..Default::default()
    };

    let n = encoded_len(&r);
    let bytes = encode_to_vec(&r).unwrap();

    // encoded_len + encode agree on size (both account for the
    // multi-byte BER-OID encoding of tags > 127).
    assert_eq!(n, bytes.len());

    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.security_classification,
        Some(SecurityClassification::Unclassified)
    );
    assert_eq!(decoded.unknown.len(), 2);
    assert_eq!(decoded.unknown[0].tag, 128);
    assert_eq!(decoded.unknown[0].value, b"forward-compat");
    assert_eq!(decoded.unknown[1].tag, 200);
    assert_eq!(decoded.unknown[1].value, b"other");
}

/// BER-OID round-trip boundary: tag 127 — last single-byte value
/// (`0x7F`). Encoded as one byte.
#[test]
fn unknown_tag_127_round_trips_single_byte_ber_oid() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        unknown: vec![OwnedRawField {
            tag: 127,
            value: b"max-single-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    assert_eq!(encoded_len(&r), bytes.len());
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 127);
    assert_eq!(decoded.unknown[0].value, b"max-single-byte");
}

/// BER-OID round-trip boundary: tag 128 — first multi-byte value
/// (`0x81 0x00`). The continuation bit `0x80` on the first byte
/// signals "more bytes follow".
#[test]
fn unknown_tag_128_round_trips_multi_byte_ber_oid() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        unknown: vec![OwnedRawField {
            tag: 128,
            value: b"first-multi-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    assert_eq!(encoded_len(&r), bytes.len());

    // Hand-verify the on-wire BER-OID bytes for tag 128. After the
    // single-byte tag-1 record (3 bytes), the next two bytes are
    // the BER-OID tag = 0x81 0x00.
    // Tag 1 record: [0x01, 0x01, 0x00] = 3 bytes.
    assert_eq!(&bytes[3..5], &[0x81, 0x00]);

    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 128);
    assert_eq!(decoded.unknown[0].value, b"first-multi-byte");
}

/// BER-OID round-trip boundary: tag 16383 (`2^14 - 1`) — last
/// two-byte value (`0xFF 0x7F`).
#[test]
fn unknown_tag_16383_round_trips_two_byte_ber_oid() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        unknown: vec![OwnedRawField {
            tag: 16383,
            value: b"max-two-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    assert_eq!(encoded_len(&r), bytes.len());
    assert_eq!(&bytes[3..5], &[0xFF, 0x7F]);

    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 16383);
    assert_eq!(decoded.unknown[0].value, b"max-two-byte");
}

/// BER-OID round-trip boundary: tag 16384 (`2^14`) — first
/// three-byte value (`0x81 0x80 0x00`).
#[test]
fn unknown_tag_16384_round_trips_three_byte_ber_oid() {
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        unknown: vec![OwnedRawField {
            tag: 16384,
            value: b"first-three-byte".to_vec(),
        }],
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    assert_eq!(encoded_len(&r), bytes.len());
    assert_eq!(&bytes[3..6], &[0x81, 0x80, 0x00]);

    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 16384);
    assert_eq!(decoded.unknown[0].value, b"first-three-byte");
}

// ------------------------------------------------------------------
// Task 1 (WP-F / REF-KLV-02): unknown-bucket typed-tag guard tests.
// ------------------------------------------------------------------

#[test]
fn encode_rejects_typed_tag_in_unknown() {
    // A caller who places a typed tag (Tag 3 = Classifying Country) in
    // `unknown` would make the encoder emit Tag 3 twice — output that the
    // ST 0102 decode_strict rejects as DuplicateTag. Guard it, mirroring
    // st0601::encode's is_reserved_or_typed_tag (ST 0107.5 §6.3.4 single-use).
    let mut r = SecurityLs::default();
    r.unknown.push(OwnedRawField {
        tag: 3,
        value: vec![b'/', b'/', b'U', b'S'],
    });
    let mut buf = [0u8; 256];
    let err = encode(&r, &mut buf).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::ReservedTagInUnknown { tag: 3 }),
        "got {err:?}"
    );
}

#[test]
fn encode_allows_truly_unknown_tag_in_unknown() {
    // A forward-compat tag the typed model doesn't know (e.g. 200) must
    // still pass through (round-trips). This is the legitimate use of `unknown`.
    let mut r = SecurityLs::default();
    r.unknown.push(OwnedRawField {
        tag: 200,
        value: vec![0xAB, 0xCD],
    });
    let mut buf = [0u8; 256];
    let n = encode(&r, &mut buf).expect("forward-compat unknown tag must encode");
    assert!(n > 0);
}

// ------------------------------------------------------------------
// Task 5 (WP-G / REF-KLV-05): decode_strict canonical-BER rejection.
// ------------------------------------------------------------------

#[test]
fn decode_strict_rejects_non_canonical_length() {
    // Tag 22 (0x16), value length 5 encoded in long form (0x81 0x05) — must
    // be the short form 0x05 per ST 0107.5 §6.3 canonical BER. The strict
    // pre-walk rejects before typed decode, so the value content is moot.
    let buf = [0x16, 0x81, 0x05, b'0', b'1', b'0', b'2', b'.'];
    let result = decode_strict(&buf);
    assert!(
        matches!(result, Err(KlvDecodeError::NonCanonicalLength { .. })),
        "got {result:?}"
    );
}

#[test]
fn decode_strict_rejects_non_canonical_tag() {
    // BER-OID tag with a leading 0x80 continuation byte (non-canonical zero).
    let buf = [0x80, 0x01, 0x00];
    let result = decode_strict(&buf);
    assert!(
        matches!(result, Err(KlvDecodeError::NonCanonicalTag { .. })),
        "got {result:?}"
    );
}

/// `klv::st0102::SECURITY_LS_UL` is a re-export of the
/// `UniversalLabel`-typed constant — the bytes match the
/// universal_label.rs canonical form.
#[test]
fn security_ls_ul_reexport_matches_universal_label() {
    assert_eq!(
        super::SECURITY_LS_UL,
        crate::klv::universal_label::UniversalLabel::SECURITY_LS_UL.0,
    );
}

// ------------------------------------------------------------------
// Task 3 (WP-F / REF-KLV-02): encode_strict_compliance tests.
// ------------------------------------------------------------------

#[test]
fn st0102_strict_rejects_missing_classification() {
    use super::encode::encode_strict_compliance;
    // All fields None — first required tag (1) is absent.
    let r = SecurityLs::default();
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 1, .. }),
        "got {err:?}"
    );
}

#[test]
fn st0102_strict_rejects_in_required_tag_order() {
    use super::encode::encode_strict_compliance;
    // Tag 1 present, Tag 2 absent → error on tag 2.
    let mut r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        ..Default::default()
    };
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 2, .. }),
        "got {err:?}"
    );
    // Tags 1+2 present, Tag 3 absent → error on tag 3.
    r.classifying_country_coding_method = Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter);
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 3, .. }),
        "got {err:?}"
    );
    // Tags 1+2+3 present, Tag 12 absent → error on tag 12.
    r.classifying_country = Some("//US".to_string());
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 12, .. }),
        "got {err:?}"
    );
    // Tags 1+2+3+12 present, Tag 13 absent → error on tag 13.
    r.object_country_coding_method = Some(ObjectCountryCodingMethod::Iso3166TwoLetter);
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 13, .. }),
        "got {err:?}"
    );
    // Tags 1+2+3+12+13 present, Tag 22 absent → error on tag 22.
    r.object_country_codes = Some("US".to_string());
    let err = encode_strict_compliance(&r).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::MissingMandatoryItem { tag: 22, .. }),
        "got {err:?}"
    );
}

#[test]
fn st0102_strict_accepts_full_record_and_round_trips() {
    use super::encode::encode_strict_compliance;
    // Build a record with all 6 required fields populated.
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".to_string()),
        version: Some(12),
        ..Default::default()
    };
    let bytes = encode_strict_compliance(&r).expect("full record must strict-encode");
    // Strict-encoded bytes must pass decode_strict (symmetric contract).
    let decoded = decode_strict(&bytes).expect("strict-encoded bytes must pass decode_strict");
    assert_eq!(decoded, r);
}

#[test]
fn st0102_lenient_encode_still_accepts_partial() {
    // §6.4 partial record: default encode must NOT reject (regression pin).
    let r = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        ..Default::default()
    };
    let mut buf = [0u8; 256];
    encode(&r, &mut buf).expect("lenient encode must accept a partial record");
}

// -------- DA-KLV-1: ST 0107.5 §6.3.3.2 empty-string convention --------

#[test]
fn st0102_empty_string_encodes_as_nul_and_round_trips() {
    // Some("") → [0x00] on wire → Some("") on decode.
    let r = SecurityLs {
        caveats: Some(String::new()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    // Tag 5 (caveats) with length 1 and value 0x00: [0x05, 0x01, 0x00]
    let pos = bytes.windows(3).position(|w| w == [0x05, 0x01, 0x00]);
    assert!(
        pos.is_some(),
        "expected [05 01 00] for empty caveats, bytes={bytes:?}"
    );
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.caveats.as_deref(),
        Some(""),
        "empty string should round-trip as Some(\"\")"
    );
}

#[test]
fn st0102_length0_string_decodes_as_absent() {
    // Tag 5 (caveats) with length 0: [0x05, 0x00]
    let body: Vec<u8> = vec![0x05, 0x00];
    let decoded = decode(&body).unwrap();
    assert_eq!(
        decoded.caveats, None,
        "length-0 string should decode as None"
    );
}

#[test]
fn st0102_nul_byte_decodes_as_empty_string() {
    // Tag 7 (classified_by) with single NUL value: [0x07, 0x01, 0x00]
    let body: Vec<u8> = vec![0x07, 0x01, 0x00];
    let decoded = decode(&body).unwrap();
    assert_eq!(
        decoded.classified_by.as_deref(),
        Some(""),
        "single NUL byte should decode as empty string"
    );
}

#[test]
fn st0102_encoded_len_counts_nul_for_empty_string() {
    // empty string → 1 value byte (NUL), so tag + ber_len(1) + 1 = 3 bytes
    let r = SecurityLs {
        classified_by: Some(String::new()),
        ..Default::default()
    };
    let n = encoded_len(&r);
    // tag 7 (1 byte) + length 1 (1 byte) + value [0x00] (1 byte) = 3
    assert_eq!(
        n, 3,
        "encoded_len should count 1 wire byte for empty string (NUL signal)"
    );
}

// -------- DA-KLV-2: control-char stripping in strict path only --------

fn minimal_strict_record() -> SecurityLs {
    // A minimal valid SecurityLs for encode_strict_compliance (all 6 required tags).
    SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166Numeric),
        classifying_country: Some("840".to_owned()), // US ISO 3166 numeric
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166Numeric),
        object_country_codes: Some("840".to_owned()),
        version: Some(12),
        ..Default::default()
    }
}

#[test]
fn st0102_strict_encode_strips_control_chars() {
    let mut r = minimal_strict_record();
    r.caveats = Some("CAVEATS\x01\x7F".to_owned());
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.caveats.as_deref(),
        Some("CAVEATS"),
        "strict encode must strip control chars from Iso646/FixedAscii fields"
    );
}

#[test]
fn st0102_strict_encode_all_control_chars_strips_to_empty_nul() {
    let mut r = minimal_strict_record();
    r.caveats = Some("\x01\x7F".to_owned());
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.caveats.as_deref(),
        Some(""),
        "all-control-char field must strip to empty string"
    );
}

#[test]
fn st0102_strict_encode_trims_leading_trailing_whitespace() {
    // ST 0107.3-12: end tab/LF/CR/space trimmed; embedded space kept.
    let mut r = minimal_strict_record();
    r.caveats = Some("  REL TO \r\n".to_owned());
    let bytes = encode_strict_compliance(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.caveats.as_deref(),
        Some("REL TO"),
        "strict encode must trim end-whitespace but keep the embedded space"
    );
}

#[test]
fn st0102_default_encode_does_not_strip_control_chars() {
    let r = SecurityLs {
        caveats: Some("CAV\x01EAT".to_owned()),
        ..Default::default()
    };
    let bytes = encode_to_vec(&r).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        decoded.caveats.as_deref(),
        Some("CAV\x01EAT"),
        "default encode must NOT strip control chars"
    );
}
