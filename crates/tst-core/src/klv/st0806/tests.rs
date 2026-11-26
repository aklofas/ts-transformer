//! ST 0806.4 RVT decode tests — hand-built spec-byte fixtures (the spec
//! ships no vectors; each fixture cites the Table 8-1/8-2/8-3/8-4 rows it
//! exercises).

use super::*;
use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};
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

// ST 0806.4 Table 8-1, Items 14/15 and 18/19: the MGRS zone is a UTM zone
// 01 to 60; the band and grid square are three letters, A to Z without I
// and O. A polar (UPS, bands A, B, Y, Z) position has no zone.

fn rvt_with_mgrs(zone: Option<u8>, band_grid: &str) -> RvtLs {
    RvtLs {
        timestamp_us: Some(1_700_000_000_000_000),
        aircraft_mgrs_zone: zone,
        aircraft_mgrs_band_grid: Some(band_grid.into()),
        aircraft_mgrs_easting_m: Some(80_400),
        aircraft_mgrs_northing_m: Some(12_000),
        ..Default::default()
    }
}

#[test]
fn the_rvt_encoder_refuses_an_mgrs_zone_outside_1_to_60() {
    for zone in [0u8, 61, 255] {
        let ls = rvt_with_mgrs(Some(zone), "TWN");
        for (form, r) in [
            ("body", encode_to_vec(&ls)),
            ("standalone", encode_to_vec_standalone(&ls)),
        ] {
            assert!(
                matches!(r, Err(KlvEncodeError::OutOfRange { tag: 14, .. })),
                "zone {zone} {form}: {r:?}"
            );
        }
        let frame_center = RvtLs {
            frame_center_mgrs_zone: Some(zone),
            ..Default::default()
        };
        assert!(
            matches!(
                encode_to_vec(&frame_center),
                Err(KlvEncodeError::OutOfRange { tag: 18, .. })
            ),
            "frame center zone {zone}"
        );
    }
    for zone in [1u8, 60] {
        let bytes = encode_to_vec(&rvt_with_mgrs(Some(zone), "TWN")).unwrap();
        assert_eq!(decode(&bytes).unwrap().aircraft_mgrs_zone, Some(zone));
    }
}

#[test]
fn the_rvt_encoder_refuses_a_band_and_grid_square_that_is_not_three_letters() {
    // A band over its 3-byte cap keeps 0.10.0's StringTooLong (pinned by
    // an_over_long_mgrs_band_keeps_the_error_0_10_0_gave_it).
    for band_grid in ["T", "TW", "tw1", "TIO", "TWO", "Té", "TW1"] {
        let r = encode_to_vec(&rvt_with_mgrs(Some(18), band_grid));
        assert!(
            matches!(r, Err(KlvEncodeError::InvalidFieldValue { tag: 15, .. })),
            "{band_grid:?}: {r:?}"
        );
        let frame_center = RvtLs {
            frame_center_mgrs_band_grid: Some(band_grid.into()),
            ..Default::default()
        };
        let r = encode_to_vec(&frame_center);
        assert!(
            matches!(r, Err(KlvEncodeError::InvalidFieldValue { tag: 19, .. })),
            "frame center {band_grid:?}: {r:?}"
        );
    }
}

#[test]
fn a_polar_position_without_a_zone_is_accepted() {
    let ls = rvt_with_mgrs(None, "AZN");
    let bytes = encode_to_vec(&ls).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.aircraft_mgrs_zone, None);
    assert_eq!(back.aircraft_mgrs_band_grid.as_deref(), Some("AZN"));
    assert_eq!(back.aircraft_mgrs(), None);
}

#[test]
fn mgrs_accessors_return_none_for_a_bad_decoded_component() {
    // The lenient decode keeps what the producer wrote; the accessors build
    // a string only from components the standard allows.
    let item = |zone: u8, band: &[u8; 3]| {
        let mut b = alloc::vec![0x0E, 0x01, zone, 0x0F, 0x03];
        b.extend_from_slice(band);
        b.extend_from_slice(&[0x10, 0x03, 0x01, 0x3A, 0x10, 0x11, 0x03, 0x00, 0x2E, 0xE0]);
        b
    };
    for (zone, band) in [(0u8, b"TWL"), (61, b"TWL"), (18, b"tw1"), (18, b"TIO")] {
        let ls = decode(&item(zone, band)).unwrap();
        assert_eq!(ls.aircraft_mgrs(), None, "zone {zone} band {band:?}");
    }
    let ls = decode(&item(18, b"TWL")).unwrap();
    assert_eq!(ls.aircraft_mgrs().as_deref(), Some("18TWL8040012000"));
    let frame_center = RvtLs {
        frame_center_mgrs_zone: Some(0),
        frame_center_mgrs_band_grid: Some("TWL".into()),
        frame_center_mgrs_easting_m: Some(1),
        frame_center_mgrs_northing_m: Some(2),
        ..Default::default()
    };
    assert_eq!(frame_center.frame_center_mgrs(), None);
}

#[test]
fn a_decoded_bad_mgrs_component_is_refused_on_reencode_and_the_original_bytes_still_decode() {
    // A relay that decodes untrusted RVT and re-encodes gets the refusal for
    // that set; the original bytes still decode, so it can forward them or
    // clear the field and re-encode.
    let original = [
        0x02, 0x08, 0, 6, 0x0A, 0x2D, 0x8A, 0xB4, 0x00, 0x00, 0x0E, 0x01, 0x00,
    ];
    let ls = decode(&original).unwrap();
    assert_eq!(ls.aircraft_mgrs_zone, Some(0));
    assert!(matches!(
        encode_to_vec(&ls),
        Err(KlvEncodeError::OutOfRange { tag: 14, .. })
    ));
    assert_eq!(decode(&original).unwrap(), ls);
    let cleared = RvtLs {
        aircraft_mgrs_zone: None,
        ..ls
    };
    assert!(encode_to_vec(&cleared).is_ok());
}

// ST 0806.4-03 / -04: a standalone RVT set carries one checksum, the last
// item of the set; the CRC-32 covers the set from its key to the CRC's value.

/// A standalone RVT set: the key, the length, `items`, a CRC item computed
/// over everything before its value, then `after`.
fn rvt_set_with_crc_then(items: &[u8], after: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = RVT_LS_UL.0.to_vec();
    let body_len = items.len() + 6 + after.len();
    assert!(body_len < 0x80, "fixture stays in BER short form");
    out.push(body_len as u8);
    out.extend_from_slice(items);
    out.extend_from_slice(&[0x01, 0x04]);
    let crc = crate::mpegts::common::crc32::crc32_mpeg2(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(after);
    out
}

#[test]
fn decode_standalone_accepts_independent_crc_vector() {
    // The jMISB vector: version 4 and the CRC 82 35 32 10.
    let mut v = RVT_LS_UL.0.to_vec();
    v.extend_from_slice(&[0x09, 0x08, 0x01, 0x04, 0x01, 0x04, 0x82, 0x35, 0x32, 0x10]);
    assert_eq!(decode_standalone(&v).unwrap().rvt_ls_version, Some(4));
}

#[test]
fn decode_standalone_rejects_item_after_crc() {
    let timestamp = [0x02, 0x08, 0, 6, 0x0A, 0x2D, 0x8A, 0xB4, 0x00, 0x00];
    let v = rvt_set_with_crc_then(&[0x08, 0x01, 0x04], &timestamp);
    let err = decode_standalone(&v).unwrap_err();
    assert!(matches!(err, KlvDecodeError::Tag1NotLast), "{err:?}");
}

#[test]
fn decode_standalone_rejects_second_crc() {
    // A first CRC item that is wrong, then a right one ending the set: the
    // second is reported at its item's start (16 + 1 + 3 + 6 = 26).
    let v = rvt_set_with_crc_then(&[0x08, 0x01, 0x04, 0x01, 0x04, 0xDE, 0xAD, 0xBE, 0xEF], &[]);
    let err = decode_standalone(&v).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 1, offset: 26 }),
        "{err:?}"
    );
    // The same with the second CRC's length in long form (81 04): its item
    // still starts at 26, three bytes before its value.
    let mut v = RVT_LS_UL.0.to_vec();
    v.extend_from_slice(&[
        16, 0x08, 0x01, 0x04, 0x01, 0x04, 0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x81, 0x04,
    ]);
    let crc = crate::mpegts::common::crc32::crc32_mpeg2(&v);
    v.extend_from_slice(&crc.to_be_bytes());
    let err = decode_standalone(&v).unwrap_err();
    assert!(
        matches!(err, KlvDecodeError::DuplicateTag { tag: 1, offset: 26 }),
        "long form: {err:?}"
    );
}

#[test]
fn decode_standalone_rejects_a_three_byte_crc() {
    // Version 4, then a CRC item of three bytes, its value at 16 + 1 + 3 + 2.
    let mut v = RVT_LS_UL.0.to_vec();
    v.extend_from_slice(&[0x08, 0x08, 0x01, 0x04, 0x01, 0x03, 0xAA, 0xBB, 0xCC]);
    let err = decode_standalone(&v).unwrap_err();
    assert!(
        matches!(
            err,
            KlvDecodeError::MalformedChecksum { offset: 22, got: 3 }
        ),
        "{err:?}"
    );
}

#[test]
fn decode_standalone_without_crc_is_still_accepted() {
    let mut v = RVT_LS_UL.0.to_vec();
    v.extend_from_slice(&[0x03, 0x08, 0x01, 0x04]);
    assert_eq!(decode_standalone(&v).unwrap().rvt_ls_version, Some(4));
}

// ST 0107.4-17: a zero-length item means "unknown". It is an occurrence of
// its tag, so the last-duplicate rule applies to it as to any value.

#[test]
fn rvt_zero_length_scalar_is_unknown() {
    // Tag 3 (true airspeed) zero-length, then Tag 8 (version) 4.
    let ls = decode(&[0x03, 0x00, 0x08, 0x01, 0x04]).unwrap();
    assert_eq!(ls.platform_true_airspeed, None);
    assert_eq!(ls.rvt_ls_version, Some(4));
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
}

#[test]
fn rvt_user_defined_set_without_item_2_is_refused() {
    // ST 0806.4-22: a User Defined LS carries its User Data (Tag 2).
    let ls = decode(&[0x0B, 0x03, 0x01, 0x01, 0x85]).unwrap();
    assert!(ls.user_defined.is_empty());
    assert_eq!(
        ls.field_errors,
        alloc::vec![KlvFieldError::TruncatedField { tag: 11 }]
    );
}

#[test]
fn rvt_user_defined_set_with_present_empty_item_2_decodes() {
    let ls = decode(&[0x0B, 0x05, 0x01, 0x01, 0x85, 0x02, 0x00]).unwrap();
    assert_eq!(
        ls.user_defined,
        alloc::vec![RvtUserData {
            numeric_id_raw: 0x85,
            data: alloc::vec![],
        }]
    );
    assert!(ls.field_errors.is_empty());
}

#[test]
fn a_zero_length_duplicate_after_a_value_unsets_it() {
    let ls = decode(&[0x03, 0x02, 0x00, 0x64, 0x03, 0x00]).unwrap();
    assert_eq!(ls.platform_true_airspeed, None);
    let mut b = alloc::vec![0x02, 0x08];
    b.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    b.extend_from_slice(&[0x02, 0x00]);
    assert_eq!(decode(&b).unwrap().timestamp_us, None);
    let ls = decode(&[0x0A, 0x03, b'm', b'p', b'4', 0x0A, 0x00]).unwrap();
    assert_eq!(ls.digital_video_file_format, None);
    assert!(ls.field_errors.is_empty());
}

#[test]
fn a_zero_length_duplicate_before_a_value_keeps_it() {
    let ls = decode(&[0x03, 0x00, 0x03, 0x02, 0x00, 0x64]).unwrap();
    assert_eq!(ls.platform_true_airspeed, Some(100));
    let ls = decode(&[0x0E, 0x00, 0x0E, 0x01, 18]).unwrap();
    assert_eq!(ls.aircraft_mgrs_zone, Some(18));
}

// ST 0806.4: the text items are ISO-7 (ISO/IEC 646, ASCII).

fn rvt_with_poi_text(text: &str) -> RvtLs {
    RvtLs {
        timestamp_us: Some(1_700_000_000_000_000),
        points_of_interest: alloc::vec![RvtPoi {
            number: Some(7),
            lat_deg: Some(45.0),
            lon_deg: Some(-90.0),
            text: Some(text.into()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn rvt_encoders_refuse_non_ascii_iso7_text() {
    let ls = rvt_with_poi_text("é");
    for (form, r) in [
        ("body", encode_to_vec(&ls)),
        ("standalone", encode_to_vec_standalone(&ls)),
    ] {
        assert!(
            matches!(r, Err(KlvEncodeError::InvalidFieldValue { tag: 6, .. })),
            "{form}: {r:?}"
        );
    }
    let top = RvtLs {
        digital_video_file_format: Some("mp4é".into()),
        ..Default::default()
    };
    assert!(matches!(
        encode_to_vec(&top),
        Err(KlvEncodeError::InvalidFieldValue { tag: 10, .. })
    ));
    let bytes = encode_to_vec(&rvt_with_poi_text("plain")).unwrap();
    assert_eq!(
        decode(&bytes).unwrap().points_of_interest[0]
            .text
            .as_deref(),
        Some("plain")
    );
}

#[test]
fn a_decoded_non_ascii_iso7_text_is_refused_on_reencode_and_the_original_bytes_still_decode() {
    // The lenient decode keeps the UTF-8 a producer wrote; a relay that
    // re-encodes gets the refusal for that set and can forward the original
    // bytes, which still decode, or clear the text.
    let original = [0x0A, 0x02, 0xC3, 0xA9, 0x08, 0x01, 0x04]; // Tag 10 "é", version 4
    let ls = decode(&original).unwrap();
    assert_eq!(ls.digital_video_file_format.as_deref(), Some("é"));
    assert!(matches!(
        encode_to_vec(&ls),
        Err(KlvEncodeError::InvalidFieldValue { tag: 10, .. })
    ));
    assert_eq!(decode(&original).unwrap(), ls);
    let cleared = RvtLs {
        digital_video_file_format: None,
        ..ls
    };
    assert!(encode_to_vec(&cleared).is_ok());
}

#[test]
fn an_over_long_mgrs_band_keeps_the_error_0_10_0_gave_it() {
    // 0.10.0 refused a band and grid square over its 3-byte cap with
    // StringTooLong, whatever the zone; the MGRS checks run after every
    // refusal 0.10.0 made, so that input keeps its error.
    for zone in [Some(18u8), Some(0)] {
        let r = encode_to_vec(&rvt_with_mgrs(zone, "TWLX"));
        assert!(
            matches!(r, Err(KlvEncodeError::StringTooLong { tag: 15, max: 3 })),
            "zone {zone:?}: {r:?}"
        );
    }
    let frame_center = RvtLs {
        frame_center_mgrs_zone: Some(61),
        frame_center_mgrs_band_grid: Some("日本".into()),
        ..Default::default()
    };
    assert!(matches!(
        encode_to_vec(&frame_center),
        Err(KlvEncodeError::StringTooLong { tag: 19, max: 3 })
    ));
    // An easting over 99 999 (refused by 0.10.0) beside a bad zone.
    let ls = RvtLs {
        aircraft_mgrs_easting_m: Some(100_000),
        ..rvt_with_mgrs(Some(0), "TWN")
    };
    assert!(matches!(
        encode_to_vec(&ls),
        Err(KlvEncodeError::OutOfRange { tag: 16, .. })
    ));
}

#[test]
fn a_zero_length_poi_or_aoi_set_adds_no_entry() {
    // A zero-length nested set is "unknown" (ST 0107.4-17), as a zero-length
    // item is: no entry, no field error.
    let mut body = alloc::vec![0x02, 0x08];
    body.extend_from_slice(&1_700_000_000_000_000u64.to_be_bytes());
    body.extend_from_slice(&[0x0C, 0x00, 0x0D, 0x00, 0x0B, 0x00, 0x08, 0x01, 0x04]);
    let ls = decode(&body).unwrap();
    assert!(ls.points_of_interest.is_empty());
    assert!(ls.areas_of_interest.is_empty());
    assert!(ls.user_defined.is_empty());
    assert!(ls.field_errors.is_empty(), "{:?}", ls.field_errors);
    assert_eq!(ls.rvt_ls_version, Some(4));
    // What decodes re-encodes.
    let bytes = encode_to_vec(&ls).unwrap();
    assert_eq!(decode(&bytes).unwrap(), ls);
}

#[test]
fn a_zero_length_item_in_a_poi_or_aoi_set_is_unknown() {
    // ST 0107.4-17 inside a nested set as at the top level: the field is
    // unset and no field error is recorded, for every defined item.
    for tag in 1..=10u8 {
        let ls = decode(&[0x0C, 0x02, tag, 0x00, 0x0D, 0x02, tag, 0x00]).unwrap();
        assert_eq!(ls.points_of_interest, [RvtPoi::default()], "POI item {tag}");
        assert_eq!(ls.areas_of_interest, [RvtAoi::default()], "AOI item {tag}");
        assert!(ls.field_errors.is_empty(), "item {tag}");
    }
    // An unknown item of no bytes is kept as it is.
    let ls = decode(&[0x0C, 0x02, 0x28, 0x00]).unwrap();
    assert_eq!(ls.points_of_interest[0].unknown.len(), 1);
}

#[test]
fn a_zero_length_poi_or_aoi_item_after_a_value_unsets_it() {
    // It is an occurrence of its tag: the value before it does not stay.
    // Number 7, type 1, then both of no bytes; the text stays.
    let set = [
        0x01, 0x02, 0x00, 0x07, 0x05, 0x01, 0x01, 0x06, 0x01, b'A', 0x01, 0x00, 0x05, 0x00,
    ];
    let mut body = alloc::vec![0x0C, set.len() as u8];
    body.extend_from_slice(&set);
    let poi = &decode(&body).unwrap().points_of_interest[0];
    assert_eq!((poi.number, poi.poi_type), (None, None));
    assert_eq!(poi.text.as_deref(), Some("A"));
    assert!(poi.field_errors.is_empty(), "{:?}", poi.field_errors);
    // The same items of an AOI: number (Tag 1) and type (Tag 6).
    let set = [
        0x01, 0x02, 0x00, 0x07, 0x06, 0x01, 0x01, 0x07, 0x01, b'A', 0x01, 0x00, 0x06, 0x00,
    ];
    let mut body = alloc::vec![0x0D, set.len() as u8];
    body.extend_from_slice(&set);
    let aoi = &decode(&body).unwrap().areas_of_interest[0];
    assert_eq!((aoi.number, aoi.aoi_type), (None, None));
    assert_eq!(aoi.text.as_deref(), Some("A"));
    assert!(aoi.field_errors.is_empty(), "{:?}", aoi.field_errors);
    // A latitude sent as the "error" sentinel, then of no bytes: neither a
    // value nor the sentinel is left to write back.
    let ls = decode(&[0x0C, 0x08, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00, 0x02, 0x00]).unwrap();
    assert_eq!(ls.points_of_interest, [RvtPoi::default()]);
    let ls = decode(&[0x0D, 0x08, 0x04, 0x04, 0x80, 0x00, 0x00, 0x00, 0x04, 0x00]).unwrap();
    assert_eq!(ls.areas_of_interest, [RvtAoi::default()]);
}
