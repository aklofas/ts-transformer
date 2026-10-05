"""klv.patch_uas_datalink — byte-faithful tag patching (v0.2.0 #3)."""

import pytest

from tstrans import klv
from tstrans.exceptions import KlvEncodeError, KlvError


def _base_ls(**overrides):
    fields = dict(
        timestamp_us=1_700_000_000_000_000,
        mission_id="MISSION01",
        frame_center_lat_deg=33.4,
        frame_center_lon_deg=-117.6,
        corner_lat_p1_deg=33.41,
        corner_lon_p1_deg=-117.61,
        uas_ls_version=19,
    )
    fields.update(overrides)
    return klv.UasDatalinkLs(**fields)


def test_patch_dict_edits_equal_full_reencode():
    # encode emits canonical table-ordered TLVs, so patching a PRESENT
    # tag must be byte-equal to re-encoding the edited record.
    raw = klv.encode_uas_datalink(_base_ls())
    out = klv.patch_uas_datalink(raw, {"corner_lat_p1_deg": 33.99})
    expected = klv.encode_uas_datalink(_base_ls(corner_lat_p1_deg=33.99))
    assert out == expected


def test_patch_accepts_typed_set_as_edits():
    raw = klv.encode_uas_datalink(_base_ls())
    via_obj = klv.patch_uas_datalink(raw, klv.UasDatalinkLs(corner_lat_p1_deg=33.99))
    via_dict = klv.patch_uas_datalink(raw, {"corner_lat_p1_deg": 33.99})
    assert via_obj == via_dict


def test_patch_empty_edits_is_identity():
    raw = klv.encode_uas_datalink(_base_ls())
    assert klv.patch_uas_datalink(raw, {}) == raw


def test_patch_unknown_field_name_raises_type_error():
    raw = klv.encode_uas_datalink(_base_ls())
    with pytest.raises(TypeError):
        klv.patch_uas_datalink(raw, {"not_a_field": 1})


def test_patch_unknown_tlv_escape_hatch():
    # Tag 200 (out of range / permanently unmodeled) is this suite's
    # durable "genuinely unknown" stand-in — tag 103 (formerly used here)
    # is a typed field (density_altitude_extended_m).
    raw = klv.encode_uas_datalink(_base_ls(unknown=((200, b"\xde\xad"),)))
    out = klv.patch_uas_datalink(raw, {"unknown": ((200, b"\x01\x02\x03"),)})
    dec = klv.decode_uas_datalink(out)
    assert (200, b"\x01\x02\x03") in tuple(dec.unknown)


def test_patch_truncated_input_raises_decode_error():
    with pytest.raises(KlvError):
        klv.patch_uas_datalink(bytes.fromhex("060e2b34"), {})


def test_patch_out_of_range_value_raises_encode_error():
    raw = klv.encode_uas_datalink(_base_ls())
    with pytest.raises(KlvEncodeError):
        klv.patch_uas_datalink(raw, {"corner_lat_p1_deg": 999.0})
