"""Round-trip preservation of ``unknown`` TLVs through KLV encoders.

Decoders preserve forward-compat unknown tags in the
typed-set dataclasses' ``unknown: tuple[tuple[int, bytes], ...]``
field, and the inverse converters forward those
entries into the Rust struct's ``Vec<OwnedRawField>`` so the bytes
survive a ``decode -> encode -> decode`` round-trip.

Collision semantics (typed wins, silent drop): when a Python record
carries an ``unknown`` TLV whose tag is in the encoder's typed table
for that set, the Python-Rust boundary drops the ``unknown`` entry —
the typed field wins regardless of whether it is currently populated.
This keeps the encoded wire form free of duplicate TLVs, matches the
behaviour of ST 0601's existing ``KlvEncodeError::ReservedTagInUnknown``
encoder guard, and implements the documented precedence rule. Real
decode never produces such a colliding ``unknown`` entry (the decoder
routes typed tags to typed fields), so the drop only affects
user-hand-constructed records.

ST 0605 ``PrecisionTimeStampPack`` has no ``unknown`` field (it's a
fixed 2-field pack) and is intentionally excluded.
"""

from __future__ import annotations

from dataclasses import replace

import pytest

from tstrans.klv import (
    ST_0601_UL,
    SecurityLs,
    UasDatalinkLs,
    VmtiLs,
    VTargetPack,
    decode_security,
    decode_uas_datalink,
    decode_vmti,
    encode_security,
    encode_uas_datalink,
    encode_vmti,
)

from _builders.klv_tlv import ber_short as _ber_short
from _builders.klv_tlv import tlv as _tlv


# ---------------------------------------------------------------------------
# Test helpers — manual TLV synthesis for fixture injection
# ---------------------------------------------------------------------------


def _ber_long(n: int) -> bytes:
    """BER definite long-form length: 0x8X header + X big-endian length bytes.

    Used to wrap the ST 0601 outer length when the body exceeds 0x7F bytes.
    """
    if n < 0x80:
        return _ber_short(n)
    # Find the minimum number of bytes needed
    payload = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(payload)]) + payload


def _st0601_checksum(buf: bytes) -> int:
    """ST 0601 §6.3 16-bit running-sum: even index → high byte, odd → low.

    Mirrors `tst_core::klv::checksum::checksum_running_sum_16`. Caller passes
    the full prefix `[UL .. start of Tag 1 value]`.
    """
    bcc = 0
    for i, b in enumerate(buf):
        shift = 8 * (((i + 1) % 2))
        bcc = (bcc + (b << shift)) & 0xFFFF
    return bcc


def _wrap_st0601_with_checksum(body_without_checksum: bytes) -> bytes:
    """Wrap an ST 0601 LS body with UL + outer BER length + ... + Tag 1 TLV.

    Computes the running-sum checksum over `[UL || outer_len || body || 0x01 ||
    0x02]` and appends the 2-byte checksum value, so the resulting buffer
    passes lenient decode's checksum verification.
    """
    body_with_checksum_tlv = body_without_checksum + b"\x01\x02"
    outer_len = len(body_with_checksum_tlv) + 2  # +2 for checksum value
    prefix = ST_0601_UL + _ber_long(outer_len) + body_with_checksum_tlv
    cksum = _st0601_checksum(prefix)
    return prefix + cksum.to_bytes(2, "big")


def _populated_uas() -> UasDatalinkLs:
    """A small populated record sized for clean encoding."""
    return UasDatalinkLs(
        universal_label=ST_0601_UL,
        declared_version=19,
        mission_id="ROUND-TRIP-TEST",
        timestamp_us=1_700_000_000_000_000,
        platform_heading_deg=42.5,
        uas_ls_version=19,
    )


# ---------------------------------------------------------------------------
# ST 0601 UasDatalinkLs — round-trip unknown TLV preservation
# ---------------------------------------------------------------------------

# ST 0601 typed tags occupy 1-65, 67-80, 82-101, 103-114, 117-120, 123-126,
# 129, 131-137, 139 (see `is_st0601_typed_tag` in
# bindings/python/src/klv.rs). Item 66 is the
# deprecated placeholder — permanently untyped by design (ST 0601.19
# §8.66: "This item has been Deprecated") — so unlike a tag picked from a
# not-yet-assigned gap it can never collide with a future typing pass.
# Matches the Rust-side test suite, which uses Tag 66 as its unknown
# stand-in for the same reason. (`_tlv` below caps at single-byte BER-OID
# tags < 0x80; 66 satisfies that too.)
_UNKNOWN_TAG_ST0601 = 66
_UNKNOWN_PAYLOAD_ST0601 = b"\xde\xad\xbe\xef"


def _hand_built_st0601_wire_with_unknown() -> bytes:
    """Build an ST 0601 wire record with Tag 2 (PTS) + Tag 65 (Version) +
    one unknown TLV at tag 66, with a valid running-sum checksum so
    lenient decode accepts it.

    The test path: "decode a fixture with a
    known UL + one unknown tag → re-encode → decode → assert unknown
    survives byte-identical".
    """
    pts = (1_700_000_000_000_000).to_bytes(8, "big")
    body_without_checksum = (
        _tlv(2, pts)  # Precision Time Stamp
        + _tlv(65, b"\x13")  # UAS LS Version = 19
        + _tlv(_UNKNOWN_TAG_ST0601, _UNKNOWN_PAYLOAD_ST0601)
    )
    return _wrap_st0601_with_checksum(body_without_checksum)


def test_st0601_unknown_survives_round_trip():
    """Decoded record with an unknown tag re-encodes losslessly."""
    wire_with_unknown = _hand_built_st0601_wire_with_unknown()

    # 1) Decode the hand-built wire and confirm the unknown appears.
    rec1 = decode_uas_datalink(wire_with_unknown)
    assert any(
        tag == _UNKNOWN_TAG_ST0601 and value == _UNKNOWN_PAYLOAD_ST0601
        for tag, value in rec1.unknown
    ), "decoder did not preserve the injected unknown TLV"

    # 2) Re-encode the decoded record — the unknown MUST survive.
    re_encoded = encode_uas_datalink(rec1)

    # 3) Decode the re-encoded bytes and assert the unknown is still there.
    rec2 = decode_uas_datalink(re_encoded)
    assert any(
        tag == _UNKNOWN_TAG_ST0601 and value == _UNKNOWN_PAYLOAD_ST0601
        for tag, value in rec2.unknown
    ), (
        "unknown TLV was dropped on re-encode (lossy round-trip) — "
        "audit #5 regression"
    )


def test_st0601_typed_field_round_trips_alongside_unknown():
    """Typed fields and unknowns coexist on the wire."""
    wire = _hand_built_st0601_wire_with_unknown()

    rec1 = decode_uas_datalink(wire)
    re_encoded = encode_uas_datalink(rec1)
    rec2 = decode_uas_datalink(re_encoded)

    # Typed fields preserved
    assert rec2.timestamp_us == 1_700_000_000_000_000
    assert rec2.uas_ls_version == 19
    # Unknown preserved
    assert any(
        tag == _UNKNOWN_TAG_ST0601 and value == _UNKNOWN_PAYLOAD_ST0601
        for tag, value in rec2.unknown
    )


def test_st0601_collision_typed_wins_silent_drop():
    """When a user constructs an unknown TLV whose tag is already covered
    by a typed field that is set, the unknown entry is silently dropped
    (typed field wins). This avoids the encoder's ``ReservedTagInUnknown``
    error AND keeps the wire form free of duplicate TLVs."""
    rec = _populated_uas()
    # Tag 13 is `image_source_sensor` (a typed string field). Construct a
    # record with both `image_source_sensor` set AND an `unknown` entry at
    # tag 13. Typed wins: encoder gets only the typed field; unknown dropped.
    rec_with_collision = replace(
        rec,
        image_source_sensor="EO",
        unknown=((13, b"GARBAGE-FROM-USER"),),
    )
    encoded = encode_uas_datalink(rec_with_collision)  # must NOT raise
    decoded = decode_uas_datalink(encoded)
    # Typed value survives
    assert decoded.image_source_sensor == "EO"
    # No leftover unknown at tag 13 (collision dropped)
    assert not any(tag == 13 for tag, _ in decoded.unknown)


def test_st0601_non_colliding_unknown_via_python_dataclass():
    """Without going through decode first: hand-construct a record with an
    unknown entry whose tag is NOT typed, encode, and confirm survival.

    This proves the inverse converter forwards `unknown` to the Rust struct
    (not just that decode-side preservation works)."""
    rec = replace(
        _populated_uas(),
        unknown=((_UNKNOWN_TAG_ST0601, _UNKNOWN_PAYLOAD_ST0601),),
    )
    encoded = encode_uas_datalink(rec)
    decoded = decode_uas_datalink(encoded)
    assert any(
        tag == _UNKNOWN_TAG_ST0601 and value == _UNKNOWN_PAYLOAD_ST0601
        for tag, value in decoded.unknown
    )


# ---------------------------------------------------------------------------
# ST 0102 SecurityLs — round-trip unknown TLV preservation
# ---------------------------------------------------------------------------

# ST 0102.12 typed tags occupy 1..=14, 22..=24. Tag 50 is safely
# unused-and-forward-compat.
_UNKNOWN_TAG_ST0102 = 50
_UNKNOWN_PAYLOAD_ST0102 = b"\xca\xfe\xba\xbe"


def test_st0102_unknown_survives_round_trip():
    # Minimal typed body: Tag 1 (security_classification) = UNCLASSIFIED.
    body = _tlv(1, b"\x01") + _tlv(_UNKNOWN_TAG_ST0102, _UNKNOWN_PAYLOAD_ST0102)
    rec1 = decode_security(body)
    assert any(
        tag == _UNKNOWN_TAG_ST0102 and value == _UNKNOWN_PAYLOAD_ST0102
        for tag, value in rec1.unknown
    )
    re_encoded = encode_security(rec1)
    rec2 = decode_security(re_encoded)
    assert any(
        tag == _UNKNOWN_TAG_ST0102 and value == _UNKNOWN_PAYLOAD_ST0102
        for tag, value in rec2.unknown
    )


def test_st0102_collision_typed_wins_silent_drop():
    """Tag 5 is `caveats` (typed string). Construct a record with both the
    typed field AND a colliding unknown — typed wins, unknown dropped."""
    rec = SecurityLs(caveats="NOFORN", unknown=((5, b"junk"),))
    encoded = encode_security(rec)
    decoded = decode_security(encoded)
    assert decoded.caveats == "NOFORN"
    assert not any(tag == 5 for tag, _ in decoded.unknown)


def test_st0102_non_colliding_unknown_via_python_dataclass():
    rec = SecurityLs(unknown=((_UNKNOWN_TAG_ST0102, _UNKNOWN_PAYLOAD_ST0102),))
    encoded = encode_security(rec)
    decoded = decode_security(encoded)
    assert any(
        tag == _UNKNOWN_TAG_ST0102 and value == _UNKNOWN_PAYLOAD_ST0102
        for tag, value in decoded.unknown
    )


# ---------------------------------------------------------------------------
# ST 0903 VmtiLs + nested VTargetPack — unknown survives at both layers
# ---------------------------------------------------------------------------

# ST 0903.6 VMTI LS typed tags occupy 1..=13 and 101..=103. Tag 50 is safe.
_UNKNOWN_TAG_VMTI = 50
_UNKNOWN_PAYLOAD_VMTI = b"\xfa\xce"

# VTargetPack typed tags occupy 1..=23 + 100..=107. Tag 50 is safe.
_UNKNOWN_TAG_VTARGET = 50
_UNKNOWN_PAYLOAD_VTARGET = b"\xab\xcd"


def _minimal_vmti_body() -> bytes:
    """Same shape as test_klv_st0903.py's helper — required tags only."""
    return (
        _tlv(2, (1_700_000_000_000_000).to_bytes(8, "big"))
        + _tlv(4, b"\x06")  # version_number
        + _tlv(8, (1920).to_bytes(2, "big"))  # frame_width
        + _tlv(9, (1080).to_bytes(2, "big"))  # frame_height
    )


def test_st0903_vmti_top_level_unknown_survives_round_trip():
    body = _minimal_vmti_body() + _tlv(_UNKNOWN_TAG_VMTI, _UNKNOWN_PAYLOAD_VMTI)
    rec1 = decode_vmti(body)
    assert any(
        tag == _UNKNOWN_TAG_VMTI and value == _UNKNOWN_PAYLOAD_VMTI
        for tag, value in rec1.unknown
    )
    re_encoded = encode_vmti(rec1)
    rec2 = decode_vmti(re_encoded)
    assert any(
        tag == _UNKNOWN_TAG_VMTI and value == _UNKNOWN_PAYLOAD_VMTI
        for tag, value in rec2.unknown
    )


def test_st0903_vmti_collision_typed_wins_silent_drop():
    """Tag 3 is `vmti_system_name` (typed string). Set both → typed wins."""
    rec = VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        vmti_system_name="MY-VMTI",
        unknown=((3, b"junk"),),
    )
    encoded = encode_vmti(rec)
    decoded = decode_vmti(encoded)
    assert decoded.vmti_system_name == "MY-VMTI"
    assert not any(tag == 3 for tag, _ in decoded.unknown)


def test_st0903_vmti_non_colliding_unknown_via_python_dataclass():
    rec = VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        unknown=((_UNKNOWN_TAG_VMTI, _UNKNOWN_PAYLOAD_VMTI),),
    )
    encoded = encode_vmti(rec)
    decoded = decode_vmti(encoded)
    assert any(
        tag == _UNKNOWN_TAG_VMTI and value == _UNKNOWN_PAYLOAD_VMTI
        for tag, value in decoded.unknown
    )


def test_st0903_vtarget_pack_nested_unknown_survives_round_trip():
    """Nested case: VmtiLs with a VTargetPack that itself carries an
    unknown TLV. Both layers must preserve their unknowns through the
    encode/decode boundary."""
    pack = VTargetPack(
        target_id=1,
        priority=200,
        unknown=((_UNKNOWN_TAG_VTARGET, _UNKNOWN_PAYLOAD_VTARGET),),
    )
    rec = VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        targets=(pack,),
        unknown=((_UNKNOWN_TAG_VMTI, _UNKNOWN_PAYLOAD_VMTI),),
    )
    encoded = encode_vmti(rec)
    decoded = decode_vmti(encoded)

    # Top-level unknown survived
    assert any(
        tag == _UNKNOWN_TAG_VMTI and value == _UNKNOWN_PAYLOAD_VMTI
        for tag, value in decoded.unknown
    )
    # Nested VTargetPack and its unknown survived
    assert len(decoded.targets) == 1
    decoded_pack = decoded.targets[0]
    assert decoded_pack.target_id == 1
    assert decoded_pack.priority == 200
    assert any(
        tag == _UNKNOWN_TAG_VTARGET and value == _UNKNOWN_PAYLOAD_VTARGET
        for tag, value in decoded_pack.unknown
    )


def test_st0903_vtarget_pack_collision_typed_wins_silent_drop():
    """VTargetPack Tag 5 is `confidence_level` (typed u8). Set both → typed
    wins, unknown dropped."""
    pack = VTargetPack(
        target_id=1,
        confidence_level=87,
        unknown=((5, b"\xff"),),
    )
    rec = VmtiLs(
        precision_time_stamp=1_700_000_000_000_000,
        targets=(pack,),
    )
    encoded = encode_vmti(rec)
    decoded = decode_vmti(encoded)
    assert len(decoded.targets) == 1
    decoded_pack = decoded.targets[0]
    assert decoded_pack.confidence_level == 87
    assert not any(tag == 5 for tag, _ in decoded_pack.unknown)


# ---------------------------------------------------------------------------
# Type / value validation on the Python -> Rust boundary
# ---------------------------------------------------------------------------


def test_unknown_with_wrong_inner_shape_raises():
    """A malformed `unknown` entry (not a 2-tuple) should raise rather than
    silently corrupt the Rust side."""
    rec = SecurityLs(unknown=((50, b"ok", b"extra"),))  # type: ignore[arg-type]
    with pytest.raises((ValueError, TypeError)):
        encode_security(rec)


def test_unknown_with_non_int_tag_raises():
    rec = SecurityLs(unknown=(("not-an-int", b"value"),))  # type: ignore[arg-type]
    with pytest.raises((ValueError, TypeError)):
        encode_security(rec)


def test_unknown_with_non_bytes_value_raises():
    rec = SecurityLs(unknown=((50, "not-bytes"),))  # type: ignore[arg-type]
    with pytest.raises((ValueError, TypeError)):
        encode_security(rec)
