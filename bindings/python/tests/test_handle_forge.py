"""Forged stream handles must be rejected.

A caller-provided raw `u32` with bits set outside the canonical 4-bit
program + 4-bit within layout aliases a valid low-byte handle once the
pre-fix `from_raw` + `unpack` path masks the high bits. The push-time
range check only sees the masked indices and silently routes the
payload to the wrong elementary stream.

The fix wires `try_from_raw` into every Python `<Kind>StreamHandle.from_raw`
staticmethod so the Python entry point raises `MuxError(INVALID_USAGE)`
before the handle can ever reach a push call.
"""

import pytest

from tstrans.exceptions import MuxError, MuxErrorKind
from tstrans.mpegts import (
    AudioCodec,
    AudioStreamHandle,
    KlvStreamHandle,
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    SubtitleStreamHandle,
    VideoCodec,
    VideoStreamHandle,
)


def _simple_config() -> MuxerConfig:
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_audio(0x102, AudioCodec.AAC)
        .add_klv(0x103, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


# ---------------------------------------------------------------------------
# from_raw rejects forged handles directly (the primary Python attack surface)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "handle_cls",
    [VideoStreamHandle, KlvStreamHandle, AudioStreamHandle, SubtitleStreamHandle],
)
def test_from_raw_rejects_forged_high_bit(handle_cls):
    """`raw | 0x100` MUST raise MuxError(INVALID_USAGE), not silently alias.

    Bit 8 is the first reserved bit above the canonical 8-bit packed layout.
    Without `try_from_raw` validation, the pre-fix code returned a wrapper
    over the forged value; subsequent pushes would mask-and-alias.
    """
    valid = handle_cls.from_raw(0x00)  # program=0, within=0 — canonical
    assert valid.raw == 0x00

    forged = valid.raw | 0x100
    with pytest.raises(MuxError) as exc_info:
        handle_cls.from_raw(forged)
    assert exc_info.value.kind == MuxErrorKind.INVALID_USAGE


@pytest.mark.parametrize(
    "handle_cls",
    [VideoStreamHandle, KlvStreamHandle, AudioStreamHandle, SubtitleStreamHandle],
)
def test_from_raw_rejects_far_upper_bits(handle_cls):
    """Upper-word bits (e.g. 0x10000, u32::MAX) also reject."""
    with pytest.raises(MuxError) as exc_info_a:
        handle_cls.from_raw(0x0001_0000)
    assert exc_info_a.value.kind == MuxErrorKind.INVALID_USAGE

    with pytest.raises(MuxError) as exc_info_b:
        handle_cls.from_raw(0xFFFF_FFFF)
    assert exc_info_b.value.kind == MuxErrorKind.INVALID_USAGE


@pytest.mark.parametrize(
    "handle_cls",
    [VideoStreamHandle, KlvStreamHandle, AudioStreamHandle, SubtitleStreamHandle],
)
def test_from_raw_accepts_full_canonical_layout(handle_cls):
    """0xFF = (program=0xF, within=0xF) — bit 7 set, bit 8 clear. Canonical.

    Push-time range checks reject this on actual muxer state, but layout
    validation only filters the strictly out-of-layout subset.
    """
    h = handle_cls.from_raw(0xFF)
    assert h.raw == 0xFF


# ---------------------------------------------------------------------------
# End-to-end: a real push through a real muxer with a forged handle fails
# ---------------------------------------------------------------------------


def test_push_video_to_with_forged_handle_rejects():
    """Even if from_raw is bypassed via the actual handle returned by
    `Muxer.video_handles()`, the forged variant must not silently route
    to the valid stream. We construct the forged handle via from_raw
    (which now rejects directly), so the test asserts the from_raw
    rejection is what surfaces.
    """
    m = Muxer(_simple_config())
    handles = m.video_handles()
    assert len(handles) == 1
    valid = handles[0]
    forged_raw = valid.raw | 0x100

    with pytest.raises(MuxError) as exc_info:
        VideoStreamHandle.from_raw(forged_raw)
    assert exc_info.value.kind == MuxErrorKind.INVALID_USAGE


def test_push_video_with_valid_handle_still_works():
    """Sanity: valid handles obtained from the muxer still work.
    Regression check that the canonical form is still accepted by the
    handle validation.
    """
    m = Muxer(_simple_config())
    handles = m.video_handles()
    nal = bytes([0x00, 0x00, 0x00, 0x01, 0x09, 0x10])  # AUD NAL
    m.push_video_to(handles[0], nal, pts=Pts90khz.from_raw(900_000), key_frame=True)

    def _pull_all(mx: Muxer) -> bytes:
        n = mx.pending_packets()
        buf = bytearray(max(n, 1) * 188)
        pulled = mx.pull(buf)
        return bytes(buf[:pulled])

    ref = Muxer(_simple_config())
    ref.push_video(nal, pts=Pts90khz.from_raw(900_000), key_frame=True)
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference video push emitted no TS bytes"
    assert _pull_all(m) == ref_ts
