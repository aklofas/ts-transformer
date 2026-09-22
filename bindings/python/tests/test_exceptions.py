"""Exception hierarchy contract: every tstrans error is a TstError;
each domain error has a typed .kind attribute (a Python Enum)."""

import pytest

from tstrans.exceptions import (
    TstError,
    MuxError,
    DemuxError,
    KlvError,
    KlvEncodeError,
    CodecError,
    MuxErrorKind,
    DemuxErrorKind,
    KlvErrorKind,
    KlvEncodeErrorKind,
    CodecErrorKind,
)


def test_tst_error_is_exception():
    assert issubclass(TstError, Exception)


@pytest.mark.parametrize("cls", [MuxError, DemuxError, KlvError, KlvEncodeError, CodecError])
def test_domain_errors_inherit_tst_error(cls):
    assert issubclass(cls, TstError)


def test_mux_error_carries_kind_and_message():
    err = MuxError(kind=MuxErrorKind.CONFIG_INVALID, message="missing pcr_pid")
    assert err.kind is MuxErrorKind.CONFIG_INVALID
    assert err.message == "missing pcr_pid"
    assert "missing pcr_pid" in str(err)


def test_codec_error_carries_codec_label():
    err = CodecError(
        kind=CodecErrorKind.UNSUPPORTED_PROFILE,
        message="profile 244 not supported",
        codec="h264",
    )
    assert err.codec == "h264"
    assert err.kind is CodecErrorKind.UNSUPPORTED_PROFILE


def test_specific_error_catchable_as_base_class():
    try:
        raise MuxError(kind=MuxErrorKind.CONFIG_INVALID, message="x")
    except TstError as e:
        assert isinstance(e, MuxError)


def test_demux_error_carries_kind_and_message():
    err = DemuxError(kind=DemuxErrorKind.SYNC_LOSS, message="lost sync at byte 12345")
    assert err.kind is DemuxErrorKind.SYNC_LOSS
    # SYNC_LOSS is the 0.7.x deprecated alias for the canonical member.
    assert err.kind is DemuxErrorKind.SYNC_BUF_EXHAUSTED
    assert err.message == "lost sync at byte 12345"
    assert "lost sync" in str(err)


def test_klv_error_carries_kind_and_message():
    err = KlvError(kind=KlvErrorKind.TRUNCATED_SET, message="set truncated at 47/100")
    assert err.kind is KlvErrorKind.TRUNCATED_SET
    assert err.message == "set truncated at 47/100"
    assert "truncated" in str(err)


# ---------------------------------------------------------------------------
# Phase 4: MuxError + KlvEncodeError refinement
# ---------------------------------------------------------------------------


def test_mux_error_class_exists_and_is_tst_error():
    assert issubclass(MuxError, TstError)
    # Mirrors Rust `tst_core::error::MuxErrorKind` — 5 variants.
    assert len(list(MuxErrorKind)) >= 5
    assert MuxErrorKind.CONFIG_INVALID.value >= 0


def test_klv_encode_error_class_exists_and_is_tst_error():
    assert issubclass(KlvEncodeError, TstError)
    # Mirrors Rust `tst_core::error::KlvEncodeError` — 8 variants today.
    assert len(list(KlvEncodeErrorKind)) >= 3


def test_mux_error_positional_message_and_kind_kwarg():
    # Phase 4 supports positional-message form alongside the legacy
    # keyword-only form used by `make_mux_error` in errors.rs.
    e = MuxError("bad config", kind=MuxErrorKind.CONFIG_INVALID)
    assert e.kind is MuxErrorKind.CONFIG_INVALID
    assert "bad config" in str(e)
    assert e.message == "bad config"


def test_klv_encode_error_carries_kind_and_optional_tag():
    e = KlvEncodeError(
        "ST 0601 mandatory tag missing",
        kind=KlvEncodeErrorKind.MISSING_MANDATORY_ITEM,
        tag=2,
    )
    assert e.kind is KlvEncodeErrorKind.MISSING_MANDATORY_ITEM
    assert e.tag == 2
    assert "mandatory" in str(e)


def test_klv_encode_error_kind_default_when_no_tag():
    e = KlvEncodeError(
        "output buffer too small",
        kind=KlvEncodeErrorKind.BUFFER_TOO_SMALL,
    )
    assert e.kind is KlvEncodeErrorKind.BUFFER_TOO_SMALL
    assert e.tag is None
