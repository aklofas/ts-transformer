"""ST 1204 MIIS Core ID + MISMMS validator — Python binding tests.

ST 1204.3 TABLE7 reference vector (34 bytes):
  01 70 F5 92 F0 23 73 36 4A F8 AA 91 62 C0 0F 2E
  B2 DA 16 B7 43 41 00 08 41 A0 BE 36 5B 5A B9 6A
  36 45
  version=1, usage=0x70 (Physical sensor, Virtual platform), no window/minor.
"""

import pytest

from tstrans.exceptions import KlvError
from tstrans.klv import (
    CoreId,
    IdType,
    MismmsViolation,
    UasDatalinkLs,
    decode_core_id,
    encode_core_id,
    core_id_text,
    validate_mismms,
    ClassifyingCountryCodingMethod,
    ObjectCountryCodingMethod,
    SecurityClassification,
    SecurityLs,
    encode_security,
)


# ST 1204.3 Table 7 reference bytes.
TABLE7 = bytes([
    0x01, 0x70,
    0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F, 0x2E, 0xB2, 0xDA,
    0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A, 0xB9, 0x6A, 0x36, 0x45,
])

# Expected sensor and platform UUID bytes from TABLE7.
SENSOR_UUID = bytes([0xF5, 0x92, 0xF0, 0x23, 0x73, 0x36, 0x4A, 0xF8, 0xAA, 0x91, 0x62, 0xC0, 0x0F, 0x2E, 0xB2, 0xDA])
PLATFORM_UUID = bytes([0x16, 0xB7, 0x43, 0x41, 0x00, 0x08, 0x41, 0xA0, 0xBE, 0x36, 0x5B, 0x5A, 0xB9, 0x6A, 0x36, 0x45])


# ---------------------------------------------------------------------------
# decode_core_id
# ---------------------------------------------------------------------------

def test_decode_table7_fields():
    cid = decode_core_id(TABLE7)
    assert isinstance(cid, CoreId)
    assert cid.version == 1
    assert cid.sensor is not None
    assert cid.sensor[0] == IdType.PHYSICAL
    assert cid.sensor[1] == SENSOR_UUID
    assert cid.platform is not None
    assert cid.platform[0] == IdType.VIRTUAL
    assert cid.platform[1] == PLATFORM_UUID
    assert cid.window is None
    assert cid.minor is None


def test_decode_table7_idtype_enum():
    cid = decode_core_id(TABLE7)
    assert isinstance(cid.sensor[0], IdType)
    assert isinstance(cid.platform[0], IdType)


# ---------------------------------------------------------------------------
# encode_core_id round-trip
# ---------------------------------------------------------------------------

def test_encode_core_id_round_trip():
    cid = decode_core_id(TABLE7)
    encoded = encode_core_id(cid)
    assert isinstance(encoded, bytes)
    assert encoded == TABLE7


# ---------------------------------------------------------------------------
# core_id_text
# ---------------------------------------------------------------------------

EXPECTED_TEXT = (
    "0170:F592-F023-7336-4AF8-AA91-62C0-0F2E-B2DA"
    "/16B7-4341-0008-41A0-BE36-5B5A-B96A-3645:D3"
)


def test_core_id_text_spec_example():
    cid = decode_core_id(TABLE7)
    assert core_id_text(cid) == EXPECTED_TEXT


# ---------------------------------------------------------------------------
# Error cases
# ---------------------------------------------------------------------------

def test_decode_core_id_malformed_raises():
    with pytest.raises(KlvError):
        decode_core_id(b"")  # empty — Truncated

    with pytest.raises(KlvError):
        decode_core_id(b"\x02\x70")  # version != 1


# ---------------------------------------------------------------------------
# Helpers for MISMMS tests
# ---------------------------------------------------------------------------

def _full_security_ls_bytes() -> bytes:
    sec = SecurityLs(
        security_classification=SecurityClassification.UNCLASSIFIED,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_THREE_LETTER,
        classifying_country="//USA",
        sci_shi_info="SCI",
        caveats="FOUO",
        releasing_instructions="USA",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_THREE_LETTER,
        object_country_codes="USA",
        version=12,
    )
    return encode_security(sec)


def _full_mismms_record() -> UasDatalinkLs:
    """Build a record that satisfies all 23 MISMMS requirements."""
    return UasDatalinkLs(
        timestamp_us=1_700_000_000_000_000,          # Tag 2
        mission_id="MISSION-1",                       # Tag 3
        platform_heading_deg=45.0,                    # Tag 5
        platform_pitch_deg=5.0,                       # Tag 6  (6|90)
        platform_roll_deg=2.0,                        # Tag 7  (7|91)
        platform_designation="UAV-1",                 # Tag 10
        image_source_sensor="EO",                     # Tag 11
        image_coordinate_system="WGS84",              # Tag 12
        sensor_lat_deg=47.0,                          # Tag 13
        sensor_lon_deg=-122.0,                        # Tag 14
        sensor_alt_m=1500.0,                          # Tag 15
        sensor_hfov_deg=5.0,                          # Tag 16
        sensor_vfov_deg=3.75,                         # Tag 17
        sensor_rel_az_deg=180.0,                      # Tag 18
        sensor_rel_el_deg=-30.0,                      # Tag 19
        sensor_rel_roll_deg=0.5,                      # Tag 20
        slant_range_m=5000.0,                         # Tag 21
        target_width_m=100.0,                         # Tag 22 (22|96)
        frame_center_lat_deg=46.9,                    # Tag 23
        frame_center_lon_deg=-122.1,                  # Tag 24
        frame_center_elev_m=50.0,                     # Tag 25 (25|78)
        security_local_set=_full_security_ls_bytes(), # Tag 48
        miis_core_id=TABLE7,                          # Tag 94
    )


# ---------------------------------------------------------------------------
# validate_mismms — full record
# ---------------------------------------------------------------------------

def test_validate_mismms_full_record_no_violations():
    record = _full_mismms_record()
    violations = validate_mismms(record)
    assert violations == [], f"expected empty; got {violations}"


# ---------------------------------------------------------------------------
# validate_mismms — missing Mission ID (Tag 3)
# ---------------------------------------------------------------------------

def test_validate_mismms_missing_mission_id():
    record = _full_mismms_record().with_(mission_id=None)
    violations = validate_mismms(record)
    missing3 = [v for v in violations if isinstance(v, MismmsViolation) and v.kind == "missing" and v.tag == 3]
    assert len(missing3) == 1, f"expected one missing tag=3; got {violations}"
    assert missing3[0].name == "Mission ID"


# ---------------------------------------------------------------------------
# MismmsViolation dataclass shape
# ---------------------------------------------------------------------------

def test_mismms_violation_dataclass_shape():
    record = _full_mismms_record().with_(mission_id=None)
    violations = validate_mismms(record)
    v = next(x for x in violations if hasattr(x, "tag") and x.tag == 3)
    assert v.kind == "missing"
    assert v.tag == 3
    assert v.name is not None
    assert v.tag_b is None


def test_mismms_violation_is_list_of_dataclasses():
    violations = validate_mismms(_full_mismms_record())
    assert isinstance(violations, list)


# ---------------------------------------------------------------------------
# IdType enum has PHYSICAL, VIRTUAL, MANAGED
# ---------------------------------------------------------------------------

def test_id_type_enum_members():
    assert IdType.PHYSICAL is not None
    assert IdType.VIRTUAL is not None
    assert IdType.MANAGED is not None


# ---------------------------------------------------------------------------
# CoreId dataclass shape
# ---------------------------------------------------------------------------

def test_core_id_is_dataclass():
    import dataclasses
    cid = decode_core_id(TABLE7)
    assert dataclasses.is_dataclass(cid)
    assert hasattr(cid, "version")
    assert hasattr(cid, "sensor")
    assert hasattr(cid, "platform")
    assert hasattr(cid, "window")
    assert hasattr(cid, "minor")


# ---------------------------------------------------------------------------
# validate_mismms — alternation conflict (tags 75 and 104)
# ---------------------------------------------------------------------------

def test_validate_mismms_alternation_conflict_tag75_and_104():
    """Tags 75 and 104 are mutually exclusive (15|75|104 group).
    Build a record with tag 75 (sensor_ellipsoid_height_m) and tag 104
    (sensor_ellipsoid_height_extended_m, a typed field)."""
    # Start with a full compliant record, then set both tags 75 and 104.
    record = _full_mismms_record().with_(
        sensor_ellipsoid_height_m=100.5,  # Tag 75
        sensor_ellipsoid_height_extended_m=1500.0,  # Tag 104
    )
    violations = validate_mismms(record)
    # Should contain exactly one alternation_conflict violation.
    conflicts = [v for v in violations if isinstance(v, MismmsViolation) and v.kind == "alternation_conflict"]
    assert len(conflicts) == 1, f"expected one alternation_conflict; got {violations}"
    v = conflicts[0]
    assert v.tag == 75, f"expected primary tag 75; got {v.tag}"
    assert v.tag_b == 104, f"expected secondary tag 104; got {v.tag_b}"
