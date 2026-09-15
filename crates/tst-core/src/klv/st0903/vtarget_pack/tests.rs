use super::decode::{read_pack, read_pack_strict};
use super::encode::{encoded_len, write_pack};
use super::model::{PACK_TAGS, VTargetPack, VTargetPackError, pack_lookup};
use crate::error::KlvEncodeError;
use crate::error::KlvFieldError;
use crate::klv::pack::OwnedRawField;

#[test]
fn pack_tags_table_has_unique_ids() {
    let mut ids: Vec<u8> = PACK_TAGS.iter().map(|t| t.id).collect();
    ids.sort();
    let len_before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "duplicate pack tag IDs");
}

#[test]
fn pack_tags_lookup_round_trips() {
    for tag in PACK_TAGS {
        assert_eq!(pack_lookup(tag.id), Some(tag));
    }
    assert_eq!(pack_lookup(0), None);
    assert_eq!(pack_lookup(255), None);
    // Deprecated tags per ST 0903.6 §10.2.2.22, §10.2.2.26,
    // §10.2.2.27 — intentionally absent from the table; lenient
    // decoders must treat any wire occurrence as an unknown tag
    // (preserved per ST 0107.5 §6).
    assert_eq!(pack_lookup(21), None);
    assert_eq!(pack_lookup(102), None);
    assert_eq!(pack_lookup(103), None);
}

#[test]
fn empty_pack_round_trips() {
    let pack = VTargetPack {
        target_id: 1,
        ..Default::default()
    };
    let mut bytes = Vec::new();
    let written = write_pack(&pack, &mut bytes).unwrap();
    assert_eq!(written, bytes.len());
    assert_eq!(written, encoded_len(&pack));
    let (decoded, consumed) = read_pack(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded.target_id, 1);
    // Default fields should round-trip as None / empty.
    assert!(decoded.centroid_pixel.is_none());
    assert!(decoded.target_color.is_none());
    assert!(decoded.unknown.is_empty());
    assert!(decoded.field_errors.is_empty());
}

#[test]
fn populated_pack_round_trips() {
    let pack = VTargetPack {
        target_id: 42,
        centroid_pixel: Some(8_294_400),
        bbox_top_left_pixel: Some(8_293_000),
        bbox_bottom_right_pixel: Some(8_295_800),
        priority: Some(1),
        confidence_level: Some(95),
        history: Some(0),
        percentage_of_target_pixels: Some(60),
        target_color: Some([0xFF, 0x80, 0x40]),
        target_intensity: Some(220),
        centroid_lat_offset: Some(0.001234),
        centroid_lon_offset: Some(-0.005678),
        centroid_hae: Some(150.0),
        bbox_top_left_lat_offset: Some(0.000123),
        bbox_top_left_lon_offset: Some(-0.000456),
        bbox_bottom_right_lat_offset: Some(0.000789),
        bbox_bottom_right_lon_offset: Some(-0.001234),
        target_location: Some(vec![0xAA, 0xBB]),
        geospatial_contour_series: Some(vec![0xCC, 0xDD]),
        centroid_pix_row: Some(1080),
        centroid_pix_col: Some(1920),
        algorithm_id: Some(7),
        detection_status: Some(1),
        vmask: Some(vec![0xDE, 0xAD]),
        vtracker: Some(vec![0x42]),
        vchip: None,
        vchip_series: Some(vec![0x01, 0x02]),
        vobject_series: Some(vec![0x03, 0x04]),
        unknown: vec![],
        field_errors: vec![],
    };
    let bytes = {
        let mut b = Vec::new();
        write_pack(&pack, &mut b).unwrap();
        b
    };
    let (decoded, _consumed) = read_pack(&bytes).unwrap();
    assert_eq!(decoded.target_id, 42);
    assert_eq!(decoded.centroid_pixel, Some(8_294_400));
    assert_eq!(decoded.priority, Some(1));
    assert_eq!(decoded.target_color, Some([0xFF, 0x80, 0x40]));
    assert_eq!(decoded.vmask, Some(vec![0xDE, 0xAD]));
    assert_eq!(decoded.detection_status, Some(1));
    assert_eq!(decoded.algorithm_id, Some(7));
    assert!((decoded.centroid_lat_offset.unwrap() - 0.001234).abs() < 1e-5);
    assert!((decoded.centroid_lon_offset.unwrap() - (-0.005678)).abs() < 1e-5);
    assert!((decoded.bbox_top_left_lat_offset.unwrap() - 0.000123).abs() < 1e-5);
    assert!((decoded.bbox_bottom_right_lon_offset.unwrap() - (-0.001234)).abs() < 1e-5);
}

#[test]
fn target_id_multibyte_round_trips() {
    // Target ID = 200 fits in 2 BER-OID bytes (0x81 0x48).
    let pack = VTargetPack {
        target_id: 200,
        ..Default::default()
    };
    let mut bytes = Vec::new();
    write_pack(&pack, &mut bytes).unwrap();
    assert_eq!(bytes[0], 0x81);
    assert_eq!(bytes[1], 0x48);
    let (decoded, _) = read_pack(&bytes).unwrap();
    assert_eq!(decoded.target_id, 200);
}

#[test]
fn truncated_target_id_rejected() {
    // 0x81 alone signals "more bytes follow" but buffer is empty.
    let bytes = [0x81u8];
    let err = read_pack(&bytes).unwrap_err();
    assert!(matches!(err, VTargetPackError::TruncatedTargetId));
}

#[test]
fn truncated_field_value_rejected() {
    // Target ID = 1 (1 byte 0x01), then tag 6 (Target History)
    // declares length 2 but provides 1 byte.
    let bytes = [0x01, 6, 2, 0xFF];
    let err = read_pack(&bytes).unwrap_err();
    assert!(matches!(
        err,
        VTargetPackError::LengthOverrun { tag: 6, .. }
    ));
}

#[test]
fn lenient_read_pack_records_field_error_and_keeps_the_pack() {
    // CORR-29(e): target_id 1, then tag 10 (centroid_lat_offset, 3-byte
    // IMAPB) with a 1-byte value — a per-field error, not a framing
    // error. Lenient decode keeps the pack and records the error.
    let bytes = [0x01, 0x0A, 0x01, 0x00];
    let (pack, consumed) = read_pack(&bytes).expect("per-field errors are recoverable");
    assert_eq!(consumed, 4);
    assert_eq!(pack.target_id, 1);
    assert_eq!(pack.centroid_lat_offset, None);
    assert_eq!(
        pack.field_errors,
        vec![KlvFieldError::InvalidLength {
            tag: 10,
            expected: 3,
            got: 1
        }]
    );
}

#[test]
fn strict_read_pack_still_rejects_a_field_error() {
    let bytes = [0x01, 0x0A, 0x01, 0x00];
    assert!(matches!(
        read_pack_strict(&bytes).unwrap_err(),
        VTargetPackError::MalformedImapb { tag: 10 }
    ));
}

#[test]
fn lenient_read_pack_still_rejects_framing_errors() {
    // LengthOverrun is framing, not a field value — stays fatal.
    let bytes = [0x01, 6, 2, 0xFF];
    assert!(matches!(
        read_pack(&bytes).unwrap_err(),
        VTargetPackError::LengthOverrun { tag: 6, .. }
    ));
}

#[test]
fn unknown_tags_preserved() {
    // Build by hand: target_id=1, then unknown tag 200 with 3 bytes
    // [0xAA 0xBB 0xCC]. Tag 200 is BER-OID-encoded as 0x81 0x48 per
    // ST 0107.5 §6.3.1 (post-E5-followup the body walker reads tags
    // as BER-OID, so multi-byte tag IDs need their proper encoding
    // on the wire — a raw 0xC8 byte would be parsed as a continuation
    // prefix and misframe the BER length).
    let bytes = [0x01u8, 0x81, 0x48, 3, 0xAA, 0xBB, 0xCC];
    let (decoded, _) = read_pack(&bytes).unwrap();
    assert_eq!(decoded.target_id, 1);
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 200);
    assert_eq!(decoded.unknown[0].value, vec![0xAA, 0xBB, 0xCC]);
}

#[test]
fn deprecated_tag_preserved_as_unknown() {
    // Per ST 0107.5 §6, deprecated tag IDs (e.g., 21) should round-
    // trip as unknown bytes — the decoder doesn't reject them, just
    // treats them as opaque.
    let bytes = [0x01u8, 21, 2, 0xDE, 0xAD];
    let (decoded, _) = read_pack(&bytes).unwrap();
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 21);
    assert_eq!(decoded.unknown[0].value, vec![0xDE, 0xAD]);
}

/// Locks in the canonical wire format of a known VTargetPack.
/// Catches accidental field-order changes in `write_pack` (which
/// round-trip tests miss because `read_pack` is order-agnostic) and
/// catches `encoded_len` drift relative to `write_pack`.
#[test]
fn write_pack_canonical_byte_layout() {
    let pack = VTargetPack {
        target_id: 7,
        centroid_pixel: Some(0x1234), // Tag 1, V6 → 2 bytes [0x12, 0x34]
        priority: Some(2),            // Tag 4, U8
        confidence_level: Some(95),   // Tag 5, U8
        target_color: Some([0xAA, 0xBB, 0xCC]), // Tag 8, 3-byte RGB
        detection_status: Some(1),    // Tag 23, U8
        vmask: Some(vec![0xDE, 0xAD]), // Tag 101
        ..Default::default()
    };

    let mut bytes = Vec::new();
    let written = write_pack(&pack, &mut bytes).unwrap();

    // Expected wire form (BER-OID Target ID + ascending-tag TLVs):
    let expected: Vec<u8> = vec![
        0x07, // BER-OID Target ID = 7
        // Tag 1, len 2, value [0x12, 0x34] (centroid_pixel)
        0x01, 0x02, 0x12, 0x34, // Tag 4, len 1, value [0x02] (priority)
        0x04, 0x01, 0x02, // Tag 5, len 1, value [0x5F] (confidence_level = 95 = 0x5F)
        0x05, 0x01, 0x5F, // Tag 8, len 3, value [0xAA, 0xBB, 0xCC] (target_color)
        0x08, 0x03, 0xAA, 0xBB, 0xCC, // Tag 23, len 1, value [0x01] (detection_status)
        0x17, 0x01, 0x01, // Tag 101, len 2, value [0xDE, 0xAD] (vmask)
        0x65, 0x02, 0xDE, 0xAD,
    ];
    assert_eq!(
        bytes, expected,
        "write_pack produced unexpected byte layout — \
        either field-order changed or a TLV got bogus bytes"
    );

    assert_eq!(written, bytes.len());
    assert_eq!(
        written,
        encoded_len(&pack),
        "write_pack length disagrees with encoded_len — drift between the two functions"
    );
}

#[test]
fn round_trip_with_unknown_preserved() {
    let mut pack = VTargetPack {
        target_id: 5,
        priority: Some(3),
        ..Default::default()
    };
    pack.unknown.push(OwnedRawField {
        tag: 200,
        value: vec![0xFF, 0xEE],
    });
    let mut bytes = Vec::new();
    write_pack(&pack, &mut bytes).unwrap();
    let (decoded, _) = read_pack(&bytes).unwrap();
    assert_eq!(decoded.target_id, 5);
    assert_eq!(decoded.priority, Some(3));
    assert_eq!(decoded.unknown.len(), 1);
    assert_eq!(decoded.unknown[0].tag, 200);
}

/// E5 follow-up: the LS body walker reads tags as BER-OID per
/// ST 0107.5 §6.3.1, so a future ST 0903.7+ pack tag ≥ 128 (which
/// encodes as multi-byte BER-OID) survives the inner walk. Pre-fix,
/// the walker read tags as `cursor[0]` and would have misframed the
/// first continuation byte (0x81 0x..) as the start of the BER length.
///
/// Verifies the two BER-OID size boundaries that matter for forward-
/// compat: 128 (smallest multi-byte: 0x81 0x00) and 16384 (smallest
/// three-byte: 0x81 0x80 0x00). Tags 127 and 16383 stay one-/two-byte
/// respectively but bracket the boundary symmetrically.
#[test]
fn unknown_tag_multibyte_ber_oid_round_trips() {
    for tag in [127u32, 128, 16383, 16384] {
        let mut pack = VTargetPack {
            target_id: 1,
            ..Default::default()
        };
        pack.unknown.push(OwnedRawField {
            tag,
            value: vec![0xAA, 0xBB, 0xCC],
        });
        let mut bytes = Vec::new();
        write_pack(&pack, &mut bytes).unwrap();
        let (decoded, _) = read_pack(&bytes).unwrap();
        assert_eq!(
            decoded.unknown.len(),
            1,
            "tag {tag}: unknown count mismatch"
        );
        assert_eq!(
            decoded.unknown[0].tag, tag,
            "tag {tag}: round-trip lost tag ID"
        );
        assert_eq!(
            decoded.unknown[0].value,
            vec![0xAA, 0xBB, 0xCC],
            "tag {tag}: value bytes corrupted"
        );
    }
}

/// E5 follow-up: BER-OID-encoded tag IDs ≤ 127 are byte-identical to
/// the pre-fix raw single-byte tags. Every §10.2 typed pack tag fits
/// in this range (highest is 107), so legacy wire bytes encode bit-
/// for-bit unchanged. This regression-pins the backward-compat
/// guarantee for the §10.2.2 typed dispatch tags (4 U8 tags spot-
/// checked: 4 priority, 5 confidence, 7 percentage, 23 detection;
/// plus the BER-OID Target ID = 7 leading byte).
#[test]
fn defined_pack_tags_byte_identical_pre_and_post_e5_followup() {
    let pack = VTargetPack {
        target_id: 7,
        priority: Some(2),
        confidence_level: Some(95),
        percentage_of_target_pixels: Some(60),
        detection_status: Some(1),
        ..Default::default()
    };
    let mut bytes = Vec::new();
    write_pack(&pack, &mut bytes).unwrap();
    // Same exact layout the canonical-bytes test would emit pre-E5
    // (single-byte tags, BER length, value): a hand-built reference
    // catches drift if a future change accidentally widened the
    // BER-OID emit path for tags ≤ 127.
    let expected: Vec<u8> = vec![
        0x07, // BER-OID Target ID = 7 (1 byte, value < 128)
        0x04, 0x01, 0x02, // Tag 4 priority
        0x05, 0x01, 0x5F, // Tag 5 confidence (0x5F = 95)
        0x07, 0x01, 0x3C, // Tag 7 percentage (0x3C = 60)
        0x17, 0x01, 0x01, // Tag 23 detection_status
    ];
    assert_eq!(
        bytes, expected,
        "single-byte BER-OID emit drifted from pre-fix byte layout for typed tags"
    );
}

// -------- REF-KLV-04b: u64 field round-trip tests --------

#[test]
fn vtarget_pack_target_id_above_u32_round_trips() {
    let mut p = VTargetPack {
        target_id: (u32::MAX as u64) + 12345,
        ..Default::default()
    };
    p.centroid_pixel = Some(1); // a pack needs >= 1 TLV item beyond targetId
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.target_id, (u32::MAX as u64) + 12345);
}

#[test]
fn vtarget_pack_pixel_above_u32_round_trips() {
    let big_pixel = (u32::MAX as u64) + 1; // needs a 5-byte var-uint
    let p = VTargetPack {
        target_id: 1,
        centroid_pixel: Some(big_pixel),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, _) = read_pack(&buf).unwrap();
    assert_eq!(decoded.centroid_pixel, Some(big_pixel));
}

// -------- DA-KLVC-1: wire-width cap enforcement --------

const V3_MAX: u64 = (1u64 << 24) - 1; // 16_777_215
const V4_MAX: u64 = u32::MAX as u64; // 4_294_967_295
const V6_MAX: u64 = (1u64 << 48) - 1; // 281_474_976_710_655

// V6 cap (tags 1, 2, 3): centroid_pixel / bbox corners

#[test]
fn centroid_pixel_over_v6_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pixel: Some(V6_MAX + 1),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::OutOfRange { tag: 1, .. }),
        "expected OutOfRange tag 1, got {err:?}"
    );
}

#[test]
fn centroid_pixel_at_v6_cap_round_trips() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pixel: Some(V6_MAX),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.centroid_pixel, Some(V6_MAX));
}

#[test]
fn bbox_top_left_pixel_over_v6_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        bbox_top_left_pixel: Some(V6_MAX + 1),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 2, .. }));
}

#[test]
fn bbox_bottom_right_pixel_over_v6_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        bbox_bottom_right_pixel: Some(V6_MAX + 1),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 3, .. }));
}

#[test]
fn bbox_pixels_at_v6_cap_round_trip() {
    // Encode→decode must be a fixpoint AT the cap for both bbox tags.
    let p = VTargetPack {
        target_id: 1,
        bbox_top_left_pixel: Some(V6_MAX),
        bbox_bottom_right_pixel: Some(V6_MAX),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.bbox_top_left_pixel, Some(V6_MAX));
    assert_eq!(decoded.bbox_bottom_right_pixel, Some(V6_MAX));
}

// V3 cap (tag 9: target_intensity, tag 22: algorithm_id)

#[test]
fn target_intensity_over_v3_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        target_intensity: Some((V3_MAX + 1) as u32),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::OutOfRange { tag: 9, .. }),
        "expected OutOfRange tag 9, got {err:?}"
    );
}

#[test]
fn target_intensity_at_v3_cap_round_trips() {
    let p = VTargetPack {
        target_id: 1,
        target_intensity: Some(V3_MAX as u32),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.target_intensity, Some(V3_MAX as u32));
}

#[test]
fn algorithm_id_over_v3_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        algorithm_id: Some((V3_MAX + 1) as u32),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 22, .. }));
}

#[test]
fn algorithm_id_at_v3_cap_round_trips() {
    let p = VTargetPack {
        target_id: 1,
        algorithm_id: Some(V3_MAX as u32),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.algorithm_id, Some(V3_MAX as u32));
}

// V4 cap (tags 19, 20: centroid_pix_row / centroid_pix_col)

#[test]
fn centroid_pix_row_over_v4_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pix_row: Some(V4_MAX + 1),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::OutOfRange { tag: 19, .. }),
        "expected OutOfRange tag 19, got {err:?}"
    );
}

#[test]
fn centroid_pix_row_at_v4_cap_round_trips() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pix_row: Some(V4_MAX),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.centroid_pix_row, Some(V4_MAX));
}

#[test]
fn centroid_pix_col_over_v4_cap_rejects() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pix_col: Some(V4_MAX + 1),
        ..Default::default()
    };
    let err = write_pack(&p, &mut Vec::new()).unwrap_err();
    assert!(matches!(err, KlvEncodeError::OutOfRange { tag: 20, .. }));
}

#[test]
fn centroid_pix_col_at_v4_cap_round_trips() {
    let p = VTargetPack {
        target_id: 1,
        centroid_pix_col: Some(V4_MAX),
        ..Default::default()
    };
    let mut buf = Vec::new();
    write_pack(&p, &mut buf).unwrap();
    let (decoded, n) = read_pack(&buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(decoded.centroid_pix_col, Some(V4_MAX));
}

// -------- Strictwire follow-up: completeness pinning test --------
//
// All u64-typed VTargetPack fields (enumerated by grep for `u64` in
// vtarget_pack/model.rs):
//
//   target_id             — BER-OID var-len, lossless (10 bytes ≥ u64::MAX)
//   centroid_pixel        — V6_MAX cap, OutOfRange above cap
//   bbox_top_left_pixel   — V6_MAX cap, OutOfRange above cap
//   bbox_bottom_right_pixel — V6_MAX cap, OutOfRange above cap
//   centroid_pix_row      — V4_MAX cap, OutOfRange above cap
//   centroid_pix_col      — V4_MAX cap, OutOfRange above cap
//
// The default (lenient) encode path in write_pack enforces all caps, so
// both lenient and strict paths are covered — encode_strict_compliance
// calls encode_to_vec which calls write_pack. The test below also has an
// explicit smoke-call through the strict entry point to confirm this.
//
// VmtiLs::precision_time_stamp (u64) is trivially lossless (8-byte
// big-endian raw bytes); see `precision_time_stamp_u64_max_round_trips`
// in st0903/tests.rs for the companion encode round-trip probe.

/// Pins the invariant: every u64-typed VTargetPack field either
/// encodes losslessly (target_id via BER-OID) or returns a structured
/// OutOfRange error when the value exceeds its wire cap — no field
/// silently truncates at u64::MAX.
///
/// This is the PT-KLV-strictwire follow-up closed as subsumed by
/// WP10/DA-KLVC-1: the caps are verified complete for all u64 fields.
#[test]
fn u64_typed_fields_never_silently_truncate_on_encode() {
    use super::decode::read_pack;
    use super::encode::write_pack;

    // ── target_id at u64::MAX: BER-OID lossless ──────────────────────
    // write_ber_oid_u64(u64::MAX) needs exactly 10 bytes (ceil(64/7));
    // the buf in write_pack is [0u8; 10] — fits exactly.
    {
        let p = VTargetPack {
            target_id: u64::MAX,
            centroid_pixel: Some(1), // ≥1 TLV item required beyond targetId
            ..Default::default()
        };
        let mut buf = Vec::new();
        write_pack(&p, &mut buf).unwrap();
        let (decoded, n) = read_pack(&buf).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(
            decoded.target_id,
            u64::MAX,
            "target_id u64::MAX must round-trip losslessly via BER-OID"
        );
    }

    // ── V6 cap fields (tags 1, 2, 3): OutOfRange at u64::MAX ─────────
    let v6_cases: &[(&str, VTargetPack)] = &[
        (
            "centroid_pixel (tag 1)",
            VTargetPack {
                target_id: 1,
                centroid_pixel: Some(u64::MAX),
                ..Default::default()
            },
        ),
        (
            "bbox_top_left_pixel (tag 2)",
            VTargetPack {
                target_id: 1,
                bbox_top_left_pixel: Some(u64::MAX),
                ..Default::default()
            },
        ),
        (
            "bbox_bottom_right_pixel (tag 3)",
            VTargetPack {
                target_id: 1,
                bbox_bottom_right_pixel: Some(u64::MAX),
                ..Default::default()
            },
        ),
    ];
    for (name, pack) in v6_cases {
        let err = write_pack(pack, &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::OutOfRange { .. }),
            "{name} at u64::MAX: expected OutOfRange, got {err:?}"
        );
    }

    // ── V4 cap fields (tags 19, 20): OutOfRange at u64::MAX ──────────
    let v4_cases: &[(&str, VTargetPack)] = &[
        (
            "centroid_pix_row (tag 19)",
            VTargetPack {
                target_id: 1,
                centroid_pix_row: Some(u64::MAX),
                ..Default::default()
            },
        ),
        (
            "centroid_pix_col (tag 20)",
            VTargetPack {
                target_id: 1,
                centroid_pix_col: Some(u64::MAX),
                ..Default::default()
            },
        ),
    ];
    for (name, pack) in v4_cases {
        let err = write_pack(pack, &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::OutOfRange { .. }),
            "{name} at u64::MAX: expected OutOfRange, got {err:?}"
        );
    }

    // ── Strict path smoke-call: encode_strict_compliance delegates to
    // encode_to_vec → write_pack, so the same OutOfRange fires through it. ──
    {
        use crate::klv::st0903::{VmtiLs, encode_strict_compliance};
        let ls = VmtiLs {
            version_number: Some(6),
            num_targets_reported: Some(1),
            targets: vec![VTargetPack {
                target_id: 1,
                centroid_pixel: Some(V6_MAX + 1),
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = encode_strict_compliance(&ls).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::OutOfRange { tag: 1, .. }),
            "strict path: expected OutOfRange tag 1 for over-cap centroid_pixel, got {err:?}"
        );
    }
}
