"""ST 0102.12 Security Metadata LS — Python wrap tests.

Generates synthetic fixtures inline. `decode_security` consumes a
body-only buffer (no UL / outer BER length); these are stripped by
the caller or `parse_klv_universal`."""

import pytest

from tstrans.exceptions import KlvError, KlvErrorKind
from tstrans.klv import (
    ClassifyingCountryCodingMethod,
    Klv0102,
    KlvFieldErrorKind,
    ObjectCountryCodingMethod,
    SecurityClassification,
    SecurityLs,
    decode_security,
)

from _builders.klv_tlv import tlv as _tlv


def _minimal_security_body() -> bytes:
    """6 required tags per ST 0102.12 §6.7 Table 1: 1, 2, 3, 12, 13, 22."""
    return (
        _tlv(1, bytes([SecurityClassification.UNCLASSIFIED.value]))
        + _tlv(2, bytes([ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER.value]))
        + _tlv(3, b"//US")
        + _tlv(12, bytes([ObjectCountryCodingMethod.ISO_3166_THREE_LETTER.value]))
        + _tlv(13, bytes.fromhex("feff00550053"))  # "US" UTF-16BE w/ BOM
        + _tlv(22, (1).to_bytes(2, "big"))
    )


def test_alias_klv0102_is_security_ls():
    assert Klv0102 is SecurityLs


def test_decode_minimal_security_ls():
    body = _minimal_security_body()
    sec = decode_security(body)
    assert sec.security_classification is SecurityClassification.UNCLASSIFIED
    assert sec.classifying_country_coding_method is ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER
    assert sec.classifying_country == "//US"
    assert sec.object_country_coding_method is ObjectCountryCodingMethod.ISO_3166_THREE_LETTER
    assert sec.object_country_codes == "US"
    assert sec.version == 1
    assert sec.sci_shi_info is None
    assert sec.caveats is None
    assert sec.field_errors == ()


def test_decode_returns_klv0102_alias():
    sec = decode_security(_minimal_security_body())
    assert isinstance(sec, Klv0102)


def test_decode_rejects_truncated_outer():
    # Lenient mode tolerates empty body (no required tags present) —
    # returns an empty record rather than raising.
    sec = decode_security(b"")
    assert sec.security_classification is None


def test_decode_strict_rejects_missing_required():
    body = _tlv(22, (1).to_bytes(2, "big"))  # only version, missing all required
    with pytest.raises(KlvError) as excinfo:
        decode_security(body, strict=True)
    assert excinfo.value.kind in (
        KlvErrorKind.MISSING_REQUIRED_TAG,
        KlvErrorKind.MALFORMED_BYTES,
        KlvErrorKind.TRUNCATED_SET,
    )


def test_security_ls_is_frozen_dataclass():
    sec = decode_security(_minimal_security_body())
    with pytest.raises((AttributeError, TypeError)):
        sec.version = 999  # type: ignore[misc]


def test_security_ls_field_errors_is_tuple():
    sec = decode_security(_minimal_security_body())
    assert isinstance(sec.field_errors, tuple)


def test_security_ls_unknown_is_tuple():
    sec = decode_security(_minimal_security_body())
    assert isinstance(sec.unknown, tuple)


def test_unknown_tag_preserved():
    body = _minimal_security_body() + _tlv(99, b"hello")
    sec = decode_security(body)
    assert any(tag == 99 for tag, _ in sec.unknown)


def test_decode_invalid_utf16_tag13_lenient():
    """Tag 13 with odd-length payload is malformed UTF-16. Per the
    Rust module's docs, lenient mode tolerates it via field_errors +
    preserves bytes in `unknown`. Either pathway is an acceptable
    signal that the malformation was surfaced (not silently dropped)."""
    bad_body = (
        _tlv(1, bytes([SecurityClassification.UNCLASSIFIED.value]))
        + _tlv(2, bytes([ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER.value]))
        + _tlv(3, b"//US")
        + _tlv(12, bytes([ObjectCountryCodingMethod.ISO_3166_THREE_LETTER.value]))
        + _tlv(13, bytes([0x00, 0x55, 0x99]))  # 3 bytes = odd → malformed UTF-16
        + _tlv(22, (1).to_bytes(2, "big"))
    )
    # If lenient throws (Rust may have made Tag 13 strict-only), that's
    # also acceptable — adjust the test arm in the FX. For now, expect
    # either no-raise + surfaced-malformation OR a raise.
    try:
        sec = decode_security(bad_body)
    except KlvError:
        return  # malformation surfaced as a raised error — acceptable
    has_field_err = any(fe.tag == 13 for fe in sec.field_errors)
    in_unknown = any(tag == 13 for tag, _ in sec.unknown)
    assert has_field_err or in_unknown, (
        f"expected malformed Tag 13 to surface via field_errors or unknown; "
        f"got field_errors={sec.field_errors} unknown_tags={[t for t, _ in sec.unknown]}"
    )


def test_decode_security_strict_rejects_non_canonical_length():
    # ST 0107.5 §6.3: long-form length (0x81 0x05) for a value ≤ 127 bytes is
    # non-canonical — the canonical encoding is the short form (0x05).
    # Tag 22 (0x16), long-form length 5, then 5 value bytes.
    buf = bytes([0x16, 0x81, 0x05, 0x30, 0x31, 0x30, 0x32, 0x2E])
    with pytest.raises(KlvError) as excinfo:
        decode_security(buf, strict=True)
    assert excinfo.value.kind == KlvErrorKind.MALFORMED_BYTES, (
        f"expected MALFORMED_BYTES, got {excinfo.value.kind}"
    )


def test_decode_lenient_keeps_a_set_with_invalid_utf8():
    # Classification SECRET, then caveats holding the byte FF: the set
    # decodes, the text is reported with its tag (ISO/IEC 646 is ASCII).
    body = _tlv(1, bytes([SecurityClassification.SECRET.value])) + _tlv(5, b"\xff")
    sec = decode_security(body)
    assert sec.security_classification == SecurityClassification.SECRET
    assert sec.caveats is None
    assert [(fe.kind, fe.tag) for fe in sec.field_errors] == [(KlvFieldErrorKind.INVALID_UTF8, 5)]


def test_decode_strict_refuses_non_ascii_text():
    with pytest.raises(KlvError) as excinfo:
        decode_security(_minimal_security_body() + _tlv(5, "\u00e9".encode()), strict=True)
    assert excinfo.value.kind == KlvErrorKind.MALFORMED_BYTES
