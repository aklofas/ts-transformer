"""Raw-first: end-to-end DemuxEvent.Video/.Audio raw + opt-in parse access.

Video and Audio events carry raw encoded bytes on `.raw`; typed units come
from the opt-in `.parse()` (→ `list[NalUnit]`/`list[Obu]` for video,
`list[AdtsFrame]`/`list[Mpeg2AudioFrame]`/`[]` for audio).

These tests use:
- Muxer-generated synthetic TS for video (H.264 NAL AUD round-trip).
- Real audio fixtures from crates/tst-core/tests/fixtures/audio/ for AAC + MP2.
"""

import tempfile
from pathlib import Path

import pytest

from tstrans.codec import AdtsFrame, Mpeg2AudioFrame, NalUnit, Obu
from tstrans.io import parse_file
from tstrans.mpegts import (
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

# ---------------------------------------------------------------------------
# Fixtures and helpers
# ---------------------------------------------------------------------------

_FIXTURE_BASE = (
    Path(__file__).parent.parent.parent.parent / "crates" / "tst-core" / "tests" / "fixtures"
)
_AAC_ADTS_FIXTURE = _FIXTURE_BASE / "audio" / "aac-adts.ts"
_MP2_FIXTURE = _FIXTURE_BASE / "audio" / "mp2.ts"

# All four audio fixtures are checked into
# crates/tst-core/tests/fixtures/audio/. A missing file is a packaging bug,
# not a runtime skip condition.
for _fx in (_AAC_ADTS_FIXTURE, _MP2_FIXTURE):
    assert _fx.is_file(), f"checked-in audio fixture missing: {_fx}"


def _make_h264_ts(tmp: Path) -> Path:
    """Write a small H.264 TS into *tmp* via the Muxer and return the path."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    m = Muxer(cfg)
    path = tmp / "h264.ts"
    # H.264 Access Unit Delimiter NAL (AUD): Annex-B, nal_unit_type=9.
    nal_aud = b"\x00\x00\x00\x01\x09\xF0"
    pts0 = 900_000  # ~10 s at 90 kHz
    with m.write_file(path) as proxy:
        for i in range(4):
            proxy.push_video(nal_aud, pts=Pts90khz.from_raw(pts0 + i * 3000))
    return path


# ---------------------------------------------------------------------------
# Video: raw Annex-B + opt-in parse → list[NalUnit]
# ---------------------------------------------------------------------------

def test_h264_video_raw_is_bytes():
    """parse_file on a synthetic H.264 TS must yield Video events whose
    `.raw` is the encoded access unit (bytes, Annex-B start code)."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_h264_ts(Path(tmp))
        events = list(parse_file(ts_path))
    video = [e for e in events if isinstance(e, DemuxEvent.Video)]
    assert video, "expected at least one DemuxEvent.Video from synthetic H.264 TS"
    s = video[0]
    assert isinstance(s.raw, (bytes, bytearray)), (
        f"expected bytes, got {type(s.raw).__name__}"
    )
    assert s.raw[:4] == b"\x00\x00\x00\x01"


def test_h264_video_parse_returns_nal_units():
    """`.parse()` must split `.raw` into a list of NalUnit instances."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_h264_ts(Path(tmp))
        events = list(parse_file(ts_path))
    video = [e for e in events if isinstance(e, DemuxEvent.Video)]
    assert video
    units = video[0].parse()
    assert isinstance(units, list)
    assert all(isinstance(n, NalUnit) for n in units), (
        "expected all elements to be NalUnit"
    )


def test_h264_nal_unit_kind_is_h264():
    """NalUnit.kind for H.264 stream must equal 'H264'."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_h264_ts(Path(tmp))
        events = list(parse_file(ts_path))
    video = [e for e in events if isinstance(e, DemuxEvent.Video)]
    assert video
    units = video[0].parse()
    assert len(units) > 0
    assert units[0].kind == "H264", f"got kind={units[0].kind!r}"


def test_video_event_has_no_payload_attribute():
    """The raw-first surface drops the eager `.payload` field."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_h264_ts(Path(tmp))
        events = list(parse_file(ts_path))
    video = [e for e in events if isinstance(e, DemuxEvent.Video)]
    assert video
    assert not hasattr(video[0], "payload")
    assert not hasattr(video[0], "codec_parse_error")


# ---------------------------------------------------------------------------
# Audio: AAC-ADTS → raw + parse() → list[AdtsFrame]
# ---------------------------------------------------------------------------

def test_aac_audio_raw_is_bytes():
    """AAC ADTS events from a real fixture must carry raw bytes."""
    aac_events = [
        e for e in parse_file(_AAC_ADTS_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "aac"
    ]
    assert aac_events, f"no AAC audio events found in {_AAC_ADTS_FIXTURE}"
    assert isinstance(aac_events[0].raw, (bytes, bytearray))


def test_aac_audio_parse_returns_adts_frames():
    """`.parse()` on an AAC Audio event returns list[AdtsFrame]."""
    aac_events = [
        e for e in parse_file(_AAC_ADTS_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "aac"
    ]
    assert aac_events
    frames = aac_events[0].parse()
    assert isinstance(frames, list)
    assert all(isinstance(f, AdtsFrame) for f in frames), (
        "expected all elements to be AdtsFrame"
    )


# ---------------------------------------------------------------------------
# Audio: MP2 → raw + parse() → list[Mpeg2AudioFrame]
# ---------------------------------------------------------------------------

def test_mp2_audio_raw_is_bytes():
    """MP2 audio events from a real fixture must carry raw bytes."""
    mp2_events = [
        e for e in parse_file(_MP2_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "mp2"
    ]
    assert mp2_events, f"no MP2 audio events found in {_MP2_FIXTURE}"
    assert isinstance(mp2_events[0].raw, (bytes, bytearray))


def test_mp2_audio_parse_returns_mpeg2_audio_frames():
    """`.parse()` on an MP2 Audio event returns list[Mpeg2AudioFrame]."""
    mp2_events = [
        e for e in parse_file(_MP2_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "mp2"
    ]
    assert mp2_events
    frames = mp2_events[0].parse()
    assert all(isinstance(f, Mpeg2AudioFrame) for f in frames), (
        "expected all elements to be Mpeg2AudioFrame"
    )


# ---------------------------------------------------------------------------
# Bytes-only codecs: LATM / AC-3 → raw bytes, parse() → [] (no typed parser)
# ---------------------------------------------------------------------------

_AAC_LATM_FIXTURE = _FIXTURE_BASE / "audio" / "aac-latm.ts"
_AC3_FIXTURE = _FIXTURE_BASE / "audio" / "ac3.ts"

for _fx in (_AAC_LATM_FIXTURE, _AC3_FIXTURE):
    assert _fx.is_file(), f"checked-in audio fixture missing: {_fx}"


def test_aac_latm_parse_is_empty_list():
    """AAC-LATM has no typed parser — `.raw` is bytes, `.parse()` is []."""
    latm_events = [
        e for e in parse_file(_AAC_LATM_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "aac_latm"
    ]
    assert latm_events, f"no AAC-LATM events found in {_AAC_LATM_FIXTURE}"
    s = latm_events[0]
    assert isinstance(s.raw, (bytes, bytearray))
    assert s.parse() == []


def test_ac3_parse_is_empty_list():
    """AC-3 typed parsing is not implemented — `.raw` is bytes, `.parse()` is []."""
    ac3_events = [
        e for e in parse_file(_AC3_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "ac3"
    ]
    assert ac3_events, f"no AC-3 events found in {_AC3_FIXTURE}"
    s = ac3_events[0]
    assert isinstance(s.raw, (bytes, bytearray))
    assert s.parse() == []


# ---------------------------------------------------------------------------
# Regression: Audio events expose .raw on every codec type
# ---------------------------------------------------------------------------

def test_audio_event_has_no_payload_attribute():
    """DemuxEvent.Audio drops the eager `.payload`/`codec_parse_error` fields."""
    aac_events = [
        e for e in parse_file(_AAC_ADTS_FIXTURE)
        if isinstance(e, DemuxEvent.Audio)
        and e.codec.value == "aac"
    ]
    assert aac_events
    assert not hasattr(aac_events[0], "payload")
    assert not hasattr(aac_events[0], "codec_parse_error")
