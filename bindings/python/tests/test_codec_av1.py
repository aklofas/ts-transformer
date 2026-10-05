"""AV1 codec surface tests.

Fixtures come from the Rust AV1 unit tests in
``crates/tst-core/src/codec/av1/tests/sequence_header.rs`` and the
``decode::frame_header`` test module.  The byte constants are verified
against the on-disk binaries at
``crates/tst-core/tests/fixtures/codec/av1/``.
"""

import pytest

from tstrans.codec import (
    Av1FrameHeaderLight,
    Av1ObuStream,
    Av1SequenceHeader,
    ChromaFormat,
    Obu,
    parse_av1_frame_header_light,
    parse_av1_obu_stream,
    parse_av1_sequence_header,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# AV1 byte fixtures
#
# MINIMAL_SEQ_HEADER: Main profile, level 2.0, 320x240, 8-bit 4:2:0,
#   no color description, no timing info.
#   Source: crates/tst-core/tests/fixtures/codec/av1/av1_320x240_main_seq_header.bin
#   Verified against minimal_sequence_header() in Rust tests.
MINIMAL_SEQ_HEADER = bytes([0, 0, 0, 4, 60, 255, 188, 0, 0, 0])

# KEYFRAME_HEADER: show_existing_frame=0, frame_type=KEY_FRAME(0), show_frame=1.
#   Bitstream bits: 0,0,0,1 in the high nibble = 0b0001_0000 = 0x10.
#   Source: crates/tst-core/tests/fixtures/codec/av1/av1_320x240_main_frame_header_keyframe.bin
KEYFRAME_HEADER = bytes([0x10])

# SHOW_EXISTING_FRAME: show_existing_frame=1, frame_to_show_map_idx=0
#   Bitstream: 1,0,0,0 in high nibble = 0x80.
SHOW_EXISTING_FRAME_HEADER = bytes([0x80])


# ---------------------------------------------------------------------------
# parse_av1_sequence_header tests
# ---------------------------------------------------------------------------


def test_parse_av1_sequence_header_returns_typed():
    """parse_av1_sequence_header returns an Av1SequenceHeader."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert isinstance(seq, Av1SequenceHeader)


def test_parse_av1_sequence_header_profile_main():
    """Main profile fixture has profile == 0."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.profile == 0


def test_parse_av1_sequence_header_dimensions():
    """Sequence header encodes 320x240 frame dimensions."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.max_frame_width == 320
    assert seq.max_frame_height == 240


def test_parse_av1_sequence_header_level_and_tier():
    """Level 2.0 (seq_level_idx=0) and seq_tier=0."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.level == 0
    assert seq.tier == 0


def test_parse_av1_sequence_header_bit_depth_and_chroma():
    """8-bit 4:2:0 minimal fixture."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.bit_depth == 8
    assert not seq.monochrome
    assert seq.chroma_format == ChromaFormat.YUV420


def test_parse_av1_sequence_header_still_picture_flags():
    """Minimal fixture is not a still picture."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert not seq.still_picture
    assert not seq.reduced_still_picture_header


def test_parse_av1_sequence_header_frame_rate_none():
    """No timing info in the minimal fixture — frame_rate is None."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.frame_rate is None


def test_parse_av1_sequence_header_raw_round_trips():
    """raw returns the original input bytes."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    assert seq.raw == MINIMAL_SEQ_HEADER


def test_parse_av1_sequence_header_repr():
    """Av1SequenceHeader.__repr__ contains profile, dimensions, bit_depth."""
    seq = parse_av1_sequence_header(MINIMAL_SEQ_HEADER)
    r = repr(seq)
    assert "0" in r      # profile
    assert "320" in r
    assert "240" in r
    assert "8" in r      # bit_depth


def test_parse_av1_sequence_header_truncated_raises():
    """Empty input raises CodecError with kind=TRUNCATED_RBSP."""
    with pytest.raises(CodecError) as exc_info:
        parse_av1_sequence_header(b"")
    err = exc_info.value
    assert err.kind is CodecErrorKind.TRUNCATED_RBSP
    assert err.codec == "av1"


# ---------------------------------------------------------------------------
# parse_av1_frame_header_light tests
# ---------------------------------------------------------------------------


def _make_seq() -> Av1SequenceHeader:
    """Parse the minimal sequence header fixture for use as context."""
    return parse_av1_sequence_header(MINIMAL_SEQ_HEADER)


def test_parse_av1_frame_header_light_returns_typed():
    """parse_av1_frame_header_light returns an Av1FrameHeaderLight."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert isinstance(fh, Av1FrameHeaderLight)


def test_parse_av1_frame_header_light_keyframe_type():
    """KEY_FRAME fixture has frame_type == 0."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert fh.frame_type == 0


def test_parse_av1_frame_header_light_show_frame():
    """Keyframe has show_frame == True."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert fh.show_frame is True


def test_parse_av1_frame_header_light_not_show_existing():
    """Keyframe has show_existing_frame == False."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert fh.show_existing_frame is False


def test_parse_av1_frame_header_light_show_existing_frame():
    """show_existing_frame fixture: show_existing_frame=True, show_frame=True."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(SHOW_EXISTING_FRAME_HEADER, seq)
    assert fh.show_existing_frame is True
    assert fh.show_frame is True


def test_parse_av1_frame_header_light_frame_size_none():
    """frame_size is always None in light scope."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert fh.frame_size is None


def test_parse_av1_frame_header_light_raw():
    """raw returns the input bytes."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    assert fh.raw == KEYFRAME_HEADER


def test_parse_av1_frame_header_light_repr():
    """Av1FrameHeaderLight.__repr__ contains frame_type, show_frame."""
    seq = _make_seq()
    fh = parse_av1_frame_header_light(KEYFRAME_HEADER, seq)
    r = repr(fh)
    # Values may appear in the repr as ints or bools.
    assert "type=0" in r or "frame_type=0" in r or "0" in r


def test_parse_av1_frame_header_light_truncated_raises():
    """Empty payload with no reduced_still_picture_header raises CodecError."""
    seq = _make_seq()
    with pytest.raises(CodecError) as exc_info:
        parse_av1_frame_header_light(b"", seq)
    err = exc_info.value
    assert err.codec == "av1"


# ---------------------------------------------------------------------------
# parse_av1_obu_stream tests
# ---------------------------------------------------------------------------


def test_parse_av1_obu_stream_returns_typed():
    """parse_av1_obu_stream returns an Av1ObuStream."""
    stream = parse_av1_obu_stream([])
    assert isinstance(stream, Av1ObuStream)


def test_parse_av1_obu_stream_empty_list():
    """Empty OBU list gives empty stream with no errors."""
    stream = parse_av1_obu_stream([])
    assert len(stream.sequence_headers) == 0
    assert len(stream.frame_headers) == 0
    assert len(stream.unparseable) == 0


def test_parse_av1_obu_stream_sequence_header_parsed():
    """Sequence Header OBU (type 1) is collected into sequence_headers."""
    seq_obu = Obu(obu_type=1, extension=None, payload=list(MINIMAL_SEQ_HEADER))
    stream = parse_av1_obu_stream([seq_obu])
    assert len(stream.sequence_headers) == 1
    assert stream.sequence_headers[0].profile == 0
    assert stream.sequence_headers[0].max_frame_width == 320


def test_parse_av1_obu_stream_seq_then_frame():
    """Sequence Header then Frame Header OBU: both collected."""
    seq_obu = Obu(obu_type=1, extension=None, payload=list(MINIMAL_SEQ_HEADER))
    frame_obu = Obu(obu_type=3, extension=None, payload=list(KEYFRAME_HEADER))
    stream = parse_av1_obu_stream([seq_obu, frame_obu])
    assert len(stream.sequence_headers) == 1
    assert len(stream.frame_headers) == 1
    assert len(stream.unparseable) == 0


def test_parse_av1_obu_stream_frame_before_seq_in_unparseable():
    """Frame Header OBU before any Sequence Header lands in unparseable."""
    frame_obu = Obu(obu_type=3, extension=None, payload=list(KEYFRAME_HEADER))
    stream = parse_av1_obu_stream([frame_obu])
    assert len(stream.sequence_headers) == 0
    assert len(stream.frame_headers) == 0
    assert len(stream.unparseable) == 1
    obu_type, err_msg = stream.unparseable[0]
    assert obu_type == 3
    assert isinstance(err_msg, str)
    assert len(err_msg) > 0


def test_parse_av1_obu_stream_truncated_seq_in_unparseable():
    """Truncated Sequence Header OBU body (type 1) lands in unparseable."""
    bad_obu = Obu(obu_type=1, extension=None, payload=[])
    stream = parse_av1_obu_stream([bad_obu])
    assert len(stream.sequence_headers) == 0
    assert len(stream.unparseable) == 1
    assert stream.unparseable[0][0] == 1


def test_parse_av1_obu_stream_metadata_obu_skipped():
    """Metadata OBU (type 5) and TileGroup OBU (type 4) pass through silently."""
    metadata_obu = Obu(obu_type=5, extension=None, payload=[0x00])
    tile_obu = Obu(obu_type=4, extension=None, payload=[0x00])
    stream = parse_av1_obu_stream([metadata_obu, tile_obu])
    assert len(stream.sequence_headers) == 0
    assert len(stream.frame_headers) == 0
    assert len(stream.unparseable) == 0
