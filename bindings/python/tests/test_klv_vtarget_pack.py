"""VTargetPack dataclass-shape tests. Decode integration is exercised
via test_klv_st0903.py (where targets land inside VmtiLs.targets)."""

from dataclasses import fields

import pytest

from tstrans.klv import KlvFieldError, VmtiLs, VTargetPack, decode_vmti, encode_vmti


def test_vtarget_pack_has_target_id():
    p = VTargetPack(target_id=42)
    assert p.target_id == 42


def test_vtarget_pack_defaults_are_none():
    p = VTargetPack(target_id=1)
    assert p.centroid_pixel is None
    assert p.bbox_top_left_pixel is None
    assert p.priority is None
    assert p.target_color is None
    assert p.target_location is None


def test_vtarget_pack_color_is_3_byte_tuple():
    p = VTargetPack(target_id=1, target_color=(255, 128, 0))
    assert p.target_color == (255, 128, 0)


def test_vtarget_pack_field_errors_default_empty_tuple():
    p = VTargetPack(target_id=1)
    assert p.field_errors == ()
    assert isinstance(p.field_errors, tuple)


def test_vtarget_pack_unknown_default_empty_tuple():
    p = VTargetPack(target_id=1)
    assert p.unknown == ()


def test_vtarget_pack_frozen():
    p = VTargetPack(target_id=1)
    with pytest.raises((AttributeError, TypeError)):
        p.target_id = 99  # type: ignore[misc]


def test_vtarget_pack_pattern_match():
    p = VTargetPack(target_id=42, priority=5)
    match p:
        case VTargetPack(target_id=t, priority=pr):
            assert t == 42
            assert pr == 5
        case _:
            pytest.fail("did not match")


def test_vtarget_pack_field_count():
    """Sanity: ~30 fields per ST 0903.6 §10.2 Table 10."""
    f = fields(VTargetPack)
    assert len(f) >= 29


def test_vtarget_pack_vmask_is_bytes_not_list():
    """Regression: VTargetPack `Option<Vec<u8>>` translators must emit
    Python `bytes`, not `list[int]`. Decodes through a synthetic
    VTargetSeries (VmtiLs Tag 101) carrying one pack with a vmask
    payload (pack-internal Tag 101)."""
    # PSI baseline for VmtiLs decode: precision_time_stamp + version_number.
    minimal_vmti = (
        bytes([2, 8])
        + (1_700_000_000_000_000).to_bytes(8, "big")
        + bytes([4, 1, 6])
    )
    # VTargetPack body: BER-OID target_id=1, then Tag 101 vmask = 0xDEAD.
    pack_body = bytes([0x01, 0x65, 0x02, 0xDE, 0xAD])
    # VTargetSeries (VmtiLs Tag 101): each pack is BER-length-prefixed.
    series = bytes([len(pack_body)]) + pack_body
    body = minimal_vmti + bytes([101, len(series)]) + series

    v = decode_vmti(body)
    assert len(v.targets) == 1
    assert v.targets[0].target_id == 1
    assert isinstance(v.targets[0].vmask, bytes)
    assert v.targets[0].vmask == b"\xde\xad"


# ---------------------------------------------------------------------------
# Encode-path validation: invalid `target_color` shapes must raise, not be
# silently dropped.
# ---------------------------------------------------------------------------


def _vmti_with_color(color):
    """Minimal VmtiLs carrying one VTargetPack with the given target_color,
    constructed so it round-trips through `encode_vmti` (which dispatches
    via `py_to_vtarget_pack` — the validator under test)."""
    return VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        version_number=6,
        targets=(VTargetPack(target_id=1, target_color=color),),
    )


def test_encode_vtarget_pack_color_2tuple_raises():
    """A 2-element target_color must raise ValueError. Validation runs at
    construction time (__post_init__), so the error fires
    at VTargetPack(...) rather than at encode_vmti(...)."""
    with pytest.raises(ValueError, match="target_color"):
        _vmti_with_color((1, 2))


def test_encode_vtarget_pack_color_4tuple_raises():
    """A 4-element target_color must raise ValueError. Validation runs at
    construction time (__post_init__), so the error fires
    at VTargetPack(...) rather than at encode_vmti(...)."""
    with pytest.raises(ValueError, match="target_color"):
        _vmti_with_color((1, 2, 3, 4))


def test_encode_vtarget_pack_color_3tuple_ok():
    """A correctly-sized 3-tuple still passes the encoder."""
    rec = _vmti_with_color((1, 2, 3))
    out = encode_vmti(rec)
    assert isinstance(out, bytes)
    assert len(out) > 0


def test_encode_vtarget_pack_color_none_ok():
    """`None` (the default) is still a valid value — it means the field
    is absent from the encoded LS — and must not be flagged."""
    rec = _vmti_with_color(None)
    out = encode_vmti(rec)
    assert isinstance(out, bytes)
    assert len(out) > 0


# ---------------------------------------------------------------------------
# Large-value (>2**32) round-trip for target_id + pixel fields.
# Python int is unbounded; the PyO3 bridge extracts u64, so values up to
# u64::MAX round-trip (a u32 extract would silently truncate).
# ---------------------------------------------------------------------------


def test_large_target_id_and_pixels_round_trip():
    """target_id + Tags 1/2/3 (centroid_pixel, bbox_*_pixel) are V6 (max 6
    bytes) so values above u32::MAX round-trip. Tags 19/20 (centroid_pix_row /
    centroid_pix_col) are V4 (max 4 bytes per §10.2.2.20/.21) so they are
    widened to int in Python but wire values must stay within u32 range."""
    big_target_id = 2**32 + 1          # 4_294_967_297 — just above u32::MAX
    big_pixel     = 2**32 + 0x1_FFFF   # comfortably above u32, valid V6
    u32_max_row   = 2**32 - 1          # u32::MAX — largest valid V4 value

    pack = VTargetPack(
        target_id=big_target_id,
        centroid_pixel=big_pixel,
        bbox_top_left_pixel=big_pixel + 1,
        bbox_bottom_right_pixel=big_pixel + 2,
        centroid_pix_row=u32_max_row,       # V4: keep within u32::MAX
        centroid_pix_col=u32_max_row - 1,   # V4: keep within u32::MAX
    )
    vmti = VmtiLs(version_number=6, targets=(pack,))
    wire = encode_vmti(vmti)
    decoded = decode_vmti(wire)

    assert len(decoded.targets) == 1
    p = decoded.targets[0]
    assert p.target_id == big_target_id, "target_id round-trips above u32::MAX"
    assert p.centroid_pixel == big_pixel, "centroid_pixel V6 round-trip"
    assert p.bbox_top_left_pixel == big_pixel + 1
    assert p.bbox_bottom_right_pixel == big_pixel + 2
    assert p.centroid_pix_row == u32_max_row
    assert p.centroid_pix_col == u32_max_row - 1
