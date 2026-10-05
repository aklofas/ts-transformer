"""ST 0601 composite read-only views — dataclass shape tests.
Integration via `UasDatalinkLs.sensor_position()` etc. is tested in
test_klv_st0601.py."""

import pytest

from tstrans.klv import Attitude, Corners, FieldOfView, GeoPoint


def test_geopoint_construction():
    g = GeoPoint(lat_deg=34.05, lon_deg=-118.25, alt_m=100.0)
    assert g.lat_deg == 34.05
    assert g.lon_deg == -118.25
    assert g.alt_m == 100.0


def test_attitude_construction():
    a = Attitude(heading_deg=90.0, pitch_deg=-10.0, roll_deg=2.5)
    assert a.heading_deg == 90.0
    assert a.pitch_deg == -10.0
    assert a.roll_deg == 2.5


def test_field_of_view_construction():
    f = FieldOfView(horizontal_deg=60.0, vertical_deg=33.75)
    assert f.horizontal_deg == 60.0
    assert f.vertical_deg == 33.75


def test_corners_construction():
    c = Corners(
        p1=(34.0, -118.0),
        p2=(34.1, -118.0),
        p3=(34.1, -118.1),
        p4=(34.0, -118.1),
    )
    assert c.p1 == (34.0, -118.0)
    assert c.p4 == (34.0, -118.1)


def test_all_composites_frozen():
    g = GeoPoint(lat_deg=0.0, lon_deg=0.0, alt_m=0.0)
    a = Attitude(heading_deg=0.0, pitch_deg=0.0, roll_deg=0.0)
    f = FieldOfView(horizontal_deg=0.0, vertical_deg=0.0)
    c = Corners(p1=(0, 0), p2=(0, 0), p3=(0, 0), p4=(0, 0))
    for obj in (g, a, f, c):
        with pytest.raises((AttributeError, TypeError)):
            setattr(obj, "lat_deg" if hasattr(obj, "lat_deg") else "heading_deg", 999)


def test_geopoint_equality():
    a = GeoPoint(lat_deg=34.0, lon_deg=-118.0, alt_m=100.0)
    b = GeoPoint(lat_deg=34.0, lon_deg=-118.0, alt_m=100.0)
    c = GeoPoint(lat_deg=34.0, lon_deg=-118.0, alt_m=101.0)
    assert a == b
    assert a != c


def test_geopoint_pattern_match():
    g = GeoPoint(lat_deg=34.0, lon_deg=-118.0, alt_m=100.0)
    match g:
        case GeoPoint(lat_deg=lat, lon_deg=lon, alt_m=alt):
            assert (lat, lon, alt) == (34.0, -118.0, 100.0)
        case _:
            pytest.fail("did not match")
