"""Generator script for synthetic AAC TS fixture files.

Run from the workspace root (ts-transformer/) to regenerate the binary
fixtures checked into bindings/python/tests/fixtures/:

    python bindings/python/tests/_builders/build_audio_aac_large.py

Generates two files:

  - aac_minimal.ts  (~3–5 KB) — 10 ADTS frames for the GIL smoke test
    (test_demux_audio_gil.py::test_next_event_audio_typed_payload_unchanged).
    10 separate PES packets, one frame each.

  - audio_aac_large.ts (~650 KB) — 10 large PES packets, each carrying 4095
    ADTS frames (~65 KB per PES). For the GIL progress test
    (test_demux_audio_gil.py::test_next_event_releases_gil_during_aac_parse).

    Why large PES packets instead of many small ones: the GIL test asserts that
    a background Python thread can increment a counter >50 times while
    Demuxer.next_event() is parsing audio frames. Parsing 4095 frames per PES
    event takes ~0.4 ms, long enough for the background thread to get CPU time
    when the GIL is released. With one frame per PES (~0.001 ms
    per event), the demux loop completes too quickly for the test to be
    reliable.

The ADTS frame used is a 16-byte MPEG-2 AAC-LC 44100 Hz stereo frame (header
only, no PCM payload). This is the smallest valid parseable ADTS frame. The
frame literal is taken from test_codec_aac.py:
  FRAME_MPEG2_LC_44100_STEREO = fff95080021ffc000000000000000000

No audio will play from this file; it is a structural fixture only.
"""

from __future__ import annotations

from pathlib import Path

# Valid 16-byte MPEG-2 AAC-LC 44100 Hz stereo ADTS frame (header only).
# Verified parseable by tstrans.codec.parse_aac_frames().
# From test_codec_aac.py: FRAME_MPEG2_LC_44100_STEREO.
_ADTS_FRAME = bytes.fromhex("fff95080021ffc000000000000000000")

# 1 AAC frame at 44100 Hz = 1024 samples = 1024/44100 sec.
# In 90 kHz TS clock units: 1024 * 90000 / 44100 ≈ 2090 ticks.
_PTS_STEP_PER_FRAME = 2090  # ticks per frame (90 kHz)

# The Muxer caps push_audio() payload at 65527 bytes per PES.
# Each ADTS frame is 16 bytes, so we can fit at most 65527 // 16 = 4095 frames.
# Using max capacity makes each next_event() call parse ~65KB of ADTS — enough
# for the GIL release to be measurable by a background thread.
_MAX_FRAMES_PER_PES = 65527 // len(_ADTS_FRAME)  # = 4095

_FIXTURES_DIR = Path(__file__).parent.parent / "fixtures"


def _build_aac_ts_small(n_frames: int) -> bytes:
    """Build a TS with one AAC audio stream, n_frames separate 1-frame PES packets.

    Each push_audio() call produces one PES. Used for the smoke test fixture.
    Returns the TS bytes (always a multiple of 188 bytes).
    """
    import tempfile

    from tstrans.mpegts import (
        AudioCodec,
        Muxer,
        MuxerConfigBuilder,
        MuxerProgramConfigBuilder,
        Pts90khz,
    )

    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_audio(0x102, AudioCodec.AAC)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    m = Muxer(cfg)

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "aac.ts"
        with m.write_file(path) as proxy:
            for i in range(n_frames):
                pts_ticks = 90_000 + i * _PTS_STEP_PER_FRAME
                proxy.push_audio(_ADTS_FRAME, pts=Pts90khz.from_raw(pts_ticks))
        return path.read_bytes()


def _build_aac_ts_large_pes(n_pushes: int) -> bytes:
    """Build a TS where each PES carries the maximum number of ADTS frames.

    Each push_audio() call pushes _MAX_FRAMES_PER_PES frames concatenated,
    producing a large PES payload that takes ~0.4ms to parse. With n_pushes=10,
    the resulting file is ~650 KB and the demux loop takes ~4ms total — enough
    for a background thread to increment >50 times when the GIL is released.

    Returns the TS bytes (always a multiple of 188 bytes).
    """
    import tempfile

    from tstrans.mpegts import (
        AudioCodec,
        Muxer,
        MuxerConfigBuilder,
        MuxerProgramConfigBuilder,
        Pts90khz,
    )

    big_payload = _ADTS_FRAME * _MAX_FRAMES_PER_PES

    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_audio(0x102, AudioCodec.AAC)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    m = Muxer(cfg)

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "aac_large.ts"
        with m.write_file(path) as proxy:
            for i in range(n_pushes):
                pts_ticks = 90_000 + i * _MAX_FRAMES_PER_PES * _PTS_STEP_PER_FRAME
                proxy.push_audio(big_payload, pts=Pts90khz.from_raw(pts_ticks))
        return path.read_bytes()


def build_aac_minimal() -> bytes:
    """10-frame AAC TS for the GIL smoke test."""
    return _build_aac_ts_small(10)


def build_aac_large() -> bytes:
    """20 large-PES AAC TS for the GIL progress test (~1.3 MB).

    Each PES holds 4095 ADTS frames (~65 KB). The demuxer emits 20 Audio
    events; the demux loop takes ~8–10ms total — enough for the background
    thread to exceed 50 increments when the GIL is released.

    20 pushes (vs. 10) is chosen for headroom: on a lightly loaded machine
    parsing 10 events at ~0.4ms each touches 4ms, but scheduling variance
    can put all time-slices in the demux thread. 20 pushes raises the floor.
    """
    return _build_aac_ts_large_pes(20)


def main() -> None:
    _FIXTURES_DIR.mkdir(parents=True, exist_ok=True)

    minimal = build_aac_minimal()
    minimal_path = _FIXTURES_DIR / "aac_minimal.ts"
    minimal_path.write_bytes(minimal)
    print(
        f"wrote {minimal_path}  "
        f"({len(minimal):,} bytes, {len(minimal) // 188} packets)"
    )

    large = build_aac_large()
    large_path = _FIXTURES_DIR / "audio_aac_large.ts"
    large_path.write_bytes(large)
    print(
        f"wrote {large_path}  "
        f"({len(large):,} bytes, {len(large) // 188} packets)"
    )


if __name__ == "__main__":
    main()
