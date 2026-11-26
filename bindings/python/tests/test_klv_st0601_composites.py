"""ST 0601 composite read-only views — dataclass shape tests.
Integration via `UasDatalinkLs.sensor_position()` etc. is tested in
test_klv_st0601.py."""

import dataclasses
import json
import math
import pathlib

import pytest

from tstrans.klv import AltitudeDatum, Attitude, Corners, FieldOfView, GeoPoint, UasDatalinkLs


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


@pytest.mark.parametrize(("obj", "field", "value"), [
    (GeoPoint(lat_deg=0.0, lon_deg=0.0, alt_m=0.0), "lat_deg", 1.0),
    (Attitude(heading_deg=0.0, pitch_deg=0.0, roll_deg=0.0), "heading_deg", 1.0),
    (FieldOfView(horizontal_deg=0.0, vertical_deg=0.0), "horizontal_deg", 1.0),
    (Corners(p1=(0, 0), p2=(0, 0), p3=(0, 0), p4=(0, 0)), "p1", (1.0, 1.0)),
])
def test_all_composites_frozen(obj, field, value):
    assert field in type(obj).__dataclass_fields__
    with pytest.raises(dataclasses.FrozenInstanceError):
        setattr(obj, field, value)


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


# ---------------------------------------------------------------------------
# The views, against the case table the Rust and JVM suites read too
# ---------------------------------------------------------------------------

_TABLE = (
    pathlib.Path(__file__).resolve().parents[3]
    / "crates/tst-core/tests/fixtures/st0601/composite-views.json"
)
_ROWS = json.loads(_TABLE.read_text())["rows"]


def _view(record: UasDatalinkLs, view: str) -> object:
    if view == "platform_attitude":
        a = record.platform_attitude()
        return None if a is None else [a.heading_deg, a.pitch_deg, a.roll_deg]
    if view in ("sensor_position", "frame_center"):
        p = getattr(record, view)()
        return None if p is None else [p.lat_deg, p.lon_deg, p.alt_m]
    if view == "corners":
        c = record.corners()
        return None if c is None else [list(c.p1), list(c.p2), list(c.p3), list(c.p4)]
    if view in ("sensor_position_datum", "frame_center_datum"):
        d = getattr(record, view)()
        assert d is None or isinstance(d, AltitudeDatum)
        return None if d is None else d.value
    raise AssertionError(f"unknown view {view}")


def _flat(v: object) -> list[float]:
    return [x for item in v for x in (item if isinstance(item, list) else [item])]  # type: ignore[union-attr]


@pytest.mark.parametrize("row", _ROWS, ids=[r["name"] for r in _ROWS])
def test_composite_views_follow_the_case_table(row: dict):
    record = UasDatalinkLs(
        **row["fields"],
        sentinel_tags=tuple(row.get("sentinel_tags", ())),
        imapb_specials=tuple(tuple(e) for e in row.get("imapb_specials", ())),
    )
    tol = row.get("tolerance", 0.0)
    for view, want in row["expect"].items():
        got = _view(record, view)
        if want is None or isinstance(want, str):
            assert got == want, (row["name"], view, got)
            continue
        assert got is not None, (row["name"], view)
        g, w = _flat(got), _flat(want)
        assert len(g) == len(w), (row["name"], view)
        for a, b in zip(g, w):
            assert (a == b) if tol == 0.0 else abs(a - b) <= tol, (row["name"], view, got, want)


def test_non_finite_offsets_come_back_as_themselves():
    for offset in (math.inf, -math.inf, math.nan):
        record = UasDatalinkLs(
            frame_center_lat_deg=10.0,
            frame_center_lon_deg=170.0,
            **{f"corner_lat_offset_p{i}_deg": 0.0 for i in range(1, 5)},
            **{f"corner_lon_offset_p{i}_deg": offset for i in range(1, 5)},
        )
        c = record.corners()
        assert c is not None
        for lon in (c.p1[1], c.p2[1], c.p3[1], c.p4[1]):
            assert (math.isnan(lon) and math.isnan(offset)) or lon == offset
