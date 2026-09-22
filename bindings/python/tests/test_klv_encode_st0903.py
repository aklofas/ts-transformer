"""ST 0903 encode round-trip tests."""

import pytest

from tstrans.exceptions import KlvEncodeError, KlvEncodeErrorKind
from tstrans.klv import (
    VmtiLs,
    VTargetPack,
    decode_vmti,
    encode_vmti,
    encode_vmti_standalone,
    encode_vmti_strict_compliance,
    encode_vmti_standalone_strict_compliance,
    parse_klv_universal,
)


def test_encode_vmti_ls_body_round_trip():
    body = (
        bytes([2, 8]) + (1_700_000_000_000_000).to_bytes(8, "big")  # PTS Tag 2
        + bytes([4, 1, 6])                                          # version Tag 4 = 6
        + bytes([8, 2]) + (1920).to_bytes(2, "big")                # frame_width Tag 8
    )
    vmti = decode_vmti(body)
    out = encode_vmti(vmti)
    assert isinstance(out, bytes)
    vmti2 = decode_vmti(out)
    assert vmti2.precision_time_stamp == vmti.precision_time_stamp
    assert vmti2.version_number == vmti.version_number


def test_encode_vmti_standalone_has_ul_prefix():
    vmti = decode_vmti(b"")
    out = encode_vmti_standalone(vmti)
    assert len(out) >= 16
    assert out[0] == 0x06  # SMPTE designator


def test_encode_vmti_standalone_round_trips_via_universal_dispatcher():
    body = bytes([2, 8]) + (1_700_000_000_000_000).to_bytes(8, "big") + bytes([4, 1, 6])
    vmti = decode_vmti(body)
    standalone = encode_vmti_standalone(vmti)
    parsed = parse_klv_universal(standalone)
    assert isinstance(parsed, VmtiLs)
    assert parsed.precision_time_stamp == vmti.precision_time_stamp


def test_encode_vmti_with_targets_round_trip():
    pack_body = bytes([0x01, 0x65, 0x02, 0xDE, 0xAD])  # target_id=1, vmask=0xDEAD
    series = bytes([len(pack_body)]) + pack_body
    body = (
        bytes([2, 8]) + (1_700_000_000_000_000).to_bytes(8, "big")
        + bytes([4, 1, 6])
        + bytes([101, len(series)]) + series
    )
    vmti = decode_vmti(body)
    assert len(vmti.targets) == 1
    out = encode_vmti(vmti)
    vmti2 = decode_vmti(out)
    assert len(vmti2.targets) == 1
    assert vmti2.targets[0].target_id == 1
    assert vmti2.targets[0].vmask == b"\xde\xad"


# ---------------------------------------------------------------------------
# encode_vmti_strict_compliance (embedded mode)
# ---------------------------------------------------------------------------


def _minimal_embedded_record() -> VmtiLs:
    """Minimal VmtiLs satisfying embedded-mode required tags 4+6 with one
    non-empty VTargetPack."""
    return VmtiLs(
        version_number=6,
        num_targets_reported=1,
        targets=(VTargetPack(target_id=1, centroid_pixel=100),),
    )


def test_encode_vmti_strict_compliance_missing_version_raises():
    # Omit version_number (Tag 4) — must raise MISSING_MANDATORY_ITEM.
    rec = VmtiLs(num_targets_reported=0)
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert ei.value.tag == 4


def test_encode_vmti_strict_compliance_missing_num_targets_raises():
    # Omit num_targets_reported (Tag 6) — must raise MISSING_MANDATORY_ITEM.
    rec = VmtiLs(version_number=6)
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert ei.value.tag == 6


def test_encode_vmti_strict_compliance_empty_vtarget_pack_raises():
    # A VTargetPack with no typed fields is invalid (ST 0903.4-10).
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=1,
        targets=(VTargetPack(target_id=42),),  # no fields beyond target_id
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.V_TARGET_PACK_EMPTY
    # .tag carries the target_id for pack-level errors
    assert ei.value.tag == 42


def test_encode_vmti_strict_compliance_duplicate_target_id_raises():
    # Two packs with the same target_id must raise DUPLICATE_TARGET_ID.
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=2,
        targets=(
            VTargetPack(target_id=1, centroid_pixel=100),
            VTargetPack(target_id=1, centroid_pixel=200),
        ),
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.DUPLICATE_TARGET_ID
    assert ei.value.tag == 1


def test_encode_vmti_strict_compliance_empty_pack_tag_preserves_large_target_id():
    # REF-KLV-04: target_id is u64; a value above u32::MAX must reach .tag
    # losslessly (regression for a u64-as-u32 truncation in the error mapper).
    big_target_id = 2**32 + 7  # 4_294_967_303 — above u32::MAX
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=1,
        targets=(VTargetPack(target_id=big_target_id),),  # empty pack triggers the error
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.V_TARGET_PACK_EMPTY
    assert ei.value.tag == big_target_id, ".tag must carry the full u64 target_id"


def test_encode_vmti_strict_compliance_duplicate_tag_preserves_large_target_id():
    # REF-KLV-04: the duplicate-id error must also forward the full u64 target_id.
    big_target_id = 2**32 + 7  # above u32::MAX; truncation to u32 would yield 7
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=2,
        targets=(
            VTargetPack(target_id=big_target_id, centroid_pixel=100),
            VTargetPack(target_id=big_target_id, centroid_pixel=200),
        ),
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.DUPLICATE_TARGET_ID
    assert ei.value.tag == big_target_id, ".tag must carry the full u64 target_id"


def test_encode_vmti_strict_compliance_valid_record_succeeds():
    rec = _minimal_embedded_record()
    out = encode_vmti_strict_compliance(rec)
    assert isinstance(out, bytes)
    assert len(out) > 0
    rec2 = decode_vmti(out)
    assert rec2.version_number == 6
    assert rec2.num_targets_reported == 1


# ---------------------------------------------------------------------------
# encode_vmti_standalone_strict_compliance
# ---------------------------------------------------------------------------


def _full_standalone_record() -> VmtiLs:
    """VmtiLs satisfying all standalone required tags (2,4,6,11,12,13)."""
    return VmtiLs(
        version_number=6,
        num_targets_reported=0,
        precision_time_stamp=1_700_000_000_000_000,
        horizontal_fov=45.0,
        vertical_fov=30.0,
        miis_id=b"\x00" * 16,
    )


def test_encode_vmti_standalone_strict_compliance_missing_pts_raises():
    # Omit precision_time_stamp (Tag 2) — standalone requires it.
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=0,
        horizontal_fov=45.0,
        vertical_fov=30.0,
        miis_id=b"\x00" * 16,
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_standalone_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert ei.value.tag == 2


def test_encode_vmti_standalone_strict_compliance_forbidden_offset_raises():
    # centroid_lat_offset (Tag 10) is forbidden in standalone mode.
    rec = VmtiLs(
        version_number=6,
        num_targets_reported=1,
        precision_time_stamp=1_700_000_000_000_000,
        horizontal_fov=45.0,
        vertical_fov=30.0,
        miis_id=b"\x00" * 16,
        targets=(VTargetPack(target_id=1, centroid_lat_offset=0.001),),
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_vmti_standalone_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.FORBIDDEN_STANDALONE_OFFSET
    assert ei.value.tag == 10


def test_encode_vmti_standalone_strict_compliance_valid_record_succeeds():
    rec = _full_standalone_record()
    out = encode_vmti_standalone_strict_compliance(rec)
    assert isinstance(out, bytes)
    # Standalone wraps in the VMTI UL (starts with 0x06 SMPTE designator)
    assert len(out) >= 16
    assert out[0] == 0x06
    # Parse via the universal dispatcher to confirm structure
    parsed = parse_klv_universal(out)
    assert isinstance(parsed, VmtiLs)
