"""ST 0102 + ST 0605 encode round-trip tests."""

import pytest

from tstrans.exceptions import KlvEncodeError, KlvEncodeErrorKind
from tstrans.klv import (
    ClassifyingCountryCodingMethod,
    ObjectCountryCodingMethod,
    PrecisionTimeStampPack,
    SecurityClassification,
    SecurityLs,
    TimeStatus,
    decode_precision_timestamp,
    decode_security,
    encode_precision_timestamp,
    encode_security,
    encode_security_strict_compliance,
)


# ---------------------------------------------------------------------------
# ST 0102 — encode_security
# ---------------------------------------------------------------------------


def test_encode_security_round_trip_basic():
    # Minimal ST 0102 body: Tag 1 (Security Classification) = 1 byte (0x01 =
    # UNCLASSIFIED). Lenient decode tolerates missing required tags.
    body = b"\x01\x01\x01"
    rec = decode_security(body)
    out = encode_security(rec)
    assert isinstance(out, bytes)
    rec2 = decode_security(out)
    assert rec2.security_classification == rec.security_classification
    assert rec2.security_classification == SecurityClassification.UNCLASSIFIED


def test_encode_security_returns_bytes_not_bytearray():
    rec = decode_security(b"\x01\x01\x01")
    out = encode_security(rec)
    assert isinstance(out, bytes)


def test_encode_security_empty_record_returns_empty_body():
    # All-None record encodes to a zero-length body.
    rec = SecurityLs()
    out = encode_security(rec)
    assert isinstance(out, bytes)
    assert out == b""


def test_encode_security_full_round_trip_preserves_typed_enums_and_strings():
    # Hand-build a record with all 3 typed-enum fields + a UTF-16 string field.
    rec = SecurityLs(
        security_classification=SecurityClassification.SECRET,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_NUMERIC,
        object_country_codes="US;CA",
        version=12,
        caveats="NOFORN",
        classified_by="OCA: Some Body",
    )
    out = encode_security(rec)
    assert isinstance(out, bytes)
    assert len(out) > 0
    rec2 = decode_security(out)
    assert rec2.security_classification == SecurityClassification.SECRET
    assert (
        rec2.classifying_country_coding_method
        == ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER
    )
    assert rec2.classifying_country == "//US"
    assert rec2.object_country_coding_method == ObjectCountryCodingMethod.ISO_3166_NUMERIC
    assert rec2.object_country_codes == "US;CA"
    assert rec2.version == 12
    assert rec2.caveats == "NOFORN"
    assert rec2.classified_by == "OCA: Some Body"


# ---------------------------------------------------------------------------
# ST 0102 — encode_security_strict_compliance
# ---------------------------------------------------------------------------


def _full_security_record() -> SecurityLs:
    """A record satisfying all 6 ST 0102 required tags (1,2,3,12,13,22)."""
    return SecurityLs(
        security_classification=SecurityClassification.UNCLASSIFIED,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_NUMERIC,
        object_country_codes="US",
        version=12,
    )


def test_encode_security_strict_compliance_missing_tag1_raises():
    # Omit security_classification (Tag 1) — must raise MISSING_MANDATORY_ITEM.
    rec = SecurityLs(
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_NUMERIC,
        object_country_codes="US",
        version=12,
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_security_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert ei.value.tag == 1


def test_encode_security_strict_compliance_missing_tag22_raises():
    # Omit version (Tag 22) — must raise MISSING_MANDATORY_ITEM with tag=22.
    rec = SecurityLs(
        security_classification=SecurityClassification.UNCLASSIFIED,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_NUMERIC,
        object_country_codes="US",
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_security_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert ei.value.tag == 22


def test_encode_security_strict_compliance_refuses_non_ascii_text_with_its_tag():
    rec = SecurityLs(
        security_classification=SecurityClassification.UNCLASSIFIED,
        classifying_country_coding_method=ClassifyingCountryCodingMethod.ISO_3166_TWO_LETTER,
        classifying_country="//US",
        object_country_coding_method=ObjectCountryCodingMethod.ISO_3166_NUMERIC,
        object_country_codes="US",
        version=12,
        caveats="\u00e9",
    )
    with pytest.raises(KlvEncodeError) as ei:
        encode_security_strict_compliance(rec)
    assert ei.value.kind is KlvEncodeErrorKind.OUT_OF_RANGE
    assert ei.value.tag == 5


def test_encode_security_strict_compliance_full_record_succeeds():
    rec = _full_security_record()
    out = encode_security_strict_compliance(rec)
    assert isinstance(out, bytes)
    assert len(out) > 0
    # Round-trip: decoded record preserves required fields.
    rec2 = decode_security(out)
    assert rec2.security_classification == SecurityClassification.UNCLASSIFIED
    assert rec2.version == 12


# ---------------------------------------------------------------------------
# ST 0605 — encode_precision_timestamp
# ---------------------------------------------------------------------------


def test_encode_precision_timestamp_is_26_bytes():
    pack = PrecisionTimeStampPack(time_status=TimeStatus(raw=0x1F), timestamp_us=0)
    out = encode_precision_timestamp(pack)
    assert isinstance(out, bytes)
    assert len(out) == 26


def test_encode_precision_timestamp_round_trip_preserves_fields():
    pack = PrecisionTimeStampPack(
        time_status=TimeStatus(raw=0x1F),
        timestamp_us=1_700_000_000_000_000,
    )
    out = encode_precision_timestamp(pack)
    pack2 = decode_precision_timestamp(out)
    assert pack2.timestamp_us == pack.timestamp_us
    assert pack2.time_status.raw == pack.time_status.raw
