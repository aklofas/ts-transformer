//! Standalone-PID consumer pattern: VMTI as its own KLV record with
//! 16-byte UL prefix + BER outer length + LS body.
//!
//! Mirrors the docstring example in `klv::st0903::mod.rs`. Verifies
//! that consumers carrying VMTI on its own KLV PID (separate from any
//! ST 0601 stream) can dispatch via the `VMTI_LS_UL` constant without
//! the demuxer needing to be VMTI-aware.

use tst_core::error::KlvDecodeError;
use tst_core::klv::checksum::checksum_running_sum_16;
use tst_core::klv::{length, st0903};

#[test]
fn standalone_ul_dispatch_pattern() {
    // Build a VMTI LS via the self-checksumming standalone entry —
    // emits [VMTI_LS_UL:16][outer BER length][body][Tag 1 TLV] per
    // ST 0903.4-17 / ST 0903.6-119 with a running-sum-16 checksum.
    let vmti = st0903::VmtiLs {
        // `checksum` is ignored on the standalone encode path — the
        // encoder computes the running 16-bit summation across
        // [UL .. start of Tag 1 value] per ST 0903.6 §10.1.1.
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        targets: vec![st0903::VTargetPack {
            target_id: 42,
            centroid_pixel: Some(1000),
            ..Default::default()
        }],
        ..Default::default()
    };
    let record = st0903::encode_to_vec_standalone(&vmti).unwrap();

    // Consumer-side dispatch (the documented pattern from
    // klv::st0903's module rustdoc).
    assert!(
        record.starts_with(&st0903::VMTI_LS_UL),
        "record should start with the 16-byte VMTI_LS_UL"
    );
    let after_ul = &record[16..];
    let (declared_len, after_len) = length::read_ber(after_ul).unwrap();
    let inner = &after_len[..declared_len];

    // Decode the inner LS body via klv::st0903::decode.
    let decoded = st0903::decode(inner).unwrap();
    // The substrate-computed checksum is observable post-decode.
    assert!(decoded.checksum.is_some());
    assert_eq!(decoded.precision_time_stamp, Some(1_700_000_000_000_000));
    assert_eq!(decoded.targets.len(), 1);
    assert_eq!(decoded.targets[0].target_id, 42);
    assert_eq!(decoded.targets[0].centroid_pixel, Some(1000));
}

#[test]
fn vmti_ul_constant_matches_misb_st0903_6_section_6_1() {
    // Sanity check the UL bytes — the 16-byte ULs are easy to typo.
    // First 4 bytes are SMPTE 336M-2007 prefix (06 0E 2B 34); remaining
    // 12 bytes encode the MISB ST 0903 designator. Verify the prefix
    // and the well-known last byte (0x06 = VMTI item) per ST 0903.6.
    assert_eq!(
        &st0903::VMTI_LS_UL[..4],
        &[0x06, 0x0E, 0x2B, 0x34],
        "SMPTE 336M-2007 prefix"
    );
    assert_eq!(
        st0903::VMTI_LS_UL[12],
        0x06,
        "VMTI = item 0x06 in the org/dict per ST 0903.6 §6.1"
    );
}

#[test]
fn standalone_ul_with_strict_decode() {
    // Test that the standalone-PID pattern works with decode_strict
    // when the LS body satisfies strict requirements (Tags 4 + 6).
    let vmti = st0903::VmtiLs {
        version_number: Some(6),
        num_targets_reported: Some(0),
        ..Default::default()
    };
    let body = st0903::encode_to_vec(&vmti).unwrap();

    let mut record = Vec::new();
    record.extend_from_slice(&st0903::VMTI_LS_UL);
    let mut len_buf = [0u8; 9];
    let len_n = length::write_ber(body.len(), &mut len_buf).unwrap();
    record.extend_from_slice(&len_buf[..len_n]);
    record.extend_from_slice(&body);

    // Strict-mode dispatch via the standalone pattern.
    assert!(record.starts_with(&st0903::VMTI_LS_UL));
    let after_ul = &record[16..];
    let (declared_len, after_len) = length::read_ber(after_ul).unwrap();
    let inner = &after_len[..declared_len];
    let decoded = st0903::decode_strict(inner).unwrap();
    assert_eq!(decoded.version_number, Some(6));
    assert_eq!(decoded.num_targets_reported, Some(0));
}

// ------------------------------------------------------------------
// The checksum-verifying standalone decoders.
// ------------------------------------------------------------------

/// The jMISB standalone VMTI test vector: system name "DSTO_ADSS_VMTI",
/// version 4, checksum `9F97`, no Item 6.
const JMISB_VECTOR: [u8; 40] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x03, 0x06, 0x00, 0x00, 0x00,
    0x17, 0x03, 0x0E, b'D', b'S', b'T', b'O', b'_', b'A', b'D', b'S', b'S', b'_', b'V', b'M', b'T',
    b'I', 0x04, 0x01, 0x04, 0x01, 0x02, 0x9F, 0x97,
];

/// An MIIS Core Identifier `st1204::decode` accepts.
const CORE_ID: [u8; 34] = [
    0x01, 0x70, 0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F, 0x2E,
    0xB2, 0xDA, 0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A, 0xB9, 0x6A,
    0x36, 0x45,
];

/// `[VMTI_LS_UL][BER length][body]`, as given.
fn packet(body: &[u8]) -> Vec<u8> {
    let mut out = st0903::VMTI_LS_UL.to_vec();
    let mut len_buf = [0u8; 9];
    let n = length::write_ber(body.len(), &mut len_buf).unwrap();
    out.extend_from_slice(&len_buf[..n]);
    out.extend_from_slice(body);
    out
}

/// `packet(body ++ 01 02 <sum>)` with the sum ST 0903.6-119 defines.
fn checksummed(body: &[u8]) -> Vec<u8> {
    let mut with = body.to_vec();
    with.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    let mut out = packet(&with);
    let at = out.len() - 2;
    let sum = checksum_running_sum_16(&out[..at]);
    out[at..].copy_from_slice(&sum.to_be_bytes());
    out
}

/// A record every strict standalone rule accepts.
fn standalone_record() -> st0903::VmtiLs {
    st0903::VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(1),
        frame_width: Some(1920),
        horizontal_fov: Some(45.0),
        vertical_fov: Some(30.0),
        miis_id: Some(CORE_ID.to_vec()),
        targets: vec![st0903::VTargetPack {
            target_id: 1,
            centroid_pixel: Some(100),
            detection_status: Some(1),
            target_location: Some(vec![0x01]),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn decode_standalone_accepts_independent_vector() {
    let ls = st0903::decode_standalone(&JMISB_VECTOR).unwrap();
    assert_eq!(ls.vmti_system_name.as_deref(), Some("DSTO_ADSS_VMTI"));
    assert_eq!(ls.version_number, Some(4));
    assert_eq!(ls.checksum, Some(0x9F97));
    // The vector has no Item 6, which the strict form requires.
    assert!(matches!(
        st0903::decode_standalone_strict(&JMISB_VECTOR),
        Err(KlvDecodeError::St0903MissingRequiredTag { tag: 6 })
    ));
}

#[test]
fn decode_standalone_rejects_bad_checksum() {
    let mut bad = JMISB_VECTOR;
    bad[20] ^= 1;
    assert!(matches!(
        st0903::decode_standalone(&bad),
        Err(KlvDecodeError::ChecksumMismatch {
            expected: 0x9F97,
            ..
        })
    ));
    assert!(matches!(
        st0903::decode_standalone_strict(&bad),
        Err(KlvDecodeError::ChecksumMismatch { .. })
    ));
}

#[test]
fn decode_standalone_rejects_a_missing_checksum() {
    // The body starts after the label and one length byte: offset 17.
    let bytes = packet(&[0x04, 0x01, 0x06, 0x06, 0x01, 0x00]);
    assert!(matches!(
        st0903::decode_standalone(&bytes),
        Err(KlvDecodeError::MissingChecksum { offset: 17 })
    ));
}

#[test]
fn decode_standalone_rejects_a_checksum_that_is_not_last() {
    let bytes = packet(&[0x01, 0x02, 0x00, 0x00, 0x04, 0x01, 0x06]);
    assert!(matches!(
        st0903::decode_standalone(&bytes),
        Err(KlvDecodeError::Tag1NotLast)
    ));
}

#[test]
fn decode_standalone_rejects_a_second_checksum() {
    // A second Tag 1 is reported at its ITEM start, whatever form its
    // length has: body at 17, the first Tag 1 at 23, the second at 27.
    for second in [
        &[0x01, 0x02, 0xCC, 0xCC][..],
        &[0x01, 0x81, 0x02, 0xCC, 0xCC],
    ] {
        let mut body = vec![0x04, 0x01, 0x06, 0x06, 0x01, 0x00, 0x01, 0x02, 0xCC, 0xCC];
        body.extend_from_slice(second);
        assert!(
            matches!(
                st0903::decode_standalone(&packet(&body)),
                Err(KlvDecodeError::DuplicateTag { tag: 1, offset: 27 })
            ),
            "{second:02x?}"
        );
    }
}

#[test]
fn decode_standalone_rejects_a_wrong_width_checksum() {
    // A wrong width is judged before a second Tag 1; the offset is the
    // value's.
    let body = [0x04, 0x01, 0x06, 0x01, 0x01, 0xCC, 0x01, 0x02, 0xCC, 0xCC];
    assert!(matches!(
        st0903::decode_standalone(&packet(&body)),
        Err(KlvDecodeError::MalformedChecksum { offset: 22, got: 1 })
    ));
    // Also when the wrong one comes second: every Tag 1's width is judged.
    let body = [0x04, 0x01, 0x06, 0x01, 0x02, 0xCC, 0xCC, 0x01, 0x01, 0xCC];
    assert!(matches!(
        st0903::decode_standalone(&packet(&body)),
        Err(KlvDecodeError::MalformedChecksum { offset: 26, got: 1 })
    ));
}

/// `packet(items ++ <padding groups> 01 02 <sum>)`: a checksum whose tag is
/// written with `padding` leading `0x80` bytes, and the sum.
fn padded_checksum(items: &[u8], padding: usize) -> (Vec<u8>, u16) {
    let mut body = items.to_vec();
    body.extend(std::iter::repeat_n(0x80, padding));
    body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    let mut bytes = packet(&body);
    let at = bytes.len() - 2;
    let sum = checksum_running_sum_16(&bytes[..at]);
    bytes[at..].copy_from_slice(&sum.to_be_bytes());
    (bytes, sum)
}

#[test]
fn a_padded_checksum_tag_is_the_checksum_for_the_lenient_decode_only() {
    // `80 01` is Tag 1 written with a padding group. The lenient decode
    // reads it as the checksum, as the ST 0601 and RVT decoders do, with
    // the reader the body's decoder uses: the item that is verified is the
    // item that fills `checksum`. Up to five groups are still that tag.
    let items = [0x04, 0x01, 0x06, 0x06, 0x01, 0x00];
    for padding in [1usize, 5] {
        let (bytes, sum) = padded_checksum(&items, padding);
        let ls = st0903::decode_standalone(&bytes)
            .unwrap_or_else(|e| panic!("{padding} padding bytes: {e:?}"));
        assert_eq!(ls.checksum, Some(sum));
        assert_eq!(ls.version_number, Some(6));
        assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
        // It is verified, and counted, as any checksum item.
        let mut wrong = bytes.clone();
        *wrong.last_mut().unwrap() ^= 1;
        assert!(matches!(
            st0903::decode_standalone(&wrong),
            Err(KlvDecodeError::ChecksumMismatch { .. })
        ));
    }
    let (two, _) = padded_checksum(&[0x04, 0x01, 0x06, 0x01, 0x02, 0xCC, 0xCC], 1);
    assert!(matches!(
        st0903::decode_standalone(&two),
        Err(KlvDecodeError::DuplicateTag { tag: 1, offset: 24 })
    ));
    // The strict decode refuses the padded tag at the item's start (the
    // body at 17, the item at 23): a BER-OID never starts with 0x80 (ST
    // 0107.5 §6.3.1), as `st0601::decode_strict_compliance` refuses it.
    for padding in [1usize, 5, 11] {
        let (bytes, _) = padded_checksum(&items, padding);
        assert!(
            matches!(
                st0903::decode_standalone_strict(&bytes),
                Err(KlvDecodeError::NonCanonicalTag { offset: 23 })
            ),
            "{padding} padding bytes: {:?}",
            st0903::decode_standalone_strict(&bytes)
        );
    }
    // A tag of more groups than any decoder of the body reads is no item at
    // all: the lenient decode refuses the set where the tag starts.
    let (bytes, _) = padded_checksum(&items, 11);
    assert!(
        matches!(
            st0903::decode_standalone(&bytes),
            Err(KlvDecodeError::MalformedTag { offset: 23 })
        ),
        "{:?}",
        st0903::decode_standalone(&bytes)
    );
}

#[test]
fn decode_standalone_strict_refuses_a_non_canonical_outer_length() {
    // As `st0601::decode_strict_compliance` does: the outer length in its
    // fewest bytes (ST 0107.5 §6.3.2). The lenient form accepts it.
    let bytes = st0903::encode_to_vec_standalone(&standalone_record()).unwrap();
    assert!(bytes[16] < 0x80, "short form on the wire");
    let mut long = bytes[..16].to_vec();
    long.extend_from_slice(&[0x81, bytes[16]]);
    long.extend_from_slice(&bytes[17..]);
    let at = long.len() - 2;
    let sum = checksum_running_sum_16(&long[..at]);
    long[at..].copy_from_slice(&sum.to_be_bytes());
    let lenient = st0903::decode_standalone(&long).expect("the lenient form reads any BER length");
    assert_eq!(lenient.version_number, Some(6));
    assert!(matches!(
        st0903::decode_standalone_strict(&long),
        Err(KlvDecodeError::NonCanonicalLength { offset: 16 })
    ));
}

#[test]
fn decode_standalone_strict_accepts_our_strict_output() {
    let ls = standalone_record();
    let bytes = st0903::encode_standalone_strict_compliance(&ls).unwrap();
    let decoded = st0903::decode_standalone_strict(&bytes).unwrap();
    assert_eq!(decoded.targets, ls.targets);
    assert_eq!(decoded.miis_id, ls.miis_id);
    assert!(decoded.checksum.is_some());
    assert_eq!(st0903::decode_standalone(&bytes).unwrap(), decoded);
}

#[test]
fn the_standalone_presence_gate_counts_a_special_as_present() {
    // Item 11 = IMAP_ABOVE_MAXIMUM (ST 0903.6 §9.1.1): on the wire with a
    // value, so present, though its field holds no measurement.
    let mut ls = standalone_record();
    ls.horizontal_fov = None;
    let mut body = st0903::encode_to_vec(&ls).unwrap();
    body.extend_from_slice(&[0x0B, 0x02, 0xE1, 0x00]);
    let decoded = st0903::decode_standalone_strict(&checksummed(&body)).unwrap();
    assert_eq!(decoded.horizontal_fov, None);
    // Absent, or zero-length ("unknown"), it is missing.
    let without = st0903::encode_to_vec(&ls).unwrap();
    for bytes in [checksummed(&without), {
        let mut b = without.clone();
        b.extend_from_slice(&[0x0B, 0x00]);
        checksummed(&b)
    }] {
        assert!(matches!(
            st0903::decode_standalone_strict(&bytes),
            Err(KlvDecodeError::St0903MissingRequiredTag { tag: 11 })
        ));
    }
}

#[test]
fn decode_standalone_strict_refuses_an_offset_item() {
    // ST 0903.6-116: standalone carriage omits Items 10, 11, 13 to 16 of
    // a pack, whatever they hold ("Invalid", 0xC1, included).
    type Set = fn(&mut st0903::VTargetPack);
    let items: [(u32, Set); 6] = [
        (10, |t| t.centroid_lat_offset = Some(1.0)),
        (11, |t| t.centroid_lon_offset = Some(1.0)),
        (13, |t| t.bbox_top_left_lat_offset = Some(1.0)),
        (14, |t| t.bbox_top_left_lon_offset = Some(1.0)),
        (15, |t| t.bbox_bottom_right_lat_offset = Some(1.0)),
        (16, |t| t.bbox_bottom_right_lon_offset = Some(1.0)),
    ];
    for (tag, set) in items {
        let mut ls = standalone_record();
        set(&mut ls.targets[0]);
        let bytes = st0903::encode_to_vec_standalone(&ls).unwrap();
        assert!(
            matches!(
                st0903::decode_standalone_strict(&bytes),
                Err(KlvDecodeError::ComplianceViolation { tag: t, .. }) if t == tag
            ),
            "Item {tag} with a value"
        );
        let lenient = st0903::decode_standalone(&bytes).expect("the lenient form has no such rule");
        assert_eq!(lenient.targets[0], ls.targets[0]);

        let mut ls = standalone_record();
        ls.targets.clear();
        ls.num_targets_reported = Some(1);
        let mut body = st0903::encode_to_vec(&ls).unwrap();
        // One pack: targetId 1, Item 4 = 5, the offset item = C1 00 00.
        body.extend_from_slice(&[0x65, 0x0A, 0x09, 0x01, 0x04, 0x01, 0x05, tag as u8, 0x03]);
        body.extend_from_slice(&[0xC1, 0x00, 0x00]);
        assert!(
            matches!(
                st0903::decode_standalone_strict(&checksummed(&body)),
                Err(KlvDecodeError::ComplianceViolation { tag: t, .. }) if t == tag
            ),
            "Item {tag} holding the Invalid special"
        );
    }
}

#[test]
fn decode_standalone_strict_refuses_an_active_target_without_a_location() {
    // ST 0903.6-130.
    for status in [1u8, 3, 4] {
        let mut ls = standalone_record();
        ls.targets[0].detection_status = Some(status);
        ls.targets[0].target_location = None;
        let bytes = st0903::encode_to_vec_standalone(&ls).unwrap();
        assert!(
            matches!(
                st0903::decode_standalone_strict(&bytes),
                Err(KlvDecodeError::ComplianceViolation { tag: 23, .. })
            ),
            "status {status}"
        );
        let lenient = st0903::decode_standalone(&bytes).unwrap();
        assert_eq!(lenient.targets[0].detection_status, Some(status));
    }
}

#[test]
fn decode_standalone_strict_refuses_a_core_identifier_st1204_does_not_accept() {
    let mut ls = standalone_record();
    ls.miis_id = Some(vec![0x01, 0x00]);
    let bytes = st0903::encode_to_vec_standalone(&ls).unwrap();
    assert!(matches!(
        st0903::decode_standalone_strict(&bytes),
        Err(KlvDecodeError::ComplianceViolation { tag: 13, .. })
    ));
}

#[test]
fn the_label_must_be_the_vmti_label() {
    let mut other = JMISB_VECTOR;
    other[15] = 0x01;
    for decode in [st0903::decode_standalone, st0903::decode_standalone_strict] {
        assert!(matches!(
            decode(&other),
            Err(KlvDecodeError::UnexpectedUniversalLabel { .. })
        ));
        assert!(matches!(
            decode(&JMISB_VECTOR[..10]),
            Err(KlvDecodeError::Truncated { offset: 0, .. })
        ));
    }
}

#[test]
fn our_standalone_output_passes_decode_standalone() {
    // The fuzz oracle's claim, on two records: a sparse one and a full one.
    for ls in [st0903::VmtiLs::default(), standalone_record()] {
        let bytes = st0903::encode_to_vec_standalone(&ls).unwrap();
        let decoded = st0903::decode_standalone(&bytes).unwrap();
        assert_eq!(decoded.version_number, ls.version_number);
    }
}
