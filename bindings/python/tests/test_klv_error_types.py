"""KlvErrorKind / KlvFieldErrorKind / KlvFieldError shape tests."""

import pytest

from tstrans.exceptions import KlvError, KlvErrorKind
from tstrans.klv import KlvFieldError, KlvFieldErrorKind


def test_klv_error_kind_has_new_variants():
    expected = {
        "BAD_UNIVERSAL_LABEL",
        "TRUNCATED_SET",
        "CHECKSUM_MISMATCH",
        "DUPLICATE_TAG",
        "MISSING_REQUIRED_TAG",
        "MALFORMED_BYTES",
        "INTERNAL",
    }
    actual = {v.name for v in KlvErrorKind}
    assert expected == actual


def test_klv_field_error_kind_variants():
    expected = {
        "OUT_OF_RANGE",
        "INVALID_UTF8",
        "INVALID_UTF16",
        "INVALID_LENGTH",
        "INVALID_CODEPOINT",
        "TRUNCATED_FIELD",
        "UNSUPPORTED_IMAPB_LENGTH",
        "INVALID_IMAPB_PARAMS",
    }
    actual = {v.name for v in KlvFieldErrorKind}
    assert expected == actual


def test_klv_field_error_construction():
    fe = KlvFieldError(
        kind=KlvFieldErrorKind.OUT_OF_RANGE,
        tag=5,
        message="value 999 out of range [0, 360]",
    )
    assert fe.kind is KlvFieldErrorKind.OUT_OF_RANGE
    assert fe.tag == 5
    assert "out of range" in fe.message


def test_klv_field_error_is_frozen_dataclass():
    fe = KlvFieldError(
        kind=KlvFieldErrorKind.INVALID_UTF8,
        tag=13,
        message="malformed UTF-8",
    )
    with pytest.raises((AttributeError, TypeError)):
        fe.tag = 99  # type: ignore[misc]


def test_klv_field_error_equality():
    a = KlvFieldError(kind=KlvFieldErrorKind.OUT_OF_RANGE, tag=5, message="x")
    b = KlvFieldError(kind=KlvFieldErrorKind.OUT_OF_RANGE, tag=5, message="x")
    c = KlvFieldError(kind=KlvFieldErrorKind.OUT_OF_RANGE, tag=6, message="x")
    assert a == b
    assert a != c


def test_klv_error_still_constructible():
    # Confirm Phase 0+1 KlvError construction still works with the
    # extended enum.
    err = KlvError(kind=KlvErrorKind.CHECKSUM_MISMATCH, message="bad checksum")
    assert err.kind is KlvErrorKind.CHECKSUM_MISMATCH
    assert err.message == "bad checksum"
    assert isinstance(err, Exception)
