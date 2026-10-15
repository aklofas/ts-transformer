"""parse_klv_universal — dispatch by 16-byte SMPTE UL prefix."""

import pytest

from tstrans.exceptions import KlvError, KlvErrorKind
from tstrans.klv import (
    PRECISION_TIMESTAMP_PACK_UL,
    PrecisionTimeStampPack,
    SECURITY_LS_UL,
    SecurityClassification,
    SecurityLs,
    ST_0601_UL,
    UasDatalinkLs,
    VMTI_LS_UL,
    VmtiLs,
    parse_klv_universal,
)

from _builders.klv_tlv import tlv as _tlv
from _builders.klv_tlv import wrap_with_ul as _wrap_with_ul


def test_dispatches_st0605_pack():
    pack = (
        PRECISION_TIMESTAMP_PACK_UL
        + bytes([0x09, 0x1F])
        + (1_700_000_000_000_000).to_bytes(8, "big")
    )
    result = parse_klv_universal(pack)
    assert isinstance(result, PrecisionTimeStampPack)
    assert result.timestamp_us == 1_700_000_000_000_000


def test_dispatches_st0102_standalone():
    body = (
        _tlv(1, bytes([SecurityClassification.UNCLASSIFIED.value]))
        + _tlv(2, bytes([0x01]))
        + _tlv(3, b"//US")
        + _tlv(12, bytes([0x02]))
        + _tlv(13, bytes.fromhex("feff00550053"))
        + _tlv(22, (1).to_bytes(2, "big"))
    )
    buf = _wrap_with_ul(SECURITY_LS_UL, body)
    result = parse_klv_universal(buf)
    assert isinstance(result, SecurityLs)
    assert result.security_classification is SecurityClassification.UNCLASSIFIED


def test_dispatches_st0601_lenient():
    from pathlib import Path

    fx = (
        Path(__file__).parent.parent.parent.parent
        / "crates" / "tst-core" / "tests" / "fixtures" / "st0601" / "synthetic_minimal.klv"
    )
    assert fx.is_file(), f"checked-in fixture missing: {fx}"
    buf = fx.read_bytes()
    result = parse_klv_universal(buf)
    assert isinstance(result, UasDatalinkLs)


def test_dispatches_st0903_standalone_empty():
    buf = _wrap_with_ul(VMTI_LS_UL, b"")
    result = parse_klv_universal(buf)
    assert isinstance(result, VmtiLs)
    assert result.targets == ()


def test_unknown_ul_returns_none():
    fake_ul = b"\x06\x0E\x2B\x34" + b"\xAA" * 12
    buf = _wrap_with_ul(fake_ul, b"hello")
    assert parse_klv_universal(buf) is None


def test_short_buffer_raises():
    with pytest.raises(KlvError) as excinfo:
        parse_klv_universal(b"\x06\x0E\x2B\x34")
    assert excinfo.value.kind is KlvErrorKind.BAD_UNIVERSAL_LABEL


def test_empty_buffer_raises():
    with pytest.raises(KlvError) as excinfo:
        parse_klv_universal(b"")
    assert excinfo.value.kind is KlvErrorKind.BAD_UNIVERSAL_LABEL


def test_returns_typed_isinstance():
    pack = (
        PRECISION_TIMESTAMP_PACK_UL
        + bytes([0x09, 0x1F])
        + (1).to_bytes(8, "big")
    )
    result = parse_klv_universal(pack)
    assert isinstance(
        result,
        (UasDatalinkLs, SecurityLs, PrecisionTimeStampPack, VmtiLs),
    )
