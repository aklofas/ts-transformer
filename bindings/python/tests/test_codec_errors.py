"""Phase 5: CodecError + CodecErrorKind shape tests."""

import pytest

from tstrans.exceptions import CodecError, CodecErrorKind, TstError


def test_codec_error_is_tst_error():
    assert issubclass(CodecError, TstError)


def test_codec_error_kind_enum_variants_present():
    expected = {
        "TRUNCATED_RBSP",
        "INVALID_GOLOMB",
        "RESERVED_VALUE",
        "UNSUPPORTED_PROFILE",
        "DANGLING_SPS_REFERENCE",
        "DANGLING_VPS_REFERENCE",
        "ENGINE_ERROR",
        "INVALID_LEB128",
        "BAD_SYNC_WORD",
        "TRUNCATED",
        "FORBIDDEN",
        "UNSUPPORTED_FREE_FORMAT",
        "INVALID_LENGTH_SIZE",
        "NAL_LENGTH_OVERFLOW",
        "BUFFER_TOO_SMALL",
    }
    actual = {v.name for v in CodecErrorKind}
    assert expected.issubset(actual), f"missing: {expected - actual}"


def test_codec_error_construct_truncated_rbsp():
    err = CodecError(
        kind=CodecErrorKind.TRUNCATED_RBSP,
        codec="h264",
        message="truncated at bit 42",
        offset_bits=42,
        needed_bits=8,
    )
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "h264"
    assert err.offset_bits == 42
    assert err.needed_bits == 8


def test_codec_error_construct_bad_sync_word():
    err = CodecError(
        kind=CodecErrorKind.BAD_SYNC_WORD,
        codec="aac",
        message="bad sync",
        expected=0xFFF,
        found=0x000,
    )
    assert err.kind is CodecErrorKind.BAD_SYNC_WORD
    assert err.codec == "aac"
    assert err.expected == 0xFFF
    assert err.found == 0x000


def test_codec_error_str_representation():
    err = CodecError(
        kind=CodecErrorKind.TRUNCATED_RBSP,
        codec="h264",
        message="truncated at bit 42",
        offset_bits=42,
        needed_bits=8,
    )
    s = str(err)
    assert "h264" in s
    assert "truncated" in s.lower()
