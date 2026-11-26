"""RVT MGRS components (ST 0806.4 Table 8-1 Items 14-21): the composite
accessors and the encoders follow the standard's zone and band rules."""

import pytest

from tstrans.exceptions import KlvEncodeError, KlvEncodeErrorKind
from tstrans.klv import RvtLs, decode_rvt, encode_rvt


def _mgrs_body(zone: int, band_grid: bytes) -> bytes:
    # Items 14-17: zone, band + grid square, easting 80400, northing 12000.
    return bytes(
        [0x0E, 0x01, zone, 0x0F, len(band_grid)]
        + list(band_grid)
        + [0x10, 0x03, 0x01, 0x3A, 0x10, 0x11, 0x03, 0x00, 0x2E, 0xE0]
    )


def test_a_valid_mgrs_reads_as_the_fifteen_character_string():
    ls = decode_rvt(_mgrs_body(18, b"TWL"))
    assert ls.aircraft_mgrs == "18TWL8040012000"
    assert len(ls.aircraft_mgrs) == 15


@pytest.mark.parametrize(
    ("zone", "band_grid"),
    [(0, b"TWL"), (61, b"TWL"), (18, b"tw1"), (18, b"TIO")],
)
def test_a_decoded_mgrs_component_the_standard_does_not_define_has_no_composite(zone, band_grid):
    # A lenient decode keeps the bad component; the composite is None.
    ls = decode_rvt(_mgrs_body(zone, band_grid))
    assert ls.aircraft_mgrs_zone == zone
    assert ls.aircraft_mgrs is None
    fc = RvtLs(
        frame_center_mgrs_zone=zone,
        frame_center_mgrs_band_grid=band_grid.decode(),
        frame_center_mgrs_easting_m=1,
        frame_center_mgrs_northing_m=2,
    )
    assert fc.frame_center_mgrs is None


@pytest.mark.parametrize(
    ("fields", "tag"),
    [
        ({"aircraft_mgrs_zone": 0}, 14),
        ({"aircraft_mgrs_zone": 61}, 14),
        ({"aircraft_mgrs_band_grid": "tw1"}, 15),
        ({"aircraft_mgrs_band_grid": "TIO"}, 15),
        ({"frame_center_mgrs_zone": 0}, 18),
        ({"frame_center_mgrs_band_grid": "TIO"}, 19),
    ],
)
def test_the_encoder_refuses_an_mgrs_component_the_standard_does_not_define(fields, tag):
    with pytest.raises(KlvEncodeError) as ei:
        encode_rvt(RvtLs(**fields))
    assert ei.value.kind is KlvEncodeErrorKind.OUT_OF_RANGE
    assert ei.value.tag == tag
