"""Raw-first: audio events surface raw bytes; parsing is opt-in via `.parse()`.

Before the raw-first rewire, the Rust→Python conversion eagerly parsed audio
frames and fell back to a `payload: bytes` + `codec_parse_error: CodecError`
shape on mid-stream parse errors. With raw-first:
  - the event always carries `.raw` (the raw PES payload bytes)
  - `.parse()` (lenient default) resyncs past bad frames and never raises
  - `.parse(strict=True)` raises `CodecError` on a malformed frame
  - the conversion path itself never parses, so it can never raise

These tests use the Muxer to produce a small TS, then patch the raw bytes on
disk to corrupt the AAC payload past the first frame.
"""

import tempfile
from pathlib import Path

import pytest

from tstrans.codec import AdtsFrame
from tstrans.exceptions import CodecError, CodecErrorKind
from tstrans.io import parse_file
from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_FIXTURE_BASE = (
    Path(__file__).parent.parent.parent.parent / "crates" / "tst-core" / "tests" / "fixtures"
)
_AAC_ADTS_FIXTURE = _FIXTURE_BASE / "audio" / "aac-adts.ts"


def _make_aac_ts(tmp: Path) -> Path:
    """Write a TS with one video + one AAC stream into *tmp*."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_audio(0x102, AudioCodec.AAC)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    m = Muxer(cfg)
    path = tmp / "aac.ts"

    # 7-byte minimal ADTS frame (MPEG-4 AAC-LC 44100 Hz stereo, header only).
    adts_frame = bytes([0xFF, 0xF1, 0x60, 0x80, 0x00, 0x1F, 0xFC])
    nal_aud = b"\x00\x00\x00\x01\x09\xF0"
    pts0 = 900_000

    with m.write_file(path) as proxy:
        proxy.push_video(nal_aud, pts=Pts90khz.from_raw(pts0))
        proxy.push_audio(adts_frame, pts=Pts90khz.from_raw(pts0))
    return path


# ---------------------------------------------------------------------------
# Test: clean AAC produces typed list via .parse() (sanity baseline)
# ---------------------------------------------------------------------------

def test_clean_aac_parse_yields_typed_list():
    """Sanity: clean ADTS stream → `.parse()` returns list[AdtsFrame]."""
    aac_evs = [
        e for e in parse_file(_AAC_ADTS_FIXTURE)
        if isinstance(e, DemuxEvent.Audio) and e.codec.value == "aac"
    ]
    assert aac_evs, "expected at least one AAC event from fixture"
    s = aac_evs[0]
    assert isinstance(s.raw, (bytes, bytearray))
    frames = s.parse()
    assert all(isinstance(f, AdtsFrame) for f in frames)


# ---------------------------------------------------------------------------
# Test: raw bytes are always present on the event
# ---------------------------------------------------------------------------

def test_raw_attribute_present_on_clean_aac():
    """Every Audio event carries `.raw` bytes regardless of parse result."""
    aac_evs = [
        e for e in parse_file(_AAC_ADTS_FIXTURE)
        if isinstance(e, DemuxEvent.Audio) and e.codec.value == "aac"
    ]
    assert aac_evs
    assert hasattr(aac_evs[0], "raw")
    assert isinstance(aac_evs[0].raw, (bytes, bytearray))


# ---------------------------------------------------------------------------
# Test: corrupted payload — lenient .parse() resyncs, strict raises
# ---------------------------------------------------------------------------

def _corrupt_aac_payload(ts_bytes: bytes) -> bytes:
    """Overwrite the first ADTS syncword (after offset 200) in a TS file's
    AAC PES payload with 0x00 0x00 so a parser sees a bad sync word."""
    data = bytearray(ts_bytes)
    for i in range(200, len(data) - 1):
        if data[i] == 0xFF and (data[i + 1] & 0xF6) == 0xF0:
            data[i] = 0x00
            data[i + 1] = 0x00
            return bytes(data)
    return ts_bytes


def test_corrupted_aac_strict_parse_raises():
    """When the ADTS payload has a bad sync word, strict `.parse(strict=True)`
    must raise CodecError; lenient `.parse()` must resync without raising."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_aac_ts(Path(tmp))
        clean_bytes = ts_path.read_bytes()
        corrupted = _corrupt_aac_payload(clean_bytes)

        if corrupted == clean_bytes:
            pytest.fail("could not locate ADTS syncword in synthetic TS")

        corrupt_path = Path(tmp) / "corrupted.ts"
        corrupt_path.write_bytes(corrupted)

        all_events = list(parse_file(corrupt_path))

    aac_evs = [
        e for e in all_events
        if isinstance(e, DemuxEvent.Audio) and e.codec.value == "aac"
    ]
    if not aac_evs:
        pytest.fail("no AAC events in corrupted TS — syncword may not have landed in AAC PID")

    # Find an event whose raw bytes start with a corrupted (non-sync) header.
    corrupt_evs = [
        e for e in aac_evs
        if len(e.raw) >= 2 and not (e.raw[0] == 0xFF and (e.raw[1] & 0xF6) == 0xF0)
    ]
    if not corrupt_evs:
        pytest.fail(
            f"corruption did not land at a frame start — "
            f"raw heads: {[bytes(e.raw[:2]) for e in aac_evs]}"
        )

    e = corrupt_evs[0]
    # Lenient parse must not raise (it resyncs past the junk).
    lenient = e.parse()
    assert isinstance(lenient, list)
    # Strict parse must raise CodecError on the malformed leading frame.
    with pytest.raises(CodecError):
        e.parse(strict=True)


def test_audio_event_conversion_never_raises():
    """The Rust→Python conversion path must not parse (and so never raise),
    even for an all-zeros AAC payload. `.raw` is always available."""
    with tempfile.TemporaryDirectory() as tmp:
        ts_path = _make_aac_ts(Path(tmp))
        raw = bytearray(ts_path.read_bytes())
        # Zero out everything after the first 564 bytes (PAT+PMT+first video PKT).
        raw[564:] = b"\x00" * (len(raw) - 564)
        zeroed_path = Path(tmp) / "zeroed.ts"
        zeroed_path.write_bytes(bytes(raw))

        try:
            events = list(parse_file(zeroed_path))
        except Exception as exc:  # noqa: BLE001
            pytest.fail(f"parse_file raised unexpectedly: {exc!r}")

    aac_evs = [
        e for e in events
        if isinstance(e, DemuxEvent.Audio) and e.codec.value == "aac"
    ]
    for ev in aac_evs:
        assert isinstance(ev.raw, (bytes, bytearray)), (
            f"unexpected raw type: {type(ev.raw).__name__}"
        )
