"""ST 0806.4 RVT (Remote Video Terminal) Local Set — Python wrap tests.

Byte fixtures are ported verbatim from the Rust `crates/tst-core/src/
klv/st0806/tests.rs` hand-built fixtures (the spec ships no vectors of
its own), so the expected values here are already proven correct at
the Rust layer."""

import pytest

from tstrans.exceptions import KlvEncodeError, KlvError, KlvErrorKind
from tstrans.klv import (
    RvtAoi,
    RvtAoiType,
    RvtLs,
    RvtPoi,
    RvtPoiType,
    RvtUserData,
    RvtUserDataType,
    decode_rvt,
    decode_rvt_standalone,
    encode_rvt,
    encode_rvt_standalone,
)

RVT_LS_UL = bytes.fromhex("060e2b34020b01010e01030102000000")


def _body_with_poi() -> bytes:
    """Timestamp + true airspeed + one POI (number=7, lat=45.0, lon=-90.0).

    POI lat 45.0 -> round(45/90 * (2**31-1)) + 1 = 0x4000_0000 (symmetric
    int32 mapping, ST 0806.4 Table 8-2 Tag 2); lon -90.0 -> 0xC000_0000."""
    poi = bytes([0x01, 0x02, 0x00, 0x07]) + bytes([0x02, 0x04, 0x40, 0x00, 0x00, 0x00]) + bytes(
        [0x03, 0x04, 0xC0, 0x00, 0x00, 0x00]
    )
    return (
        bytes([0x02, 0x08])
        + (1_700_000_000_000_000).to_bytes(8, "big")
        + bytes([0x03, 0x02, 0x00, 0x64])  # Tag 3, len 2, 100 m/s
        + bytes([0x0C, len(poi)])
        + poi
    )


# ---------------------------------------------------------------------------
# decode_rvt (body form)
# ---------------------------------------------------------------------------


def test_decode_rvt_body_scalars_and_poi():
    ls = decode_rvt(_body_with_poi())
    assert isinstance(ls, RvtLs)
    assert ls.timestamp_us == 1_700_000_000_000_000
    assert ls.platform_true_airspeed == 100
    assert len(ls.points_of_interest) == 1
    poi = ls.points_of_interest[0]
    assert poi.number == 7
    assert abs(poi.lat_deg - 45.0) < 1e-6
    assert abs(poi.lon_deg + 90.0) < 1e-6
    assert ls.field_errors == ()


def test_decode_rvt_repeatable_pois_accumulate():
    b = _body_with_poi() + bytes([0x0C, 0x04, 0x01, 0x02, 0x00, 0x08])
    ls = decode_rvt(b)
    assert len(ls.points_of_interest) == 2
    assert ls.points_of_interest[1].number == 8


def test_decode_rvt_poi_error_sentinel_recorded():
    # POI lat = 0x80000000 -> spec "error" sentinel: field None, tag recorded.
    b = bytes([0x0C, 0x06, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00])
    ls = decode_rvt(b)
    poi = ls.points_of_interest[0]
    assert poi.lat_deg is None
    assert poi.sentinel_tags == (2,)


def test_decode_rvt_mgrs_uint24_and_composite():
    # Zone 18 / band+grid "TWL" / easting 80400 (0x013A10) / northing 12000 (0x002EE0).
    b = bytes(
        [
            0x0E, 0x01, 18,
            0x0F, 0x03, ord("T"), ord("W"), ord("L"),
            0x10, 0x03, 0x01, 0x3A, 0x10,
            0x11, 0x03, 0x00, 0x2E, 0xE0,
        ]
    )
    ls = decode_rvt(b)
    assert ls.aircraft_mgrs_zone == 18
    assert ls.aircraft_mgrs_band_grid == "TWL"
    assert ls.aircraft_mgrs_easting_m == 80_400
    assert ls.aircraft_mgrs_northing_m == 12_000
    assert ls.aircraft_mgrs == "18TWL8040012000"
    assert ls.frame_center_mgrs is None


def test_decode_rvt_user_defined_ls_bitfield():
    # User Defined LS (RVT Tag 11): tag1 = 0b10_000101 (UINT, id 5), tag2 = 2 bytes.
    b = bytes([0x0B, 0x07, 0x01, 0x01, 0x85, 0x02, 0x02, 0xBE, 0xEF])
    ls = decode_rvt(b)
    ud = ls.user_defined[0]
    assert isinstance(ud, RvtUserData)
    assert ud.data_type == RvtUserDataType.UINT
    assert ud.numeric_id == 5
    assert ud.data == b"\xbe\xef"


def test_decode_rvt_aoi_type_three_is_reserved_poi_type_three_is_target():
    poi_b = bytes([0x0C, 0x03, 0x05, 0x01, 0x03])
    aoi_b = bytes([0x0D, 0x03, 0x06, 0x01, 0x03])
    assert decode_rvt(poi_b).points_of_interest[0].poi_type == RvtPoiType.TARGET
    assert decode_rvt(aoi_b).areas_of_interest[0].aoi_type == RvtAoiType.RESERVED


def test_decode_rvt_poi_type_wire_unknown_is_raw_int():
    # Code 9 is outside 1..=4 -- wire-unknown pass-through as a raw int,
    # not a RvtPoiType instance (same asymmetry as IcingDetected).
    b = bytes([0x0C, 0x03, 0x05, 0x01, 0x09])
    poi_type = decode_rvt(b).points_of_interest[0].poi_type
    assert poi_type == 9
    assert not isinstance(poi_type, RvtPoiType)


def test_decode_rvt_unknown_tag_preserved():
    # Tag 200 is outside the top-level 1..=21 table -- round-trip it
    # through encode_rvt (which BER-OID-encodes the tag id) rather than
    # hand-building wire bytes: a raw byte 200 (0xC8) has the BER-OID
    # continuation bit set, so it isn't a valid single-byte tag id.
    ls = RvtLs(timestamp_us=1, unknown=((200, bytes([0xAA, 0xBB])),))
    back = decode_rvt(encode_rvt(ls))
    assert any(tag == 200 for tag, _ in back.unknown)


def test_decode_rvt_empty_body_lenient():
    ls = decode_rvt(b"")
    assert ls.timestamp_us is None
    assert ls.points_of_interest == ()
    assert ls.areas_of_interest == ()
    assert ls.user_defined == ()
    assert ls.unknown == ()
    assert ls.field_errors == ()


def test_rvt_ls_frozen():
    ls = decode_rvt(b"")
    with pytest.raises((AttributeError, TypeError)):
        ls.timestamp_us = 999  # type: ignore[misc]


def test_rvt_ls_tuple_fields():
    ls = decode_rvt(_body_with_poi())
    assert isinstance(ls.points_of_interest, tuple)
    assert isinstance(ls.areas_of_interest, tuple)
    assert isinstance(ls.user_defined, tuple)
    assert isinstance(ls.unknown, tuple)
    assert isinstance(ls.field_errors, tuple)


# ---------------------------------------------------------------------------
# decode_rvt_standalone (own UL + BER length + CRC-32/MPEG-2 verify)
# ---------------------------------------------------------------------------


def test_decode_rvt_standalone_round_trips_and_verifies_crc():
    good = encode_rvt_standalone(RvtLs(timestamp_us=1_700_000_000_000_000, video_data_rate=2_000_000))
    assert good[:16] == RVT_LS_UL
    ls = decode_rvt_standalone(good)
    assert ls.timestamp_us == 1_700_000_000_000_000
    assert ls.video_data_rate == 2_000_000


def test_decode_rvt_standalone_crc_mismatch_raises():
    good = encode_rvt_standalone(RvtLs(timestamp_us=1))
    bad = good[:-1] + bytes([good[-1] ^ 0xFF])
    # The trailing 4 bytes ARE the declared CRC value itself (Tag 1's
    # value bytes -- see `encode_to_vec_standalone`), so flipping only
    # the last byte corrupts the DECLARED value while the RECOMPUTED
    # value (over everything else, unchanged) stays the original correct
    # CRC. Both are therefore knowable here, letting us assert the D3
    # mapper arm's message carries the exact hex the Rust `Crc32Mismatch`
    # Display impl emits: `"CRC-32 mismatch: declared {expected:#010x},
    # computed {found:#010x}"` (crates/tst-core/src/error.rs) -- not just
    # that some KlvError was raised.
    declared = int.from_bytes(bad[-4:], "big")
    recomputed = int.from_bytes(good[-4:], "big")
    with pytest.raises(KlvError) as excinfo:
        decode_rvt_standalone(bad)
    assert excinfo.value.kind == KlvErrorKind.CHECKSUM_MISMATCH
    assert f"declared {declared:#010x}" in excinfo.value.message
    assert f"computed {recomputed:#010x}" in excinfo.value.message


def test_decode_rvt_standalone_bad_universal_label_raises():
    with pytest.raises(KlvError):
        decode_rvt_standalone(bytes(16) + bytes([0x00]))


# ---------------------------------------------------------------------------
# encode_rvt / encode_rvt_standalone
# ---------------------------------------------------------------------------


def test_rvt_round_trip_body_form():
    ls = RvtLs(
        timestamp_us=123,
        frag_circle_radius_m=250,
        points_of_interest=(
            RvtPoi(number=7, lat_deg=45.0, lon_deg=-90.0, label="ALPHA"),
        ),
    )
    back = decode_rvt(encode_rvt(ls))
    assert back.timestamp_us == 123
    assert back.frag_circle_radius_m == 250
    assert back.points_of_interest[0].number == 7
    assert back.points_of_interest[0].label == "ALPHA"


def test_rvt_round_trip_all_top_level_scalar_fields():
    ls = RvtLs(
        timestamp_us=1_700_000_000_000_000,
        platform_true_airspeed=100,
        platform_indicated_airspeed=95,
        telemetry_accuracy_indicator=3,
        frag_circle_radius_m=250,
        frame_code=60,
        rvt_ls_version=4,
        video_data_rate=2_000_000,
        digital_video_file_format="MPEG-2",
        aircraft_mgrs_zone=18,
        aircraft_mgrs_band_grid="TWL",
        aircraft_mgrs_easting_m=80_400,
        aircraft_mgrs_northing_m=12_000,
        frame_center_mgrs_zone=19,
        frame_center_mgrs_band_grid="ABC",
        frame_center_mgrs_easting_m=1,
        frame_center_mgrs_northing_m=2,
    )
    back = decode_rvt(encode_rvt(ls))
    assert back.timestamp_us == ls.timestamp_us
    assert back.platform_true_airspeed == ls.platform_true_airspeed
    assert back.platform_indicated_airspeed == ls.platform_indicated_airspeed
    assert back.telemetry_accuracy_indicator == ls.telemetry_accuracy_indicator
    assert back.frag_circle_radius_m == ls.frag_circle_radius_m
    assert back.frame_code == ls.frame_code
    assert back.rvt_ls_version == ls.rvt_ls_version
    assert back.video_data_rate == ls.video_data_rate
    assert back.digital_video_file_format == ls.digital_video_file_format
    assert back.aircraft_mgrs == "18TWL8040012000"
    assert back.frame_center_mgrs == "19ABC0000100002"


def test_rvt_round_trip_poi_all_fields():
    poi = RvtPoi(
        number=7,
        lat_deg=45.0,
        lon_deg=-90.0,
        alt_m=1000.0,
        poi_type=RvtPoiType.TARGET,
        text="a POI",
        source_icon="icon",
        source_id="src",
        label="ALPHA",
        operation_id="op1",
    )
    ls = RvtLs(timestamp_us=1, points_of_interest=(poi,))
    back = decode_rvt(encode_rvt(ls)).points_of_interest[0]
    assert back.number == poi.number
    assert abs(back.lat_deg - poi.lat_deg) < 1e-6
    assert abs(back.lon_deg - poi.lon_deg) < 1e-6
    # alt_m is a coarser uint16 range ([-900, 19000] m over 65536 counts,
    # ~0.3 m/count) -- not lossless like the int32 lat/lon mapping above.
    assert abs(back.alt_m - poi.alt_m) < 1.0
    assert back.poi_type == RvtPoiType.TARGET
    assert back.text == poi.text
    assert back.source_icon == poi.source_icon
    assert back.source_id == poi.source_id
    assert back.label == poi.label
    assert back.operation_id == poi.operation_id


def test_rvt_round_trip_aoi_all_fields():
    aoi = RvtAoi(
        number=2,
        corner_lat_p1_deg=10.0,
        corner_lon_p1_deg=20.0,
        corner_lat_p3_deg=5.0,
        corner_lon_p3_deg=25.0,
        aoi_type=RvtAoiType.RESERVED,
        text="an AOI",
        source_id="src2",
        label="BRAVO",
        operation_id="op2",
    )
    ls = RvtLs(timestamp_us=1, areas_of_interest=(aoi,))
    back = decode_rvt(encode_rvt(ls)).areas_of_interest[0]
    assert back.number == aoi.number
    assert abs(back.corner_lat_p1_deg - aoi.corner_lat_p1_deg) < 1e-6
    assert abs(back.corner_lon_p1_deg - aoi.corner_lon_p1_deg) < 1e-6
    assert abs(back.corner_lat_p3_deg - aoi.corner_lat_p3_deg) < 1e-6
    assert abs(back.corner_lon_p3_deg - aoi.corner_lon_p3_deg) < 1e-6
    assert back.aoi_type == RvtAoiType.RESERVED
    assert back.text == aoi.text
    assert back.source_id == aoi.source_id
    assert back.label == aoi.label
    assert back.operation_id == aoi.operation_id


def test_rvt_round_trip_user_defined():
    ud = RvtUserData(numeric_id_raw=0b10_000101, data=b"\xbe\xef")
    ls = RvtLs(timestamp_us=1, user_defined=(ud,))
    back = decode_rvt(encode_rvt(ls)).user_defined[0]
    assert back.numeric_id_raw == ud.numeric_id_raw
    assert back.data == ud.data
    assert back.data_type == RvtUserDataType.UINT
    assert back.numeric_id == 5


def test_rvt_standalone_emits_ul_timestamp_first_crc_last_and_reverifies():
    ls = RvtLs(timestamp_us=1, video_data_rate=2_000_000)
    encoded = encode_rvt_standalone(ls)
    assert encoded[:16] == RVT_LS_UL
    reparsed = decode_rvt_standalone(encoded)  # CRC verify is the assertion
    assert reparsed.video_data_rate == 2_000_000
    # Tag 1 (CRC), len 4, is the last 6 bytes of the record.
    assert encoded[-6] == 0x01
    assert encoded[-5] == 0x04


def test_encode_rvt_standalone_without_timestamp_raises():
    with pytest.raises(KlvEncodeError):
        encode_rvt_standalone(RvtLs())


def test_encode_rvt_poi_missing_number_raises():
    ls = RvtLs(points_of_interest=(RvtPoi(lat_deg=1.0, lon_deg=2.0),))
    with pytest.raises(KlvEncodeError):
        encode_rvt(ls)


def test_encode_rvt_poi_missing_latitude_raises():
    ls = RvtLs(points_of_interest=(RvtPoi(number=1, lon_deg=0.0),))
    with pytest.raises(KlvEncodeError):
        encode_rvt(ls)


def test_encode_rvt_aoi_missing_type_raises():
    ls = RvtLs(
        areas_of_interest=(
            RvtAoi(
                number=1,
                corner_lat_p1_deg=1.0,
                corner_lon_p1_deg=2.0,
                corner_lat_p3_deg=3.0,
                corner_lon_p3_deg=4.0,
            ),
        )
    )
    with pytest.raises(KlvEncodeError):
        encode_rvt(ls)


def test_encode_rvt_sentinel_error_value_reemits_on_encode():
    # Decode a POI whose latitude carries the spec's INT_MIN "error"
    # sentinel, then satisfy the other encode mandatories (number/lon)
    # around it and confirm the sentinel re-emits rather than being
    # dropped (RvtPoi is frozen -- no `with_` helper, so rebuild it
    # explicitly, carrying `sentinel_tags` forward).
    b = bytes([0x0C, 0x06, 0x02, 0x04, 0x80, 0x00, 0x00, 0x00])
    decoded_poi = decode_rvt(b).points_of_interest[0]
    poi = RvtPoi(number=1, lon_deg=10.0, sentinel_tags=decoded_poi.sentinel_tags)
    back = decode_rvt(encode_rvt(RvtLs(points_of_interest=(poi,))))
    assert back.points_of_interest[0].sentinel_tags == (2,)
    assert back.points_of_interest[0].lat_deg is None


def test_encode_rvt_unknown_tag_clobbering_timestamp_dropped():
    # Unlike the Rust-layer `encode_to_vec` (which raises
    # `ReservedTagInUnknown` for this exact collision, see
    # `unknown_tag_clobbering_timestamp_rejected` in tests.rs), the
    # Python `py_to_unknown` translator filters typed-tag collisions out
    # of `unknown` BEFORE the Rust encoder ever sees them -- "typed
    # wins, drop silently" is this binding's own consistency policy
    # (matches `py_to_vmti_ls`/`py_to_uas_datalink_ls`), so no exception
    # reaches here. Round-trips cleanly with the typed field intact.
    ls = RvtLs(timestamp_us=1, unknown=((2, bytes(8)),))
    back = decode_rvt(encode_rvt(ls))
    assert back.timestamp_us == 1
    assert back.unknown == ()


def test_encode_rvt_poi_unknown_tag_clobbering_number_dropped():
    ls = RvtLs(
        points_of_interest=(
            RvtPoi(number=7, lat_deg=10.0, lon_deg=20.0, unknown=((1, bytes([0x00, 0x63])),)),
        )
    )
    back = decode_rvt(encode_rvt(ls)).points_of_interest[0]
    assert back.number == 7
    assert back.unknown == ()


def test_encode_rvt_aoi_unknown_tag_clobbering_type_dropped():
    ls = RvtLs(
        areas_of_interest=(
            RvtAoi(
                number=1,
                corner_lat_p1_deg=1.0,
                corner_lon_p1_deg=2.0,
                corner_lat_p3_deg=3.0,
                corner_lon_p3_deg=4.0,
                aoi_type=RvtAoiType.FRIENDLY,
                unknown=((6, bytes([0x02])),),
            ),
        )
    )
    back = decode_rvt(encode_rvt(ls)).areas_of_interest[0]
    assert back.aoi_type == RvtAoiType.FRIENDLY
    assert back.unknown == ()


def test_encode_rvt_unknown_fields_pass_through_when_not_typed():
    # Tag 200 is outside both the top-level 1..=21 table and the POI/AOI
    # 1..=10 range -- must round-trip verbatim, the clobber guard must
    # not reject tags it doesn't own.
    ls = RvtLs(
        timestamp_us=1,
        unknown=((200, bytes([0xAA, 0xBB])),),
        points_of_interest=(
            RvtPoi(number=1, lat_deg=10.0, lon_deg=20.0, unknown=((200, bytes([0xCC])),)),
        ),
        areas_of_interest=(
            RvtAoi(
                number=2,
                corner_lat_p1_deg=1.0,
                corner_lon_p1_deg=2.0,
                corner_lat_p3_deg=3.0,
                corner_lon_p3_deg=4.0,
                aoi_type=RvtAoiType.FRIENDLY,
                unknown=((200, bytes([0xDD])),),
            ),
        ),
    )
    back = decode_rvt(encode_rvt(ls))
    assert back.unknown == ((200, b"\xaa\xbb"),)
    assert back.points_of_interest[0].unknown == ((200, b"\xcc"),)
    assert back.areas_of_interest[0].unknown == ((200, b"\xdd"),)


def test_rvt_ls_with_helper():
    ls = RvtLs(timestamp_us=1)
    ls2 = ls.with_(frag_circle_radius_m=99)
    assert ls2.timestamp_us == 1
    assert ls2.frag_circle_radius_m == 99
    assert ls.frag_circle_radius_m is None


# ---------------------------------------------------------------------------
# Representative slice of the ST 0806 behavior (the exception type is
# `KlvError` -- there is no `KlvDecodeException` in tstrans.exceptions).
# ---------------------------------------------------------------------------


def test_decode_rvt_body_brief_slice():
    ls = decode_rvt(_body_with_poi())
    assert ls.timestamp_us == 1_700_000_000_000_000
    assert len(ls.points_of_interest) == 1
    assert ls.points_of_interest[0].number == 7
    assert abs(ls.points_of_interest[0].lat_deg - 45.0) < 1e-6


def test_rvt_round_trip_brief_slice():
    ls = RvtLs(timestamp_us=123, frag_circle_radius_m=250)
    back = decode_rvt(encode_rvt(ls))
    assert back.frag_circle_radius_m == 250


def test_standalone_crc_mismatch_raises_brief_slice():
    good = encode_rvt_standalone(RvtLs(timestamp_us=1))
    bad = good[:-1] + bytes([good[-1] ^ 0xFF])
    declared = int.from_bytes(bad[-4:], "big")
    recomputed = int.from_bytes(good[-4:], "big")
    with pytest.raises(KlvError) as excinfo:
        decode_rvt_standalone(bad)
    assert excinfo.value.kind == KlvErrorKind.CHECKSUM_MISMATCH
    assert f"declared {declared:#010x}" in excinfo.value.message
    assert f"computed {recomputed:#010x}" in excinfo.value.message
