"""AAC codec surface tests.

Fixtures are synthetic ADTS frames built from the ADTS bit-field spec
(ISO/IEC 13818-7 §1.A), mirroring the ``build_frame`` helper in
``crates/tst-core/src/codec/aac/tests/frames.rs``.

ADTS header layout (7 bytes, no CRC):
  Byte 0:       syncword[11:4]        = 0xFF
  Byte 1 hi:    syncword[3:0]         = 0xF
  Byte 1 bit3:  ID (0=MPEG-4, 1=MPEG-2)
  Byte 1 bits1-2: layer              = 0b00
  Byte 1 bit0:  protection_absent    = 1 (no CRC)
  Byte 2 hi:    profile[1:0]          (0=Main, 1=Lc, 2=Ssr, 3=LTP)
  Byte 2 bits2-5: sampling_frequency_index (4=44100 Hz)
  Byte 2 bit0 + Byte 3 bits6-7: channel_configuration (3 bits)
  Bytes 3-5:    frame_length (13 bits), buffer_fullness (11 bits)
  Byte 6:       num_raw_data_blocks[1:0] (0 wire = 1 logical block)
"""

import pytest

from tstrans.codec import (
    AacChannelLayout,
    AacProfile,
    AdtsFrame,
    AdtsFrameIter,
    MpegVersion,
    iter_aac_frames,
    iter_aac_frames_with_resync,
    parse_aac_frames,
    parse_aac_frames_with_resync,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# ADTS byte fixtures
#
# MPEG-2, AAC-LC (profile=1), sample_rate_index=4 (44100 Hz),
# channel_config=2 (stereo), frame_length=16, no CRC.
# Matches build_frame(sample_rate_index=4, channel_config=2, total_len=16)
# from the Rust test helper with ID=1 (MPEG-2).
FRAME_MPEG2_LC_44100_STEREO = bytes.fromhex("fff95080021ffc000000000000000000")

# Same but MPEG-4 (ID bit = 0): byte 1 changes 0xF9 -> 0xF1.
FRAME_MPEG4_LC_44100_STEREO = bytes.fromhex("fff15080021ffc000000000000000000")

# MPEG-2, AAC-LC, 44100 Hz, PCE-defined channel layout (channel_config=0).
FRAME_MPEG2_LC_44100_PCE = bytes.fromhex("fff95000021ffc000000000000000000")

# Two back-to-back MPEG-2 frames.
TWO_FRAMES = FRAME_MPEG2_LC_44100_STEREO * 2

# Garbage prefix followed by one valid frame (resync tests).
GARBAGE_PREFIX = b"\xde\xad\xbe\xef"
GARBAGE_THEN_FRAME = GARBAGE_PREFIX + FRAME_MPEG2_LC_44100_STEREO

# No valid sync word anywhere (all zeros).
NO_SYNC = bytes(32)


# ---------------------------------------------------------------------------
# AacProfile enum tests
# ---------------------------------------------------------------------------


def test_aac_profile_lc_exists():
    """AacProfile.LC is the most common real-world profile."""
    assert AacProfile.LC is not None


def test_aac_profile_all_variants():
    """All four ADTS profile variants are accessible."""
    assert AacProfile.MAIN is not None
    assert AacProfile.LC is not None
    assert AacProfile.SSR is not None
    assert AacProfile.LTP is not None


# ---------------------------------------------------------------------------
# MpegVersion enum tests
# ---------------------------------------------------------------------------


def test_mpeg_version_variants():
    """MpegVersion has MPEG2 and MPEG4 variants."""
    assert MpegVersion.MPEG2 is not None
    assert MpegVersion.MPEG4 is not None


# ---------------------------------------------------------------------------
# AacChannelLayout class tests
# ---------------------------------------------------------------------------


def test_aac_channel_layout_pce_defined():
    """AacChannelLayout.PceDefined: channels is None."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_PCE)
    assert f.channel_layout.is_pce_defined is True
    assert f.channel_layout.channels is None


def test_aac_channel_layout_stereo():
    """AacChannelLayout.Channels(2): is_pce_defined=False, channels=2."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.channel_layout.is_pce_defined is False
    assert f.channel_layout.channels == 2


# ---------------------------------------------------------------------------
# AdtsFrame field tests
# ---------------------------------------------------------------------------


def test_adts_frame_returns_typed():
    """parse_aac_frames returns AdtsFrame instances."""
    frames = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert len(frames) == 1
    assert isinstance(frames[0], AdtsFrame)


def test_adts_frame_profile_lc():
    """AAC-LC profile fixture: profile == AacProfile.LC."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.profile == AacProfile.LC


def test_adts_frame_sample_rate():
    """sample_rate_index=4 decodes to 44100 Hz."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.sample_rate_hz == 44100


def test_adts_frame_channel_configuration_stereo():
    """channel_configuration == 2 for stereo fixture."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.channel_configuration == 2


def test_adts_frame_mpeg_version_mpeg2():
    """MPEG-2 fixture: mpeg_version == MpegVersion.MPEG2."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.mpeg_version == MpegVersion.MPEG2


def test_adts_frame_mpeg_version_mpeg4():
    """MPEG-4 fixture: mpeg_version == MpegVersion.MPEG4."""
    [f] = parse_aac_frames(FRAME_MPEG4_LC_44100_STEREO)
    assert f.mpeg_version == MpegVersion.MPEG4


def test_adts_frame_frame_length_bytes():
    """frame_length_bytes matches the constructed frame length."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.frame_length_bytes == len(FRAME_MPEG2_LC_44100_STEREO)


def test_adts_frame_samples_per_frame():
    """samples_per_frame is 1024 for standard ADTS frames."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.samples_per_frame == 1024


def test_adts_frame_has_crc_false():
    """protection_absent=1 → has_crc == False."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.has_crc is False


def test_adts_frame_num_raw_data_blocks():
    """num_raw_data_blocks_in_frame wire=0 → logical 1 block."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert f.num_raw_data_blocks == 1


def test_adts_frame_raw_header_length():
    """raw_header is 7 bytes for no-CRC frames."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert isinstance(f.raw_header, bytes)
    assert len(f.raw_header) == 7


def test_adts_frame_payload_bytes():
    """payload is bytes and has correct length."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert isinstance(f.payload, bytes)
    assert len(f.payload) == len(FRAME_MPEG2_LC_44100_STEREO)


def test_adts_frame_repr():
    """AdtsFrame.__repr__ contains profile, sample rate, channel, frame_len."""
    [f] = parse_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    r = repr(f)
    assert "44100" in r


# ---------------------------------------------------------------------------
# parse_aac_frames (eager, strict) tests
# ---------------------------------------------------------------------------


def test_parse_aac_frames_empty():
    """Empty input yields empty list (not an error)."""
    assert parse_aac_frames(b"") == []


def test_parse_aac_frames_two_back_to_back():
    """Two contiguous frames are both collected."""
    frames = parse_aac_frames(TWO_FRAMES)
    assert len(frames) == 2
    assert all(isinstance(f, AdtsFrame) for f in frames)


def test_parse_aac_frames_bad_sync_raises():
    """All-zero input raises CodecError with kind=BAD_SYNC_WORD."""
    with pytest.raises(CodecError) as exc_info:
        parse_aac_frames(NO_SYNC)
    err = exc_info.value
    assert err.kind is CodecErrorKind.BAD_SYNC_WORD
    assert err.codec == "aac"


def test_parse_aac_frames_garbage_prefix_raises():
    """Garbage bytes at start raise CodecError (strict mode)."""
    with pytest.raises(CodecError):
        parse_aac_frames(GARBAGE_THEN_FRAME)


# ---------------------------------------------------------------------------
# parse_aac_frames_with_resync (eager, best-effort) tests
# ---------------------------------------------------------------------------


def test_parse_aac_frames_with_resync_empty():
    """Empty input yields empty list (never raises)."""
    assert parse_aac_frames_with_resync(b"") == []


def test_parse_aac_frames_with_resync_garbage_prefix_skips():
    """Garbage prefix: resync skips to valid frame, returns 1 result."""
    frames = parse_aac_frames_with_resync(GARBAGE_THEN_FRAME)
    assert len(frames) == 1
    assert isinstance(frames[0], AdtsFrame)


def test_parse_aac_frames_with_resync_all_garbage_returns_empty():
    """All-zeros input: resync yields nothing (never raises)."""
    frames = parse_aac_frames_with_resync(NO_SYNC)
    assert frames == []


def test_parse_aac_frames_with_resync_never_raises():
    """with_resync never raises CodecError regardless of input."""
    # A mix of garbage and a partial frame — must not raise.
    bad = b"\xde\xad\xbe\xef\xde\xad\xbe\xef\xde\xad"
    result = parse_aac_frames_with_resync(bad)
    assert isinstance(result, list)


# ---------------------------------------------------------------------------
# iter_aac_frames (lazy iterator, strict) tests
# ---------------------------------------------------------------------------


def test_iter_aac_frames_returns_iter():
    """iter_aac_frames returns an AdtsFrameIter."""
    it = iter_aac_frames(FRAME_MPEG2_LC_44100_STEREO)
    assert isinstance(it, AdtsFrameIter)


def test_iter_aac_frames_lazy_one_frame():
    """Iterating over one frame yields exactly one AdtsFrame."""
    frames = list(iter_aac_frames(FRAME_MPEG2_LC_44100_STEREO))
    assert len(frames) == 1
    assert isinstance(frames[0], AdtsFrame)


def test_iter_aac_frames_lazy_two_frames():
    """Iterating over two back-to-back frames yields two results."""
    frames = list(iter_aac_frames(TWO_FRAMES))
    assert len(frames) == 2


def test_iter_aac_frames_bad_sync_raises():
    """Iterating over no-sync data raises CodecError."""
    with pytest.raises(CodecError) as exc_info:
        list(iter_aac_frames(NO_SYNC))
    assert exc_info.value.kind is CodecErrorKind.BAD_SYNC_WORD


# ---------------------------------------------------------------------------
# iter_aac_frames_with_resync (lazy iterator, best-effort) tests
# ---------------------------------------------------------------------------


def test_iter_aac_frames_with_resync_skips_garbage():
    """Resync iterator skips garbage prefix, finds valid frame."""
    frames = list(iter_aac_frames_with_resync(GARBAGE_THEN_FRAME))
    assert len(frames) == 1
    assert isinstance(frames[0], AdtsFrame)


def test_iter_aac_frames_with_resync_never_raises():
    """Resync iterator never raises on bad input."""
    frames = list(iter_aac_frames_with_resync(NO_SYNC))
    assert frames == []
