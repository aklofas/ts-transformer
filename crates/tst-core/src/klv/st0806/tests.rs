//! ST 0806.4 RVT decode tests — hand-built spec-byte fixtures (the spec
//! ships no vectors; each fixture cites the Table 8-1/8-2/8-3/8-4 rows it
//! exercises).

use super::*;
use crate::klv::pack::OwnedRawField;
use crate::klv::st0806::model::*;

/// Body-form fixture: timestamp + true airspeed + one POI (number/lat/lon).
/// POI lat 45.0° -> round(45/90 * (2^31-1)) = 0x3FFF_FFFF + 1 = 1_073_741_824
/// = 0x4000_0000 (symmetric int32 mapping, ST 0806.4 Table 8-2 Tag 2).
fn body_with_poi() -> alloc::vec::Vec<u8> {
    let mut b = alloc::vec::Vec::new();
    b.extend_from_slice(&[0x02, 0x08]); // Tag 2, len 8
    b.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    b.extend_from_slice(&[0x03, 0x02, 0x00, 0x64]); // Tag 3, len 2, 100 m/s
    // POI LS: number=7 (tag1 u16), lat=45.0 (tag2), lon=-90.0 (tag3 -> 0xC000_0000)
    let mut poi = alloc::vec::Vec::new();
    poi.extend_from_slice(&[0x01, 0x02, 0x00, 0x07]);
    poi.extend_from_slice(&[0x02, 0x04, 0x40, 0x00, 0x00, 0x00]);
    poi.extend_from_slice(&[0x03, 0x04, 0xC0, 0x00, 0x00, 0x00]);
    b.push(0x0C); // Tag 12
    b.push(poi.len() as u8);
    b.extend_from_slice(&poi);
    b
}

/// Build a standalone RVT LS (UL + BER len + Tag 2 timestamp + Tag 1 CRC)
/// with a correctly computed CRC-32/MPEG-2 covering everything up to (not
/// including) the CRC's own 4 value bytes. Exercised directly by
/// `decode_standalone_verifies_crc` below.
fn standalone_with_crc() -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    out.extend_from_slice(&RVT_LS_UL.0);
    let mut body = alloc::vec::Vec::new();
    body.extend_from_slice(&[0x02, 0x08]); // Tag 2, len 8
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x01, 0x04]); // Tag 1 (CRC), len 4 -- value appended below
    // Outer BER length (short form; body so far + 4 CRC bytes fits in 0x7F).
    let declared_len = body.len() + 4;
    assert!(declared_len < 0x80, "fixture must stay in BER short form");
    out.push(declared_len as u8);
    out.extend_from_slice(&body);
    let crc = crate::mpegts::common::crc32::crc32_mpeg2(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

#[test]
fn decode_body_form_scalars_and_poi() {
    let ls = decode(&body_with_poi()).unwrap();
    assert_eq!(ls.timestamp_us, Some(1_700_000_000_000_000));
    assert_eq!(ls.platform_true_airspeed, Some(100));
    assert_eq!(ls.points_of_interest.len(), 1);
    let poi = &ls.points_of_interest[0];
    assert_eq!(poi.number, Some(7));
    assert!((poi.lat_deg.unwrap() - 45.0).abs() < 1e-6);
    assert!((poi.lon_deg.unwrap() + 90.0).abs() < 1e-6);
    assert!(ls.field_errors.is_empty());
}

#[test]
fn decode_repeatable_pois_accumulate() {
    let mut b = body_with_poi();
    // Append a second POI LS (number=8 only — lenient decode keeps partials).
    b.extend_from_slice(&[0x0C, 0x04, 0x01, 0x02, 0x00, 0x08]);
    let ls = decode(&b).unwrap();
    assert_eq!(ls.points_of_interest.len(), 2);
    assert_eq!(ls.points_of_interest[1].number, Some(8));
}

#[test]
fn decode_poi_error_sentinel_recorded() {
    // POI lat = 0x80000000 -> spec "error" sentinel: field None, tag recorded.
    let b = [0x0C, 0x06, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00];
    let ls = decode(&b).unwrap();
    let poi = &ls.points_of_interest[0];
    assert_eq!(poi.lat_deg, None);
    assert_eq!(poi.sentinel_tags, alloc::vec![2]);
}

#[test]
fn decode_mgrs_uint24_and_composite() {
    // Zone 18 / band+grid "TWL" / easting 80400 (0x013A10) / northing 12000 (0x002EE0).
    let b = [
        0x0E, 0x01, 18, //
        0x0F, 0x03, b'T', b'W', b'L', //
        0x10, 0x03, 0x01, 0x3A, 0x10, //
        0x11, 0x03, 0x00, 0x2E, 0xE0,
    ];
    let ls = decode(&b).unwrap();
    assert_eq!(ls.aircraft_mgrs_easting_m, Some(80_400));
    assert_eq!(ls.aircraft_mgrs().as_deref(), Some("18TWL8040012000"));
}

#[test]
fn decode_user_defined_ls_bitfield() {
    // User Defined LS (RVT Tag 11): tag1 = 0b10_000101 (UINT, id 5), tag2 = 2 bytes.
    let b = [0x0B, 0x07, 0x01, 0x01, 0x85, 0x02, 0x02, 0xBE, 0xEF];
    let ls = decode(&b).unwrap();
    let ud = &ls.user_defined[0];
    assert_eq!(ud.data_type(), RvtUserDataType::Uint);
    assert_eq!(ud.numeric_id(), 5);
    assert_eq!(ud.data, alloc::vec![0xBE, 0xEF]);
}

#[test]
fn decode_standalone_verifies_crc() {
    // Assemble UL + BER len + (tag2 timestamp .. tag1 CRC) and corrupt the CRC.
    let good = standalone_with_crc();
    assert!(decode_standalone(&good).is_ok());
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    assert!(matches!(
        decode_standalone(&bad).unwrap_err(),
        crate::error::KlvDecodeError::Crc32Mismatch { .. }
    ));
}

#[test]
fn rvt_tags_ids_unique_and_ascending() {
    // Mirrors the st0601 `tags.rs` schema-invariant tests: RVT_TAGS drives
    // `lookup`'s dispatch, so a duplicate or out-of-order id would silently
    // shadow an earlier entry.
    let mut prev = 0u8;
    for t in super::tags::RVT_TAGS {
        assert!(
            t.id > prev,
            "RVT_TAGS must be strictly ascending by id: tag {} ({}) is not > {prev}",
            t.id,
            t.name
        );
        prev = t.id;
    }
}

#[test]
fn decode_aoi_type_three_is_reserved_poi_type_three_is_target() {
    let poi_b = [0x0C, 0x03, 0x05, 0x01, 0x03];
    let aoi_b = [0x0D, 0x03, 0x06, 0x01, 0x03];
    assert_eq!(
        decode(&poi_b).unwrap().points_of_interest[0].poi_type,
        Some(RvtPoiType::Target)
    );
    assert_eq!(
        decode(&aoi_b).unwrap().areas_of_interest[0].aoi_type,
        Some(RvtAoiType::Reserved)
    );
}

#[test]
fn round_trip_body_form() {
    let mut ls = RvtLs {
        timestamp_us: Some(1_700_000_000_000_000),
        frag_circle_radius_m: Some(250),
        ..RvtLs::default()
    };
    ls.points_of_interest.push(RvtPoi {
        number: Some(7),
        lat_deg: Some(45.0),
        lon_deg: Some(-90.0),
        label: Some("ALPHA".into()),
        ..RvtPoi::default()
    });
    let bytes = encode_to_vec(&ls).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.timestamp_us, ls.timestamp_us);
    assert_eq!(back.frag_circle_radius_m, Some(250));
    assert_eq!(back.points_of_interest[0].number, Some(7));
    assert_eq!(back.points_of_interest[0].label.as_deref(), Some("ALPHA"));
}

#[test]
fn standalone_emits_ul_timestamp_first_crc_last_and_reverifies() {
    let ls = RvtLs {
        timestamp_us: Some(1),
        video_data_rate: Some(2_000_000),
        ..RvtLs::default()
    };
    let bytes = encode_to_vec_standalone(&ls).unwrap();
    assert_eq!(&bytes[..16], &RVT_LS_UL.0);
    let reparsed = decode_standalone(&bytes).unwrap(); // CRC verify is the assertion
    assert_eq!(reparsed.video_data_rate, Some(2_000_000));
    // Tag 2 first / Tag 1 last in the body:
    let (_, after) = crate::klv::length::read_ber(&bytes[16..]).unwrap();
    assert_eq!(after[0], 0x02);
    assert_eq!(after[after.len() - 6], 0x01); // tag1, len4, 4 value bytes
}

#[test]
fn standalone_without_timestamp_is_missing_mandatory() {
    let ls = RvtLs::default();
    assert!(matches!(
        encode_to_vec_standalone(&ls).unwrap_err(),
        crate::error::KlvEncodeError::MissingMandatoryItem { tag: 2, .. }
    ));
}

#[test]
fn poi_missing_latitude_rejected() {
    let mut ls = RvtLs::default();
    ls.points_of_interest.push(RvtPoi {
        number: Some(1),
        lon_deg: Some(0.0),
        ..RvtPoi::default()
    });
    assert!(matches!(
        encode_to_vec(&ls).unwrap_err(),
        crate::error::KlvEncodeError::MissingMandatoryItem { tag: 2, .. } // POI Tag 2 = latitude
    ));
}

#[test]
fn sentinel_error_value_reemits_on_encode() {
    let b = [0x0C, 0x06, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00];
    let mut ls = decode(&b).unwrap();
    // Satisfy POI encode mandatories around the sentinel'd latitude:
    ls.points_of_interest[0].number = Some(1);
    ls.points_of_interest[0].lon_deg = Some(10.0);
    let bytes = encode_to_vec(&ls).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.points_of_interest[0].sentinel_tags, alloc::vec![2]);
}

#[test]
fn unknown_tag_clobbering_timestamp_rejected() {
    // Regression: without a duplicate-tag guard, a caller-stuffed `unknown`
    // entry at a typed tag id silently overwrites the typed field on round-trip
    // with no error. Scenario: Tag 2 (timestamp) is both set via the typed
    // field AND stuffed into `unknown`.
    let ls = RvtLs {
        timestamp_us: Some(1),
        unknown: alloc::vec![OwnedRawField {
            tag: 2,
            value: alloc::vec![0, 0, 0, 0, 0, 0, 0, 99],
        }],
        ..RvtLs::default()
    };
    assert!(matches!(
        encode_to_vec(&ls).unwrap_err(),
        crate::error::KlvEncodeError::ReservedTagInUnknown { tag: 2 }
    ));
}

#[test]
fn poi_unknown_tag_clobbering_number_rejected() {
    // Same clobber class as `unknown_tag_clobbering_timestamp_rejected`,
    // one layer down: POI/AOI have no `tags.rs` table of their own, so
    // the guard there is a literal 1..=10 range check rather than
    // `is_typed_tag`.
    let mut ls = RvtLs::default();
    ls.points_of_interest.push(RvtPoi {
        number: Some(7),
        lat_deg: Some(10.0),
        lon_deg: Some(20.0),
        unknown: alloc::vec![OwnedRawField {
            tag: 1,
            value: alloc::vec![0x00, 0x63],
        }],
        ..RvtPoi::default()
    });
    assert!(matches!(
        encode_to_vec(&ls).unwrap_err(),
        crate::error::KlvEncodeError::ReservedTagInUnknown { tag: 1 }
    ));
}

#[test]
fn aoi_unknown_tag_clobbering_type_rejected() {
    let mut ls = RvtLs::default();
    ls.areas_of_interest.push(RvtAoi {
        number: Some(1),
        corner_lat_p1_deg: Some(1.0),
        corner_lon_p1_deg: Some(2.0),
        corner_lat_p3_deg: Some(3.0),
        corner_lon_p3_deg: Some(4.0),
        aoi_type: Some(RvtAoiType::Friendly),
        unknown: alloc::vec![OwnedRawField {
            tag: 6,
            value: alloc::vec![0x02],
        }],
        ..RvtAoi::default()
    });
    assert!(matches!(
        encode_to_vec(&ls).unwrap_err(),
        crate::error::KlvEncodeError::ReservedTagInUnknown { tag: 6 }
    ));
}

#[test]
fn unknown_fields_pass_through_when_not_typed() {
    // A genuinely unrecognized tag (200 is outside both the top-level
    // 1..=21 table and the POI/AOI 1..=10 range) must still pass through
    // verbatim and round-trip — the new guard must not reject tags it
    // doesn't own.
    let mut ls = RvtLs {
        timestamp_us: Some(1),
        unknown: alloc::vec![OwnedRawField {
            tag: 200,
            value: alloc::vec![0xAA, 0xBB],
        }],
        ..RvtLs::default()
    };
    ls.points_of_interest.push(RvtPoi {
        number: Some(1),
        lat_deg: Some(10.0),
        lon_deg: Some(20.0),
        unknown: alloc::vec![OwnedRawField {
            tag: 200,
            value: alloc::vec![0xCC],
        }],
        ..RvtPoi::default()
    });
    ls.areas_of_interest.push(RvtAoi {
        number: Some(2),
        corner_lat_p1_deg: Some(1.0),
        corner_lon_p1_deg: Some(2.0),
        corner_lat_p3_deg: Some(3.0),
        corner_lon_p3_deg: Some(4.0),
        aoi_type: Some(RvtAoiType::Friendly),
        unknown: alloc::vec![OwnedRawField {
            tag: 200,
            value: alloc::vec![0xDD],
        }],
        ..RvtAoi::default()
    });

    let bytes = encode_to_vec(&ls).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.unknown.len(), 1);
    assert_eq!(back.unknown[0].tag, 200);
    assert_eq!(back.unknown[0].value, alloc::vec![0xAA, 0xBB]);
    assert_eq!(back.points_of_interest[0].unknown.len(), 1);
    assert_eq!(back.points_of_interest[0].unknown[0].tag, 200);
    assert_eq!(
        back.points_of_interest[0].unknown[0].value,
        alloc::vec![0xCC]
    );
    assert_eq!(back.areas_of_interest[0].unknown.len(), 1);
    assert_eq!(back.areas_of_interest[0].unknown[0].tag, 200);
    assert_eq!(
        back.areas_of_interest[0].unknown[0].value,
        alloc::vec![0xDD]
    );
}

#[test]
fn rvt_uls_are_pairwise_distinct_and_pinned_at_byte_12() {
    // The manifest [[surface]] entries for
    // RVT_POI_LS_UL/RVT_AOI_LS_UL/RVT_USER_DEFINED_LS_UL cite this file as
    // their owning test; this test names those constants AND pins the four
    // wire constants.
    let uls = [
        (RVT_LS_UL, 0x02u8),
        (RVT_POI_LS_UL, 0x0C),
        (RVT_AOI_LS_UL, 0x0D),
        (RVT_USER_DEFINED_LS_UL, 0x0F),
    ];
    for (ul, expected_byte_12) in uls {
        assert_eq!(ul.0[12], expected_byte_12);
    }
    for i in 0..uls.len() {
        for j in (i + 1)..uls.len() {
            assert_ne!(
                uls[i].0, uls[j].0,
                "UL {i} and {j} must be pairwise distinct"
            );
        }
    }
}
