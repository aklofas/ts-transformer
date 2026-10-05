"""parse_klv_universal must reject trailing bytes after the
declared outer BER length for ST 0102 and ST 0903 (it already does for
ST 0601 / ST 0605 by virtue of strict family decoders)."""

import pytest
from tstrans.klv import (
    SECURITY_LS_UL,
    VMTI_LS_UL,
    SecurityClassification,
    SecurityLs,
    VmtiLs,
    parse_klv_universal,
)
from tstrans.exceptions import KlvError, KlvErrorKind

from _builders.klv_tlv import tlv as _tlv


def _ber_long(n: int) -> bytes:
    if n < 0x80:
        return bytes([n])
    for nbytes in range(1, 9):
        if n < (1 << (8 * nbytes)):
            return bytes([0x80 | nbytes]) + n.to_bytes(nbytes, "big")
    raise ValueError("length too large for BER")


def _wrap_with_ul(ul: bytes, body: bytes) -> bytes:
    return ul + _ber_long(len(body)) + body


def _minimal_st0102_body() -> bytes:
    """Build a minimal valid ST 0102 body (Security Classification = UNCLASSIFIED
    plus required tag 12/13/22 fields the decoder expects)."""
    return (
        _tlv(1, bytes([SecurityClassification.UNCLASSIFIED.value]))
        + _tlv(2, bytes([0x01]))
        + _tlv(3, b"//US")
        + _tlv(12, bytes([0x02]))
        + _tlv(13, bytes.fromhex("feff00550053"))
        + _tlv(22, (1).to_bytes(2, "big"))
    )


def _minimal_st0102() -> bytes:
    """Complete ST 0102 universal record (UL + BER length + body)."""
    body = _minimal_st0102_body()
    return _wrap_with_ul(SECURITY_LS_UL, body)


def _minimal_st0903() -> bytes:
    """Minimal ST 0903 universal record with empty targets list."""
    return _wrap_with_ul(VMTI_LS_UL, b"")


# ── ST 0102 tests ──────────────────────────────────────────────────────────────

def test_st0102_clean_decode_passes() -> None:
    """Sanity — a well-formed ST 0102 record must decode without error."""
    result = parse_klv_universal(_minimal_st0102())
    assert isinstance(result, SecurityLs)


def test_st0102_rejects_trailing_bytes() -> None:
    """Trailing bytes after the declared body must raise
    KlvError(kind=MALFORMED_BYTES), not silently succeed."""
    payload = _minimal_st0102() + b"\xde\xad\xbe\xef"
    with pytest.raises(KlvError) as ei:
        parse_klv_universal(payload)
    assert ei.value.kind is KlvErrorKind.MALFORMED_BYTES, (
        f"expected MALFORMED_BYTES, got {ei.value.kind!r}"
    )


def test_st0102_rejects_single_trailing_byte() -> None:
    """One trailing byte is enough to reject — not just multi-byte garbage."""
    payload = _minimal_st0102() + b"\xff"
    with pytest.raises(KlvError) as ei:
        parse_klv_universal(payload)
    assert ei.value.kind is KlvErrorKind.MALFORMED_BYTES


# ── ST 0903 tests ──────────────────────────────────────────────────────────────

def test_st0903_clean_decode_passes() -> None:
    """Sanity — a well-formed ST 0903 record must decode without error."""
    result = parse_klv_universal(_minimal_st0903())
    assert isinstance(result, VmtiLs)


def test_st0903_rejects_trailing_bytes() -> None:
    """Trailing bytes after the declared body must raise
    KlvError(kind=MALFORMED_BYTES)."""
    payload = _minimal_st0903() + b"\xff\xff"
    with pytest.raises(KlvError) as ei:
        parse_klv_universal(payload)
    assert ei.value.kind is KlvErrorKind.MALFORMED_BYTES, (
        f"expected MALFORMED_BYTES, got {ei.value.kind!r}"
    )


def test_st0903_rejects_single_trailing_byte() -> None:
    """One trailing byte is enough to reject."""
    payload = _minimal_st0903() + b"\x00"
    with pytest.raises(KlvError) as ei:
        parse_klv_universal(payload)
    assert ei.value.kind is KlvErrorKind.MALFORMED_BYTES
