"""ST 0903.6 VMTI Local Set — Python wrap tests.

Generates a minimal body-only synthetic fixture inline; standalone
VMTI carriage (with UL prefix + outer BER length) is exercised via
parse_klv_universal."""

import pytest

from tstrans.exceptions import KlvError, KlvErrorKind
from tstrans.klv import Klv0903, VmtiLs, VTargetPack, decode_vmti

from _builders.klv_tlv import tlv as _tlv


def _minimal_vmti_body() -> bytes:
    """Tags per ST 0903.6 §6 Table 1 + downstream sanity fields."""
    return (
        _tlv(2, (1_700_000_000_000_000).to_bytes(8, "big"))  # precision_time_stamp
        + _tlv(4, (6).to_bytes(1, "big"))  # vmtiLsVersionNum = 6
        + _tlv(8, (1920).to_bytes(2, "big"))  # frame_width
        + _tlv(9, (1080).to_bytes(2, "big"))  # frame_height
    )


def test_alias_klv0903_is_vmti_ls():
    assert Klv0903 is VmtiLs


def test_decode_empty_body_lenient():
    v = decode_vmti(b"")
    assert isinstance(v, VmtiLs)
    assert v.precision_time_stamp is None
    assert v.targets == ()


def test_decode_minimal_vmti():
    body = _minimal_vmti_body()
    v = decode_vmti(body)
    assert v.precision_time_stamp == 1_700_000_000_000_000
    assert v.version_number == 6
    assert v.frame_width == 1920
    assert v.frame_height == 1080


def test_decode_returns_klv0903_alias():
    v = decode_vmti(_minimal_vmti_body())
    assert isinstance(v, Klv0903)


def test_vmti_ls_targets_is_tuple():
    v = decode_vmti(_minimal_vmti_body())
    assert isinstance(v.targets, tuple)
    assert v.targets == ()


def test_vmti_ls_field_errors_is_tuple():
    v = decode_vmti(_minimal_vmti_body())
    assert isinstance(v.field_errors, tuple)


def test_vmti_ls_unknown_is_tuple():
    v = decode_vmti(_minimal_vmti_body())
    assert isinstance(v.unknown, tuple)


def test_vmti_ls_frozen():
    v = decode_vmti(_minimal_vmti_body())
    with pytest.raises((AttributeError, TypeError)):
        v.frame_width = 999  # type: ignore[misc]


def test_decode_strict_rejects_missing_required():
    with pytest.raises(KlvError) as excinfo:
        decode_vmti(b"", strict=True)
    assert excinfo.value.kind in (
        KlvErrorKind.MISSING_REQUIRED_TAG,
        KlvErrorKind.MALFORMED_BYTES,
        KlvErrorKind.TRUNCATED_SET,
    )


def test_unknown_tag_preserved():
    """Tag 50 isn't in the typed table — should land in `.unknown`."""
    body = _minimal_vmti_body() + _tlv(50, b"hello")
    v = decode_vmti(body)
    assert any(tag == 50 for tag, _ in v.unknown)


def test_vmti_ls_miis_id_is_bytes_not_list():
    """Regression: `Option<Vec<u8>>` translators must emit Python `bytes`,
    not `list[int]`. The dataclass field is typed `bytes | None`."""
    body = _minimal_vmti_body() + _tlv(13, b"\xde\xad\xbe\xef")  # Tag 13 MIIS
    v = decode_vmti(body)
    assert isinstance(v.miis_id, bytes)
    assert v.miis_id == b"\xde\xad\xbe\xef"
