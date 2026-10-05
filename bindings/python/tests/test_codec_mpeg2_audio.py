"""MPEG-2 audio codec surface tests.

Fixtures are synthetic MPEG audio frames built from the MPEG-1/2 header
bit-field spec (ISO/IEC 11172-3 §2.4.2), mirroring the header constants
used in ``crates/tst-core/src/codec/mpegaudio/decode.rs`` unit tests.

MPEG audio header layout (4 bytes, MSB first):
  Bits 31-21: syncword        = 0x7FF (all 11 bits set)
  Bits 20-19: version_id      0b11=MPEG-1, 0b10=MPEG-2, 0b00=MPEG-2.5
  Bits 18-17: layer_desc      0b11=Layer I, 0b10=Layer II, 0b01=Layer III
  Bit  16:    protection_bit  0=CRC present, 1=no CRC
  Bits 15-12: bitrate_index
  Bits 11-10: sample_rate_index
  Bit  9:     padding_bit
  Bit  8:     private_bit
  Bits 7-6:   channel_mode    0b00=Stereo, 0b01=JointStereo, 0b10=Dual, 0b11=Mono

From the Rust decode.rs tests:
  V1L3 128kbps 44100Hz JointStereo no-CRC: [0xFF, 0xFB, 0x90, 0x40]
    => frame_length=417, samples_per_frame=1152

Synthetic V1L2 192kbps 44100Hz Stereo no-CRC:
  version_id=0b11, layer_desc=0b10(L2), protection=1(no CRC)
  bitrate_index for V1L2 192kbps = index 12 (per ISO 11172-3 Table 3)
  sample_rate_index 0=44100, channel_mode=0b00 (Stereo)
  Byte 0: 0xFF
  Byte 1: sync bits 10-8=111, version_id=11, layer_desc=10, protection=1 => 0b11111101 = 0xFD
  Byte 2: bitrate_idx=12=0b1100, sr_idx=0=0b00, padding=0, private=0 => 0b11000000 = 0xC0
  Byte 3: channel_mode=0b00, mode_ext=0b00, copyright=0, original=0, emphasis=0b00 => 0x00
  frame_length = 144*192000/44100 + 0 = 626 bytes
"""

import pytest

from tstrans.codec import (
    ChannelMode,
    Layer,
    Mpeg2AudioFrame,
    Mpeg2AudioFrameIter,
    Version,
    iter_mpeg2_audio_frames,
    iter_mpeg2_audio_frames_with_resync,
    parse_mpeg2_audio_frames,
    parse_mpeg2_audio_frames_with_resync,
)
from tstrans.exceptions import CodecError, CodecErrorKind

# ---------------------------------------------------------------------------
# Frame byte fixtures
# ---------------------------------------------------------------------------

# V1L3 128kbps 44100Hz JointStereo no-CRC — from decode.rs test constant.
# frame_length=417, samples_per_frame=1152
_V1L3_HEADER = bytes([0xFF, 0xFB, 0x90, 0x40])
FRAME_V1L3_128K_44100_JS = _V1L3_HEADER + bytes(417 - 4)

# V1L2 192kbps 44100Hz Stereo no-CRC.
# Byte 1: 0xFD (sync[3:0]=1111 + version=11 + layer=10 + protection=1)
# Byte 2: bitrate_index=10 (192kbps in V1L2 table) + sr_idx=0 => 0b10100000=0xA0
# frame_length = 144*192000/44100 = 626 bytes
_V1L2_HEADER = bytes([0xFF, 0xFD, 0xA0, 0x00])
FRAME_V1L2_192K_44100_STEREO = _V1L2_HEADER + bytes(626 - 4)

# Two back-to-back V1L3 frames.
TWO_FRAMES = FRAME_V1L3_128K_44100_JS * 2

# Garbage prefix (no valid sync) followed by one valid frame.
GARBAGE_PREFIX = b"\xde\xad\xbe\xef"
GARBAGE_THEN_FRAME = GARBAGE_PREFIX + FRAME_V1L3_128K_44100_JS

# All-zeros — no valid syncword anywhere.
NO_SYNC = bytes(32)


# ---------------------------------------------------------------------------
# Layer enum tests
# ---------------------------------------------------------------------------


def test_layer_enum_variants():
    """Layer has I, II, III variants."""
    assert Layer.I is not None
    assert Layer.II is not None
    assert Layer.III is not None


# ---------------------------------------------------------------------------
# Version enum tests
# ---------------------------------------------------------------------------


def test_version_enum_variants():
    """Version has MPEG1, MPEG2, MPEG2_5 variants."""
    assert Version.MPEG1 is not None
    assert Version.MPEG2 is not None
    assert Version.MPEG2_5 is not None


# ---------------------------------------------------------------------------
# ChannelMode enum tests
# ---------------------------------------------------------------------------


def test_channel_mode_enum_variants():
    """ChannelMode has STEREO, JOINT_STEREO, DUAL_CHANNEL, MONO variants."""
    assert ChannelMode.STEREO is not None
    assert ChannelMode.JOINT_STEREO is not None
    assert ChannelMode.DUAL_CHANNEL is not None
    assert ChannelMode.MONO is not None


# ---------------------------------------------------------------------------
# Mpeg2AudioFrame field tests
# ---------------------------------------------------------------------------


def test_mpeg2_audio_frame_returns_typed():
    """parse_mpeg2_audio_frames returns Mpeg2AudioFrame instances."""
    frames = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert len(frames) == 1
    assert isinstance(frames[0], Mpeg2AudioFrame)


def test_mpeg2_audio_frame_layer():
    """V1L3 fixture: layer == Layer.III."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.layer == Layer.III


def test_mpeg2_audio_frame_version():
    """V1L3 fixture: version == Version.MPEG1."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.version == Version.MPEG1


def test_mpeg2_audio_frame_bitrate_kbps():
    """V1L3 128kbps fixture: bitrate_kbps == 128."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.bitrate_kbps == 128


def test_mpeg2_audio_frame_sample_rate_hz():
    """V1L3 44100 Hz fixture: sample_rate_hz == 44100."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.sample_rate_hz == 44100


def test_mpeg2_audio_frame_channel_mode_joint_stereo():
    """V1L3 joint-stereo fixture: channel_mode == ChannelMode.JOINT_STEREO."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.channel_mode == ChannelMode.JOINT_STEREO


def test_mpeg2_audio_frame_channels():
    """V1L3 joint-stereo: channels == 2."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.channels == 2


def test_mpeg2_audio_frame_channel_mode_stereo():
    """V1L2 stereo fixture: channel_mode == ChannelMode.STEREO."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L2_192K_44100_STEREO)
    assert f.channel_mode == ChannelMode.STEREO


def test_mpeg2_audio_frame_frame_length_bytes():
    """V1L3 128k 44100 frame_length_bytes == 417."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.frame_length_bytes == 417


def test_mpeg2_audio_frame_samples_per_frame():
    """V1L3 MPEG-1 samples_per_frame == 1152."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.samples_per_frame == 1152


def test_mpeg2_audio_frame_has_crc_false():
    """protection_bit=1 (no CRC) → has_crc == False."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.has_crc is False


def test_mpeg2_audio_frame_raw_header_length():
    """raw_header is always 4 bytes."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert isinstance(f.raw_header, bytes)
    assert len(f.raw_header) == 4


def test_mpeg2_audio_frame_raw_header_bytes():
    """raw_header matches the leading 4 bytes of the fixture."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert f.raw_header == bytes(_V1L3_HEADER)


def test_mpeg2_audio_frame_payload_bytes():
    """payload is bytes with length == frame_length_bytes."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert isinstance(f.payload, bytes)
    assert len(f.payload) == 417


def test_mpeg2_audio_frame_repr():
    """Mpeg2AudioFrame.__repr__ contains the sample rate."""
    [f] = parse_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert "44100" in repr(f)


# ---------------------------------------------------------------------------
# parse_mpeg2_audio_frames (eager, strict) tests
# ---------------------------------------------------------------------------


def test_parse_mpeg2_audio_frames_empty():
    """Empty input yields empty list (not an error)."""
    assert parse_mpeg2_audio_frames(b"") == []


def test_parse_mpeg2_audio_frames_two_back_to_back():
    """Two contiguous frames are both collected."""
    frames = parse_mpeg2_audio_frames(TWO_FRAMES)
    assert len(frames) == 2
    assert all(isinstance(f, Mpeg2AudioFrame) for f in frames)


def test_parse_mpeg2_audio_frames_bad_sync_raises():
    """All-zero input raises CodecError with kind=BAD_SYNC_WORD."""
    with pytest.raises(CodecError) as exc_info:
        parse_mpeg2_audio_frames(NO_SYNC)
    err = exc_info.value
    assert err.kind is CodecErrorKind.BAD_SYNC_WORD
    assert err.codec == "mpeg2audio"


def test_parse_mpeg2_audio_frames_garbage_prefix_raises():
    """Garbage bytes at start raise CodecError (strict mode)."""
    with pytest.raises(CodecError):
        parse_mpeg2_audio_frames(GARBAGE_THEN_FRAME)


# ---------------------------------------------------------------------------
# parse_mpeg2_audio_frames_with_resync (eager, best-effort) tests
# ---------------------------------------------------------------------------


def test_parse_mpeg2_audio_frames_with_resync_empty():
    """Empty input yields empty list (never raises)."""
    assert parse_mpeg2_audio_frames_with_resync(b"") == []


def test_parse_mpeg2_audio_frames_with_resync_garbage_prefix_skips():
    """Garbage prefix: resync skips to valid frame, returns 1 result."""
    frames = parse_mpeg2_audio_frames_with_resync(GARBAGE_THEN_FRAME)
    assert len(frames) == 1
    assert isinstance(frames[0], Mpeg2AudioFrame)


def test_parse_mpeg2_audio_frames_with_resync_all_garbage_returns_empty():
    """All-zeros input: resync yields nothing (never raises)."""
    frames = parse_mpeg2_audio_frames_with_resync(NO_SYNC)
    assert frames == []


def test_parse_mpeg2_audio_frames_with_resync_never_raises():
    """with_resync never raises CodecError regardless of input."""
    bad = b"\xde\xad\xbe\xef\xde\xad\xbe\xef\xde\xad"
    result = parse_mpeg2_audio_frames_with_resync(bad)
    assert isinstance(result, list)


# ---------------------------------------------------------------------------
# iter_mpeg2_audio_frames (lazy iterator, strict) tests
# ---------------------------------------------------------------------------


def test_iter_mpeg2_audio_frames_returns_iter():
    """iter_mpeg2_audio_frames returns an Mpeg2AudioFrameIter."""
    it = iter_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS)
    assert isinstance(it, Mpeg2AudioFrameIter)


def test_iter_mpeg2_audio_frames_one_frame():
    """Iterating over one frame yields exactly one Mpeg2AudioFrame."""
    frames = list(iter_mpeg2_audio_frames(FRAME_V1L3_128K_44100_JS))
    assert len(frames) == 1
    assert isinstance(frames[0], Mpeg2AudioFrame)


def test_iter_mpeg2_audio_frames_two_frames():
    """Iterating over two back-to-back frames yields two results."""
    frames = list(iter_mpeg2_audio_frames(TWO_FRAMES))
    assert len(frames) == 2


def test_iter_mpeg2_audio_frames_bad_sync_raises():
    """Iterating over no-sync data raises CodecError."""
    with pytest.raises(CodecError) as exc_info:
        list(iter_mpeg2_audio_frames(NO_SYNC))
    assert exc_info.value.kind is CodecErrorKind.BAD_SYNC_WORD


# ---------------------------------------------------------------------------
# iter_mpeg2_audio_frames_with_resync (lazy iterator, best-effort) tests
# ---------------------------------------------------------------------------


def test_iter_mpeg2_audio_frames_with_resync_skips_garbage():
    """Resync iterator skips garbage prefix, finds valid frame."""
    frames = list(iter_mpeg2_audio_frames_with_resync(GARBAGE_THEN_FRAME))
    assert len(frames) == 1
    assert isinstance(frames[0], Mpeg2AudioFrame)


def test_iter_mpeg2_audio_frames_with_resync_never_raises():
    """Resync iterator never raises on bad input."""
    frames = list(iter_mpeg2_audio_frames_with_resync(NO_SYNC))
    assert frames == []
