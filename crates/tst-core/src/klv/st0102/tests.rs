use super::decode::{decode, decode_strict};
use super::encode::{encode, encode_strict_compliance, encode_to_vec, encoded_len};
use super::model::SecurityLs;
use crate::error::{KlvDecodeError, KlvEncodeError};
use crate::klv::length::write_ber;
use crate::klv::pack::OwnedRawField;
use crate::klv::st0102::enums::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use crate::test_prelude::*;

/// Build an LS body: per item the one tag byte, length (BER), value bytes.
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
    // Strict mode rejects the same input as DuplicateTag.
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

    // Re-encoded big-endian: ST 0102.12's Tag 13 is RFC 2781 UTF-16, here
    // the byte order mark U+FEFF as `FE FF`, then 'F' (U+0046) and 'R'
    // (U+0052) as big-endian code units, six value bytes.
    let bytes = encode_to_vec(&decoded).unwrap();
    assert_eq!(bytes, [0x0D, 0x06, 0xFE, 0xFF, 0x00, 0x46, 0x00, 0x52]);
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
    // `[1,1,1, 1,1,2]` — the second Tag 1 starts at byte 3, and the
    // error must report that offset, not 0.
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
fn unknown_tags_above_127_preserved_on_encode() {
    // ST 0102 LS may grow new tags > 127 in future revisions; the
    // lenient decoder preserves them per ST 0107.5 §6, and encode
    // writes them back as the one-byte tags the set's label declares,
    // so the round-trip stays lossless.
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

    // encoded_len + encode agree on size.
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

// The Security Metadata LS label is `06 0E 2B 34 02 03 …`: byte 6 = 0x03
// declares one-byte local tags (SMPTE ST 336 Table 5), so a tag is one
// byte 0x00 to 0xFF and the length that follows is BER.

#[test]
fn security_unknown_high_tags_are_one_byte() {
    for (tag, byte) in [(127u32, 0x7Fu8), (128, 0x80), (200, 0xC8), (255, 0xFF)] {
        let r = SecurityLs {
            security_classification: Some(SecurityClassification::Unclassified),
            unknown: vec![OwnedRawField {
                tag,
                value: vec![0xAA],
            }],
            ..Default::default()
        };
        let bytes = encode_to_vec(&r).unwrap();
        assert_eq!(bytes, [0x01, 0x01, 0x01, byte, 0x01, 0xAA], "tag {tag}");
        assert_eq!(encoded_len(&r), bytes.len(), "tag {tag}: encoded_len");
        assert_eq!(decode(&bytes).unwrap().unknown, r.unknown, "tag {tag}");
    }
}

#[test]
fn security_one_byte_unknown_then_known_item() {
    // Tag 0x80 (128), length 1, then Tag 1 (classification) = 0x05.
    let r = decode(&[0x80, 0x01, 0x42, 0x01, 0x01, 0x05]).unwrap();
    assert_eq!(
        r.unknown,
        vec![OwnedRawField {
            tag: 128,
            value: vec![0x42],
        }]
    );
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::TopSecret)
    );
}

#[test]
fn unknown_tag_above_255_is_refused() {
    for tag in [256u32, 16384] {
        let r = SecurityLs {
            unknown: vec![OwnedRawField {
                tag,
                value: vec![0xAA],
            }],
            ..Default::default()
        };
        let err = encode_to_vec(&r).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::TagNotOneByte { tag: t } if t == tag),
            "tag {tag}: {err:?}"
        );
    }
}

#[test]
fn security_ls_defined_tags_decode_unchanged_under_one_byte_syntax() {
    // Every defined tag is below 128, where a one-byte tag and a BER-OID
    // tag are the same byte: the full record's bytes and its decode do not
    // depend on the tag syntax, alone and nested in ST 0601 Tag 48.
    let original = SecurityLs {
        security_classification: Some(SecurityClassification::TopSecret),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
        classifying_country: Some("//USA".to_string()),
        sci_shi_info: Some("HCS".to_string()),
        caveats: Some("FOUO".to_string()),
        releasing_instructions: Some("USA".to_string()),
        classified_by: Some("ID-1".to_string()),
        derived_from: Some("Src".to_string()),
        classification_reason: Some("1.4".to_string()),
        declassification_date: Some("20351231".to_string()),
        classification_marking_system: Some("CAPCO".to_string()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
        object_country_codes: Some("USA".to_string()),
        classification_comments: Some("Cmt".to_string()),
        version: Some(12),
        classifying_country_coding_method_version_date: Some("2025-01-15".to_string()),
        object_country_coding_method_version_date: Some("2025-01-15".to_string()),
        unknown: Vec::new(),
        field_errors: Vec::new(),
    };
    let expected = build_record(&[
        (1, &[0x05]),
        (2, &[0x02]),
        (3, b"//USA"),
        (4, b"HCS"),
        (5, b"FOUO"),
        (6, b"USA"),
        (7, b"ID-1"),
        (8, b"Src"),
        (9, b"1.4"),
        (10, b"20351231"),
        (11, b"CAPCO"),
        (12, &[0x02]),
        (13, &[0xFE, 0xFF, 0x00, b'U', 0x00, b'S', 0x00, b'A']),
        (14, b"Cmt"),
        (22, &[0x00, 0x0C]),
        (23, b"2025-01-15"),
        (24, b"2025-01-15"),
    ]);
    let bytes = encode_to_vec(&original).unwrap();
    assert_eq!(bytes, expected);
    assert_eq!(decode(&bytes).unwrap(), original);
    assert_eq!(decode_strict(&bytes).unwrap(), original);

    let parent = crate::klv::st0601::UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        security_local_set: Some(bytes.clone()),
        ..Default::default()
    };
    let parent_bytes = crate::klv::st0601::encode_to_vec(&parent).unwrap();
    let inner = crate::klv::st0601::decode(&parent_bytes)
        .unwrap()
        .security_local_set
        .expect("Tag 48 survives the parent round trip");
    assert_eq!(inner, bytes);
    assert_eq!(decode(&inner).unwrap(), original);
}

#[test]
fn a_0_10_0_two_byte_tag_is_truncated() {
    // 0.10.0 wrote unknown tag 128 as the BER-OID `81 00`. Read with
    // one-byte tags, `81 00` is tag 129 with an empty value, and the
    // length byte that follows becomes a tag whose length overruns the set.
    let written_by_0_10_0 = [
        0x01, 0x01, 0x01, // classification
        0x81, 0x00, 0x01, 0x42, // unknown tag 128 = 'B', two-byte tag
        0x16, 0x02, 0x00, 0x0C, // version 12
    ];
    let err = decode(&written_by_0_10_0).unwrap_err();
    assert!(matches!(err, KlvDecodeError::Truncated { .. }), "{err:?}");
}

#[test]
fn a_0_10_0_two_byte_tag_can_read_as_other_items() {
    // 0.10.0's two-byte tag `81 00` for unknown tag 128 with the value
    // `02 01 05` reads, with one-byte tags, as unknown tag 129 (empty)
    // followed by a typed Tag 3 (Classifying Country) holding `01 05`.
    let written_by_0_10_0 = [
        0x01, 0x01, 0x01, // classification
        0x81, 0x00, 0x03, 0x02, 0x01, 0x05, // unknown tag 128, two-byte tag
        0x16, 0x02, 0x00, 0x0C, // version 12
    ];
    let r = decode(&written_by_0_10_0).unwrap();
    assert_eq!(
        r.unknown,
        vec![OwnedRawField {
            tag: 129,
            value: Vec::new(),
        }]
    );
    assert_eq!(r.classifying_country.as_deref(), Some("\u{1}\u{5}"));
    assert_eq!(r.version, Some(12));
    assert!(r.field_errors.is_empty());
}

// ------------------------------------------------------------------
// Unknown-bucket typed-tag guard tests.
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
// decode_strict canonical-BER rejection.
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
// encode_strict_compliance tests.
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

// -------- ST 0107.5 §6.3.3.2 empty-string convention --------

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

// ST 0107.4-17: a zero-length item means "unknown". It is an occurrence of
// its tag, so the last-duplicate rule applies to it as to any value.

#[test]
fn st0102_zero_length_optional_item_is_unknown() {
    // Tag 1 (classification) and Tag 22 (version), both zero-length.
    let r = decode(&[0x01, 0x00, 0x16, 0x00]).unwrap();
    assert_eq!(r.security_classification, None);
    assert_eq!(r.version, None);
    assert!(r.field_errors.is_empty(), "{:?}", r.field_errors);
}

#[test]
fn st0102_strict_zero_length_required_item_is_missing() {
    let err = decode_strict(&[0x01, 0x00]).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::St0102MissingRequiredTag { tag: 1 }),
        "{err:?}"
    );
}

#[test]
fn st0102_lenient_wrong_length_enum_is_a_field_error() {
    // Tag 1 with two bytes, then a version: the set decodes, the
    // classification is unset and reported, the version kept.
    let r = decode(&[0x01, 0x02, 0x00, 0x00, 0x16, 0x02, 0x00, 0x0C]).unwrap();
    assert_eq!(r.security_classification, None);
    assert_eq!(r.version, Some(12));
    assert_eq!(
        r.field_errors,
        vec![crate::error::KlvFieldError::InvalidLength {
            tag: 1,
            expected: 1,
            got: 2,
        }]
    );
    // A wrong-length version likewise.
    let r = decode(&[0x16, 0x01, 0x0C]).unwrap();
    assert_eq!(r.version, None);
    assert_eq!(
        r.field_errors,
        vec![crate::error::KlvFieldError::InvalidLength {
            tag: 22,
            expected: 2,
            got: 1,
        }]
    );
    // Strict decode still refuses it.
    assert!(matches!(
        decode_strict(&[0x01, 0x02, 0x00, 0x00]).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidLength { tag: 1, .. })
    ));
}

#[test]
fn a_zero_length_duplicate_after_a_value_unsets_it() {
    let r = decode(&[0x05, 0x03, b'a', b'b', b'c', 0x05, 0x00]).unwrap();
    assert_eq!(r.caveats, None);
    let r = decode(&[0x01, 0x01, 0x01, 0x01, 0x00]).unwrap();
    assert_eq!(r.security_classification, None);
    let r = decode(&[0x16, 0x02, 0x00, 0x0C, 0x16, 0x00]).unwrap();
    assert_eq!(r.version, None);
    let r = decode(&[0x0D, 0x04, 0xFE, 0xFF, 0x00, b'U', 0x0D, 0x00]).unwrap();
    assert_eq!(r.object_country_codes, None);
    // Strict decode refuses the repeat, as for any repeated tag.
    assert!(matches!(
        decode_strict(&[0x05, 0x03, b'a', b'b', b'c', 0x05, 0x00]).unwrap_err(),
        KlvDecodeError::DuplicateTag { tag: 5, offset: 5 }
    ));
}

#[test]
fn a_zero_length_duplicate_before_a_value_keeps_it() {
    let r = decode(&[0x05, 0x00, 0x05, 0x03, b'a', b'b', b'c']).unwrap();
    assert_eq!(r.caveats.as_deref(), Some("abc"));
    let r = decode(&[0x01, 0x00, 0x01, 0x01, 0x05]).unwrap();
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::TopSecret)
    );
    let r = decode(&[0x16, 0x00, 0x16, 0x02, 0x00, 0x0C]).unwrap();
    assert_eq!(r.version, Some(12));
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

// -------- control-char stripping in strict path only --------

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

// ------------------------------------------------------------------
// The strict paths agree with each other and with ISO/IEC 646.
// ------------------------------------------------------------------

#[test]
fn strict_decode_accepts_a_repeated_unknown_tag() {
    // ST 0107.3-04: a decoder skips unknown items, so their repetition is
    // not a defect of the set; a repeated defined tag still is.
    let mut bytes = encode_to_vec(&build_minimal_required_record()).unwrap();
    bytes.extend_from_slice(&[0x63, 0x01, 0xAA, 0x63, 0x01, 0xBB]);
    let r = decode_strict(&bytes).unwrap();
    assert_eq!(
        r.unknown,
        vec![
            OwnedRawField {
                tag: 99,
                value: vec![0xAA],
            },
            OwnedRawField {
                tag: 99,
                value: vec![0xBB],
            },
        ]
    );
    let mut repeated = encode_to_vec(&build_minimal_required_record()).unwrap();
    repeated.extend_from_slice(&[0x01, 0x01, 0x01]);
    assert!(matches!(
        decode_strict(&repeated).unwrap_err(),
        KlvDecodeError::DuplicateTag { tag: 1, .. }
    ));
}

#[test]
fn strict_decode_refuses_non_ascii_iso646_text() {
    let mut bytes = encode_to_vec(&build_minimal_required_record()).unwrap();
    bytes.extend_from_slice(&[0x05, 0x02, 0xC3, 0xA9]); // caveats "é"
    assert!(matches!(
        decode_strict(&bytes).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::NonAsciiText { tag: 5 })
    ));
    // The lenient decode keeps the text as the producer wrote it.
    assert_eq!(decode(&bytes).unwrap().caveats.as_deref(), Some("é"));
}

#[test]
fn lenient_decode_keeps_a_set_with_invalid_utf8() {
    // Classification SECRET (0x04), then caveats holding the byte FF.
    let r = decode(&[0x01, 0x01, 0x04, 0x05, 0x01, 0xFF]).unwrap();
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::Secret)
    );
    assert_eq!(r.caveats, None);
    assert_eq!(
        r.field_errors,
        vec![crate::error::KlvFieldError::InvalidUtf8 { tag: 5 }]
    );
    // Strict decode still refuses it.
    assert!(matches!(
        decode_strict(&[0x01, 0x01, 0x04, 0x05, 0x01, 0xFF]).unwrap_err(),
        KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidUtf8 { tag: 5 })
    ));
}

#[test]
fn strict_decode_refuses_a_malformed_date() {
    // ST 0102.12 Table 2: Tag 10 is YYYYMMDD, Tags 23 and 24 YYYY-MM-DD.
    for (tag, date) in [
        (10u8, &b"2026AB01"[..]),
        (23, b"2026/01/01"),
        (24, b"2026-0101-"),
    ] {
        let mut bytes = encode_to_vec(&build_minimal_required_record()).unwrap();
        bytes.push(tag);
        bytes.push(date.len() as u8);
        bytes.extend_from_slice(date);
        let err = decode_strict(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                KlvDecodeError::FieldError(crate::error::KlvFieldError::InvalidFormat { tag: t })
                    if t == u32::from(tag)
            ),
            "tag {tag}: {err:?}"
        );
    }
    // The check is of the shape, not the calendar.
    let mut bytes = encode_to_vec(&build_minimal_required_record()).unwrap();
    bytes.extend_from_slice(&[0x0A, 0x08]);
    bytes.extend_from_slice(b"99999999");
    assert!(decode_strict(&bytes).is_ok());
}

#[test]
fn strict_encode_refuses_what_strict_decode_refuses() {
    let cases: [(u32, SecurityLs); 4] = [
        (
            1,
            SecurityLs {
                security_classification: Some(SecurityClassification::from_wire(0xFA)),
                ..minimal_strict_record()
            },
        ),
        (
            10,
            SecurityLs {
                declassification_date: Some("2".to_owned()),
                ..minimal_strict_record()
            },
        ),
        (
            10,
            SecurityLs {
                declassification_date: Some("2026AB01".to_owned()),
                ..minimal_strict_record()
            },
        ),
        (
            5,
            SecurityLs {
                caveats: Some("é".to_owned()),
                ..minimal_strict_record()
            },
        ),
    ];
    for (tag, record) in cases {
        let err = encode_strict_compliance(&record).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::InvalidFieldValue { tag: t, .. } if t == tag),
            "tag {tag}: {err:?}"
        );
        // What the encoder refuses, strict decode refuses too.
        let written = encode_to_vec(&record).unwrap();
        assert!(decode_strict(&written).is_err(), "tag {tag}");
    }
    let good = SecurityLs {
        declassification_date: Some("20351231".to_owned()),
        classifying_country_coding_method_version_date: Some("2025-01-15".to_owned()),
        ..minimal_strict_record()
    };
    let bytes = encode_strict_compliance(&good).unwrap();
    assert_eq!(decode_strict(&bytes).unwrap(), good);
}

#[test]
fn a_duplicate_that_does_not_decode_unsets_the_earlier_value() {
    use crate::error::KlvFieldError;
    // The last occurrence of a tag decides, as in ST 0601: when it does
    // not decode, the field is unset and its field error recorded.
    // Tag 1 (a code): one byte, then two.
    let r = decode(&[0x01, 0x01, 0x02, 0x01, 0x02, 0x01, 0x01]).unwrap();
    assert_eq!(r.security_classification, None);
    assert_eq!(
        r.field_errors,
        [KlvFieldError::InvalidLength {
            tag: 1,
            expected: 1,
            got: 2
        }]
    );
    // Tag 22 (the version): two bytes, then one.
    let r = decode(&[0x16, 0x02, 0x00, 0x0C, 0x16, 0x01, 0x0C]).unwrap();
    assert_eq!(r.version, None);
    assert_eq!(r.field_errors.len(), 1);
    // Tag 3 (text): "US", then a byte that is not UTF-8.
    let r = decode(&[0x03, 0x02, b'U', b'S', 0x03, 0x01, 0xFF]).unwrap();
    assert_eq!(r.classifying_country, None);
    assert_eq!(r.field_errors, [KlvFieldError::InvalidUtf8 { tag: 3 }]);
    // Tag 13 (UTF-16): "U", then an odd number of bytes.
    let r = decode(&[
        0x0D, 0x04, 0xFE, 0xFF, 0x00, b'U', 0x0D, 0x03, 0xFE, 0xFF, 0x00,
    ])
    .unwrap();
    assert_eq!(r.object_country_codes, None);
    assert_eq!(r.field_errors, [KlvFieldError::InvalidUtf16 { tag: 13 }]);
    // The other order: the good occurrence is the last, and decides.
    let r = decode(&[0x01, 0x02, 0x01, 0x01, 0x01, 0x01, 0x02]).unwrap();
    assert_eq!(
        r.security_classification,
        Some(SecurityClassification::Restricted)
    );
    assert_eq!(r.field_errors.len(), 1);
}
