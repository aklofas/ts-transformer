"""ST 0601 pack & list items + `klv::st1010` SDCC-FLP —
Python wrap tests.

Spec vectors are transcribed from the same MISB ST 0601.19 §8 worked
examples the Rust test suite pins (`crates/tst-core/src/klv/st0601/
tests.rs`, `wpc_*`), driven through `decode_uas_datalink`: a hand-built
spec-byte vector catches a wrong
wire formula that a decode(encode(x)) round trip cannot. Each vector is
also re-encoded and the resulting TLV bytes compared back against the
original vector, exercising the Python -> Rust inverse translator.
"""

from __future__ import annotations

import pytest

from tstrans.exceptions import KlvEncodeError, KlvError
from tstrans.klv import (
    ST_0601_UL,
    AirbaseLocations,
    ControlCommand,
    CountryCodes,
    ImageHorizonPixels,
    Location,
    MetadataSubstreamId,
    PayloadList,
    PayloadRecord,
    PayloadType,
    SdccFlp,
    SdccFlpField,
    SensorFrameRate,
    UasDatalinkLs,
    ViewDomain,
    ViewDomainPair,
    Waypoint,
    WeaponsStore,
    decode_sdcc_flp,
    decode_uas_datalink,
    encode_sdcc_flp_mode2,
    encode_uas_datalink,
)

from _builders.klv_tlv import ber_short as _ber_short

# ---------------------------------------------------------------------------
# Hand-built-wire test helpers. `_tlv`/`_ber_oid_tag`/`_ber_long` are ST
# 0601-specific (multi-byte BER-OID tag + BER long-form length) and stay
# local — a different wire shape from the single-byte-tag + short-form
# `_tlv` the other KLV test files share via `_builders.klv_tlv`.
# ---------------------------------------------------------------------------


def _hex(s: str) -> bytes:
    """Strip whitespace and parse a hex string — mirrors the identical
    helper in the Rust `st0601::tests` and `st1010::tests` modules."""
    return bytes.fromhex("".join(s.split()))


def _ber_long(n: int) -> bytes:
    """BER definite-form length: short form under 0x80, else 0x8X header
    + X big-endian length bytes."""
    if n < 0x80:
        return _ber_short(n)
    payload = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(payload)]) + payload


def _ber_oid_tag(tag: int) -> bytes:
    """BER-OID (base-128, high-bit-continuation) tag encoding — the
    write-side inverse of `_local_set_ber_oid_tag`. Single byte for
    tag < 0x80 (covers 81/102/115/116/121/122/127); multi-byte (7 bits
    per byte, continuation bit set on all but the last byte) above that
    (covers 128/130/138/140/141/142/143)."""
    if tag < 0x80:
        return bytes([tag])
    groups = [tag & 0x7F]
    tag >>= 7
    while tag > 0:
        groups.append(tag & 0x7F)
        tag >>= 7
    groups.reverse()
    return bytes(g | 0x80 for g in groups[:-1]) + bytes([groups[-1]])


def _tlv(tag: int, value: bytes) -> bytes:
    """One `[BER-OID tag][BER length][value]` TLV — the ST 0601 LS body shape."""
    return _ber_oid_tag(tag) + _ber_long(len(value)) + value


def _st0601_checksum(buf: bytes) -> int:
    """ST 0601 §6.3 16-bit running-sum: even index -> high byte, odd -> low.
    `buf` is `[UL .. start of Tag 1 value]`."""
    bcc = 0
    for i, b in enumerate(buf):
        shift = 8 * (((i + 1) % 2))
        bcc = (bcc + (b << shift)) & 0xFFFF
    return bcc


def _wrap_st0601_with_checksum(body_without_checksum: bytes) -> bytes:
    """Wrap an ST 0601 LS body with UL + outer BER length + ... + Tag 1
    TLV, computing a valid running-sum checksum so lenient decode accepts it."""
    body_with_checksum_tlv = body_without_checksum + b"\x01\x02"
    outer_len = len(body_with_checksum_tlv) + 2
    prefix = ST_0601_UL + _ber_long(outer_len) + body_with_checksum_tlv
    cksum = _st0601_checksum(prefix)
    return prefix + cksum.to_bytes(2, "big")


def _decode_single_tlv(tag: int, value: bytes) -> UasDatalinkLs:
    """Decode a minimal ST 0601 record containing exactly one TLV —
    mirrors the Rust test suite's `decode_with_single_tlv[_ber_oid]`."""
    return decode_uas_datalink(_wrap_st0601_with_checksum(_tlv(tag, value)))


def _decode_body(body: bytes) -> UasDatalinkLs:
    """Decode a full (already TLV-framed) LS body — mirrors the Rust
    test suite's `decode_body`."""
    return decode_uas_datalink(_wrap_st0601_with_checksum(body))


def _local_set_ber_oid_tag(buf: bytes, i: int) -> tuple[int, int]:
    """Read a BER-OID tag starting at index i. Returns (tag, bytes_consumed)."""
    tag = 0
    j = i
    while True:
        b = buf[j]
        tag = (tag << 7) | (b & 0x7F)
        j += 1
        if not (b & 0x80):
            break
    return tag, j - i


def _iter_tlvs(encoded: bytes):
    """Yield `(tag, value_bytes)` for every top-level TLV in an ST 0601
    wire record's body (after the 16-byte UL + outer BER length)."""
    offset = 16
    first = encoded[offset]
    if first < 0x80:
        offset += 1
    else:
        nbytes = first & 0x7F
        offset += 1 + nbytes
    while offset < len(encoded):
        tag, consumed = _local_set_ber_oid_tag(encoded, offset)
        offset += consumed
        length_byte = encoded[offset]
        if length_byte < 0x80:
            length = length_byte
            offset += 1
        else:
            nbytes = length_byte & 0x7F
            length = int.from_bytes(encoded[offset + 1 : offset + 1 + nbytes], "big")
            offset += 1 + nbytes
        value = encoded[offset : offset + length]
        yield tag, value
        offset += length


def _find_tag_value_bytes(encoded: bytes, tag: int) -> bytes | None:
    for t, v in _iter_tlvs(encoded):
        if t == tag:
            return v
    return None


def _find_all_tag_value_bytes(encoded: bytes, tag: int) -> list[bytes]:
    return [v for t, v in _iter_tlvs(encoded) if t == tag]


def _reencoded_tag_value(tag: int, record: UasDatalinkLs) -> bytes | None:
    """Re-encode `record` (typically the direct output of
    `_decode_single_tlv`) and return `tag`'s TLV value bytes — the
    Python-level analogue of the Rust suite's
    `tlv_value(&re_encoded, TAG) == Some(v)` assertions."""
    return _find_tag_value_bytes(encode_uas_datalink(record), tag)


# ---------------------------------------------------------------------------
# Simple DLP packs (81/115/116/121/127/143)
# ---------------------------------------------------------------------------


def test_image_horizon_geo_truncated_vector():
    # Tag 81 — (0,36)->(56,0), no optional geo fields (§8.81 example).
    v = bytes([0x00, 0x24, 0x38, 0x00])
    rec = _decode_single_tlv(81, v)
    h = rec.image_horizon
    assert h is not None
    assert (h.x0_pct, h.y0_pct, h.x1_pct, h.y1_pct) == (0, 36, 56, 0)
    assert h.start_lat_deg is None
    assert h.start_lon_deg is None
    assert h.end_lat_deg is None
    assert h.end_lon_deg is None
    assert _reencoded_tag_value(81, rec) == v


def test_control_command_multi_instance_and_time_us():
    # Tag 115 — MULTI-INSTANCE: two occurrences append two ControlCommands.
    v115a = _hex("05 11 466C7920746F20576179706F696E742031")
    v115b = _hex("07 03 41 42 43")  # (7, "ABC")
    rec = _decode_body(_tlv(115, v115a) + _tlv(115, v115b))
    assert len(rec.control_commands) == 2
    assert rec.control_commands[0] == ControlCommand(id=5, command="Fly to Waypoint 1")
    assert rec.control_commands[0].time_us is None
    assert rec.control_commands[1] == ControlCommand(id=7, command="ABC")

    # Round trip through encode -> decode preserves both instances.
    back = decode_uas_datalink(encode_uas_datalink(rec))
    assert back.control_commands == rec.control_commands


def test_control_command_with_time_us_round_trips():
    # time_us presence isn't one of the spec vectors (only its
    # absence is spec-pinned above) — a closed-loop round trip is a
    # legitimate binding-fidelity check here, not a wire-spec claim.
    rec = UasDatalinkLs(
        control_commands=(ControlCommand(id=200, command="abc", time_us=1_700_000_000_000_000),)
    )
    back = decode_uas_datalink(encode_uas_datalink(rec))
    assert back.control_commands == rec.control_commands


def test_control_command_verification_and_active_wavelengths_id_lists():
    rec116 = _decode_single_tlv(116, bytes([0x03, 0x07]))
    assert rec116.control_command_verification == (3, 7)
    assert _reencoded_tag_value(116, rec116) == bytes([0x03, 0x07])

    rec121 = _decode_single_tlv(121, bytes([0x01, 0x03]))
    assert rec121.active_wavelengths == (1, 3)
    assert _reencoded_tag_value(121, rec121) == bytes([0x01, 0x03])


def test_sensor_frame_rate_vector_and_denominator_default():
    v = _hex("83 D4 60 87 69")
    rec = _decode_single_tlv(127, v)
    fr = rec.sensor_frame_rate
    assert fr is not None
    assert (fr.numerator, fr.denominator) == (60000, 1001)
    assert _reencoded_tag_value(127, rec) == v

    # Denominator absent from the wire defaults to 1.
    rec2 = _decode_single_tlv(127, _hex("1E"))
    fr2 = rec2.sensor_frame_rate
    assert (fr2.numerator, fr2.denominator) == (30, 1)
    assert fr2.fps == 30.0
    assert _reencoded_tag_value(127, rec2) == _hex("1E")


def test_metadata_substream_id_vector():
    v = _hex("00 8DC4F462 3EA25A85 9C5D0AF0 C95E8C39")
    rec = _decode_single_tlv(143, v)
    ms = rec.metadata_substream_id
    assert ms is not None
    assert ms.local_id == 0
    assert ms.uuid[0] == 0x8D
    assert len(ms.uuid) == 16
    assert _reencoded_tag_value(143, rec) == v


def test_metadata_substream_id_uuid_wrong_length_rejected():
    with pytest.raises(ValueError):
        MetadataSubstreamId(local_id=0, uuid=b"\x00" * 15)


def test_strict_compliance_allows_repeated_115_and_102():
    # Multiples Allowed = Yes items must not trip the once-per-packet
    # DuplicateTag check under strict compliance — exercises BOTH carve-out
    # tags (115 and 102), not just 115, despite the test's original name.
    pack = _hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000")
    body = (
        _tlv(2, (1_700_000_000_000_000).to_bytes(8, "big"))
        + _tlv(115, _hex("05 11 466C7920746F20576179706F696E742031"))
        + _tlv(115, _hex("07 03 41 42 43"))
        + _tlv(102, pack)
        + _tlv(102, pack)
        + _tlv(65, bytes([19]))
    )
    buf = _wrap_st0601_with_checksum(body)
    rec = decode_uas_datalink(buf, compliance=True)
    assert len(rec.control_commands) == 2
    assert len(rec.sdcc_flps) == 2


# ---------------------------------------------------------------------------
# VLP series packs (122/128/130/138/140/141/142)
# ---------------------------------------------------------------------------


def test_country_codes_vector():
    v = _hex("01 0E 03 43414E 00 03 465241")
    rec = _decode_single_tlv(122, v)
    cc = rec.country_codes
    assert cc == CountryCodes(coding_method=14, overflight="CAN", operator=None, manufacture="FRA")
    assert _reencoded_tag_value(122, rec) == v


def test_country_codes_truncation_cases():
    # Manufacture explicit-length-0 with Operator present canonicalizes
    # away the now-redundant trailing zero-length pair on re-encode.
    v = _hex("01 0E 03 43414E 03 465241 00")
    rec = _decode_single_tlv(122, v)
    cc = rec.country_codes
    assert cc.operator == "FRA"
    assert cc.manufacture is None
    assert _reencoded_tag_value(122, rec) == _hex("01 0E 03 43414E 03 465241")

    # Fully truncated: only coding_method + overflight on the wire.
    v2 = _hex("01 0E 03 43414E")
    rec2 = _decode_single_tlv(122, v2)
    assert rec2.country_codes.operator is None
    assert rec2.country_codes.manufacture is None
    assert _reencoded_tag_value(122, rec2) == v2


def test_wavelengths_list_vector():
    v = _hex("0D 15 0000 07D0 0000 0FA0 4E4E 4952")
    rec = _decode_single_tlv(128, v)
    wl = rec.wavelengths_list
    assert len(wl) == 1
    w = wl[0]
    assert w.id == 21
    assert w.name == "NNIR"
    assert _reencoded_tag_value(128, rec) == v


def test_airbase_locations_vector():
    v = _hex("0B406BC20919BDA554070E000B40783CB819A2927407C600")
    rec = _decode_single_tlv(130, v)
    al = rec.airbase_locations
    take_off = al.take_off
    assert abs(take_off.lat_deg - 38.841859) < 1e-4
    assert abs(take_off.lon_deg - -77.036784) < 1e-4
    assert abs(take_off.hae_m - 3.0) < 0.1
    recovery = al.recovery
    assert abs(recovery.lat_deg - 38.939353) < 1e-4
    assert abs(recovery.lon_deg - -77.459811) < 1e-4
    assert abs(recovery.hae_m - 95.0) < 0.1
    assert _reencoded_tag_value(130, rec) == v


def test_airbase_locations_recovery_omitted_defaults_to_take_off():
    v = _hex("0B406BC20919BDA554070E00")  # take-off only, no recovery pair
    rec = _decode_single_tlv(130, v)
    al = rec.airbase_locations
    assert al.recovery == al.take_off
    assert _reencoded_tag_value(130, rec) == v


def test_payload_list_vector():
    v = _hex(
        """
        03 12 0000 0F56 4953 204E 6F73 6520 4361 6D65 7261
        15 01 0012 4143 4D45 2056 4953 204D 6F64 656C 2031 3233
        14 02 0011 4143 4D45 2049 5220 4D6F 6465 6C20 3435 36
        """
    )
    assert len(v) == 63, "the §8.138 example value is 63 bytes"
    rec = _decode_single_tlv(138, v)
    pl = rec.payload_list
    assert pl.count == 3
    assert len(pl.records) == 3
    assert pl.records[0] == PayloadRecord(
        id=0, payload_type=PayloadType.ELECTRO_OPTICAL, name="VIS Nose Camera"
    )
    assert pl.records[1].name == "ACME VIS Model 123"
    assert pl.records[2].name == "ACME IR Model 456"
    assert _reencoded_tag_value(138, rec) == v


def test_weapons_stores_vector_and_status_accessors():
    r1 = _hex("0E 01 01 01 03 82 03 07 48 61 72 70 6F 6F 6E")  # Harpoon
    r2 = _hex("0F 01 01 02 02 9E 04 08 48 65 6C 6C 66 69 72 65")  # Hellfire
    r3 = _hex("0C 01 02 01 01 03 06 47 42 55 2D 31 35")  # GBU-15
    v = r1 + r2 + r3
    assert len(v) == 44, "3 records' own length prefixes total 44 bytes"

    rec = _decode_single_tlv(140, v)
    stores = rec.weapons_stores
    assert len(stores) == 3

    harpoon = stores[0]
    assert (harpoon.station_id, harpoon.hardpoint_id, harpoon.carriage_id, harpoon.store_id) == (
        1,
        1,
        1,
        3,
    )
    assert harpoon.general_status == 3
    assert harpoon.fuze_enabled
    assert not harpoon.laser_enabled
    assert not harpoon.target_enabled
    assert not harpoon.weapon_armed
    assert harpoon.weapon_type == "Harpoon"

    hellfire = stores[1]
    assert hellfire.general_status == 4
    assert hellfire.fuze_enabled
    assert hellfire.laser_enabled
    assert hellfire.target_enabled
    assert hellfire.weapon_armed
    assert hellfire.weapon_type == "Hellfire"

    gbu15 = stores[2]
    assert (gbu15.station_id, gbu15.hardpoint_id) == (1, 2)
    assert gbu15.general_status == 3
    assert not gbu15.fuze_enabled
    assert gbu15.weapon_type == "GBU-15"

    assert _reencoded_tag_value(140, rec) == v


def test_waypoint_list_vector():
    v = _hex(
        """
        0F 00 0001 03 4071D894 19BDBFE7 089800
        0F 01 0002 02 4071D388 19BCCE24 08FC00
        0F 02 7FFF 01 4071E308 19BF2C1B 07D000
        0F 03 FFFE 00 4071E5AF 19BF5AA7 096000
        """
    )
    rec = _decode_single_tlv(141, v)
    wps = rec.waypoint_list
    assert len(wps) == 4
    assert wps[0].prosecution_order == 1
    assert wps[1].prosecution_order == 2
    assert wps[2].prosecution_order == 0x7FFF  # cancelled
    assert wps[3].prosecution_order == -2  # historical
    assert wps[0].info == 3
    assert wps[3].info == 0

    loc = wps[0].location
    assert abs(loc.lat_deg - 38.889422) < 1e-5
    assert abs(loc.lon_deg - -77.035162) < 1e-5
    assert abs(loc.hae_m - 200.0) < 0.1

    loc3 = wps[3].location
    assert abs(loc3.lat_deg - 38.889822) < 1e-5
    assert abs(loc3.hae_m - 300.0) < 0.1

    assert _reencoded_tag_value(141, rec) == v


def test_view_domain_truncated_roll():
    v = _hex("06 348000 4B0000 06 1A4000 0C8000")
    rec = _decode_single_tlv(142, v)
    vd = rec.view_domain
    assert abs(vd.azimuth.start_deg - 210.0) < 0.01
    assert abs(vd.azimuth.range_deg - 300.0) < 0.01
    assert abs(vd.elevation.start_deg - -75.0) < 0.01
    assert abs(vd.elevation.range_deg - 50.0) < 0.01
    assert vd.roll is None
    assert _reencoded_tag_value(142, rec) == v


def test_view_domain_leading_unknown_pair():
    v = _hex("00 06 1A4000 0C8000 06 578000 050000")
    rec = _decode_single_tlv(142, v)
    vd = rec.view_domain
    assert vd.azimuth is None
    assert abs(vd.elevation.start_deg - -75.0) < 0.01
    assert abs(vd.roll.start_deg - 350.0) < 0.1
    assert abs(vd.roll.range_deg - 20.0) < 0.1
    assert _reencoded_tag_value(142, rec) == v


# ---------------------------------------------------------------------------
# Tag 102 SDCC-FLP positional capture
# ---------------------------------------------------------------------------


def test_sdcc_positional_capture():
    """Two Tag 102 occurrences over two disjoint preceding-item groups
    prove per-occurrence capture (not one running list). The 6 scalar
    tags' wire bytes are carved out of `encode_uas_datalink`'s own
    output (rather than hand-derived) so this test doesn't duplicate the
    IMAPB/linear-range byte math — only the positional-capture logic
    (the thing this test targets) is hand-built."""
    scalars = UasDatalinkLs(
        sensor_lat_deg=34.0,
        sensor_lon_deg=-118.0,
        sensor_alt_m=1500.0,
        platform_heading_deg=90.0,
        platform_pitch_deg=1.0,
        platform_roll_deg=0.5,
    )
    tlv_map = dict(_iter_tlvs(encode_uas_datalink(scalars)))
    pack = _hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000")

    body = (
        _tlv(13, tlv_map[13])
        + _tlv(14, tlv_map[14])
        + _tlv(15, tlv_map[15])
        + _tlv(102, pack)
        + _tlv(5, tlv_map[5])
        + _tlv(6, tlv_map[6])
        + _tlv(7, tlv_map[7])
        + _tlv(102, pack)
    )
    rec = _decode_body(body)
    assert len(rec.sdcc_flps) == 2
    assert rec.sdcc_flps[0].preceding_tags == (13, 14, 15)
    assert rec.sdcc_flps[1].preceding_tags == (5, 6, 7)
    m = decode_sdcc_flp(rec.sdcc_flps[0].bytes)
    assert m.std_devs == (1.0, 2.0, 4.0)

    # Byte-fidelity re-encode: both occurrences survive a round trip.
    back = decode_uas_datalink(encode_uas_datalink(rec))
    assert len(back.sdcc_flps) == 2
    assert back.sdcc_flps[0].bytes == rec.sdcc_flps[0].bytes


def test_sdcc_malformed_header_is_field_error_not_exception():
    # A truncated BER-OID Matrix Size (empty value) cannot be peeked for
    # N — the occurrence is dropped into field_errors, not raised.
    rec = _decode_single_tlv(102, b"")
    assert rec.sdcc_flps == ()
    assert len(rec.field_errors) == 1


def test_sdcc_tag_dropped_from_unknown_on_the_python_boundary():
    # Tag 102 is now typed (sdcc_flps) — `py_to_unknown`'s "typed wins,
    # silently drop" collision policy (bindings/python/src/klv.rs) filters
    # a caller-supplied `unknown` entry at tag 102 BEFORE it ever reaches
    # the real Rust encoder, so this must NOT raise (the raw-crate
    # equivalent, `wpc_sdcc_tag_rejected_from_unknown`, calls
    # `encode_to_vec` directly and DOES observe `ReservedTagInUnknown` —
    # that check never fires here because the colliding entry never
    # reaches it).
    rec = UasDatalinkLs(unknown=((102, b"\x01"),))
    encoded = encode_uas_datalink(rec)  # must NOT raise
    decoded = decode_uas_datalink(encoded)
    assert not any(tag == 102 for tag, _ in decoded.unknown)


# ---------------------------------------------------------------------------
# is_st0601_typed_tag predicate covers every pack & list tag
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "tag", [81, 102, 115, 116, 121, 122, 127, 128, 130, 138, 140, 141, 142, 143]
)
def test_wpc_typed_tags_dropped_from_unknown_on_collision(tag):
    """Every pack & list tag must be recognized as typed by
    `is_st0601_typed_tag` — a caller-supplied `unknown` entry at that tag
    is silently dropped (typed wins) rather than surviving into the
    encoded/decoded record. If the predicate missed a tag, an entry
    supplied via `unknown` would slip past the filter and hit
    the real Rust encoder's own (stricter) `ReservedTagInUnknown` check
    instead — this test would have failed with an unexpected raise."""
    rec = UasDatalinkLs(unknown=((tag, b"\x01"),))
    encoded = encode_uas_datalink(rec)  # must NOT raise
    decoded = decode_uas_datalink(encoded)
    assert not any(t == tag for t, _ in decoded.unknown)


def test_deprecated_tag_66_and_stand_in_tag_200_stay_untyped():
    # 66 (deprecated-forever) and 200 (out of range) are the durable
    # unknown-tag test stand-ins — encoding must NOT reject them.
    rec = UasDatalinkLs(unknown=((66, b"\xde\xad"), (200, b"\xbe\xef")))
    encoded = encode_uas_datalink(rec)
    back = decode_uas_datalink(encoded)
    assert (66, b"\xde\xad") in back.unknown
    assert (200, b"\xbe\xef") in back.unknown


# ---------------------------------------------------------------------------
# klv::st1010 SDCC-FLP — general-purpose module, standalone entry points
# ---------------------------------------------------------------------------


def test_decode_sdcc_flp_mode1_golden():
    # Hand-derived Mode-1 golden (ST 1010.1 back-compat): correlations
    # are always ST 1201 in Mode 1; std devs assumed IEEE.
    v = _hex("03 43 3F800000 40000000 40800000 600000 400000 200000")
    m = decode_sdcc_flp(v)
    assert m.matrix_size == 3
    assert m.std_devs == (1.0, 2.0, 4.0)
    assert abs(m.correlations[0] - 0.5) < 1e-6
    assert abs(m.correlations[1] - 0.0) < 1e-6
    assert abs(m.correlations[2] - -0.5) < 1e-6


def test_decode_sdcc_flp_mode2_full_3x3_ieee_golden():
    v = _hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000")
    m = decode_sdcc_flp(v)
    assert m.matrix_size == 3
    assert m.std_devs == (1.0, 2.0, 4.0)
    assert m.correlations == (0.5, 0.0, -0.5)
    assert m.correlation_present == (True, True, True)
    assert m.correlation(2, 0) == 0.0  # symmetry accessor


def test_sdcc_flp_correlation_diagonal_returns_std_dev():
    v = _hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000")
    m = decode_sdcc_flp(v)
    assert m.correlation(0, 0) == 1.0
    assert m.correlation(1, 1) == 2.0
    assert m.correlation(2, 2) == 4.0


def test_sdcc_flp_correlation_slen_zero_offdiagonal_still_works():
    # Mode 2, N=3, Slen=0 (no std-dev data at all — spec-legal), Clen=4
    # IEEE correlations.
    v = _hex("03 84 00 3F000000 00000000 BF000000")
    m = decode_sdcc_flp(v)
    assert m.std_devs == ()
    # `correlations` is always full-triangle-sized regardless of Slen, so
    # off-diagonal access must succeed even with no std devs.
    assert m.correlation(0, 1) == 0.5
    assert m.correlation(2, 0) == 0.0


def test_sdcc_flp_correlation_slen_zero_diagonal_raises_index_error():
    v = _hex("03 84 00 3F000000 00000000 BF000000")
    m = decode_sdcc_flp(v)
    # i==0 is well within matrix_size=3 — this must be the documented
    # "no std-dev data" error, not a plain out-of-bounds one.
    with pytest.raises(IndexError, match="no standard-deviation value"):
        m.correlation(0, 0)


def test_sdcc_flp_correlation_out_of_bounds_raises_index_error():
    v = _hex("03 84 04 3F800000 40000000 40800000 3F000000 00000000 BF000000")
    m = decode_sdcc_flp(v)
    with pytest.raises(IndexError, match="index out of bounds"):
        m.correlation(3, 0)


def test_decode_sdcc_flp_sparse_bit_vector_golden():
    # N=3 sparse, only rho13=0.25 present.
    v = _hex("03 A4 04 40 3F800000 40000000 40800000 3E800000")
    m = decode_sdcc_flp(v)
    assert m.correlations == (0.0, 0.25, 0.0)
    assert m.correlation_present == (False, True, False)


def test_decode_sdcc_flp_malformed_raises_klv_error():
    with pytest.raises(KlvError):
        decode_sdcc_flp(b"")


def test_encode_sdcc_flp_mode2_round_trips():
    encoded = encode_sdcc_flp_mode2([1.0, 2.0, 4.0], [0.5, 0.0, -0.5], 2)
    m = decode_sdcc_flp(encoded)
    assert m.std_devs == (1.0, 2.0, 4.0)
    assert abs(m.correlations[0] - 0.5) < 1e-3  # IMAPB(-1,1,2) quantization


def test_encode_sdcc_flp_mode2_mismatched_correlations_length_raises():
    with pytest.raises(KlvEncodeError):
        encode_sdcc_flp_mode2([1.0, 2.0, 4.0], [0.5, 0.0], 2)


# ---------------------------------------------------------------------------
# Dataclass field sanity (defaults, direct construction)
# ---------------------------------------------------------------------------


def test_bare_uas_datalink_ls_wpc_fields_default_to_absent():
    ls = UasDatalinkLs()
    assert ls.image_horizon is None
    assert ls.control_commands == ()
    assert ls.control_command_verification is None
    assert ls.active_wavelengths is None
    assert ls.sensor_frame_rate is None
    assert ls.metadata_substream_id is None
    assert ls.country_codes is None
    assert ls.wavelengths_list is None
    assert ls.airbase_locations is None
    assert ls.payload_list is None
    assert ls.weapons_stores is None
    assert ls.waypoint_list is None
    assert ls.view_domain is None
    assert ls.sdcc_flps == ()


def test_view_domain_pair_and_location_direct_construction():
    pair = ViewDomainPair(start_deg=10.0, range_deg=20.0)
    assert ViewDomain(azimuth=pair).azimuth == pair
    loc = Location(lat_deg=1.0, lon_deg=2.0, hae_m=3.0)
    assert AirbaseLocations(take_off=loc).take_off == loc


def test_sdcc_flp_field_direct_construction():
    f = SdccFlpField(preceding_tags=(5, 6, 7), bytes=b"\x03\x84\x04")
    assert f.preceding_tags == (5, 6, 7)
    assert f.bytes == b"\x03\x84\x04"


def test_payload_list_and_weapons_store_direct_construction():
    pl = PayloadList(count=1, records=(PayloadRecord(id=0, payload_type=PayloadType.SAR, name="x"),))
    assert pl.records[0].payload_type == PayloadType.SAR

    ws = WeaponsStore(
        station_id=1,
        hardpoint_id=1,
        carriage_id=1,
        store_id=3,
        status_raw=0b0000_0001_0000_0011,  # general_status=3, fuze bit (bit 8) set
        weapon_type="Harpoon",
    )
    assert ws.general_status == 0b0000_0011
    assert ws.fuze_enabled
    assert not ws.laser_enabled
