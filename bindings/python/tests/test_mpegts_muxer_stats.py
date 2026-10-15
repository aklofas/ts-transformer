"""MuxerStats + per-stream codec stats tests.

Covers the three accessors `Muxer.stats()`, `Muxer.reset_stats()`,
`Muxer.stream_codec_stats(pid)` and the Python-side dataclass shapes
for `MuxerStats` (top-level snapshot) and the tagged-union
`StreamCodecStats` hierarchy. Mirrors the Rust contract in
`tst_core::mpegts::mux::stats_accounting::MuxerStats` +
`tst_core::mpegts::stats::StreamCodecStats`.
"""

import pytest

from tstrans.mpegts import (
    AudioStreamCodecStats,
    KlvStreamCodecStats,
    Muxer,
    MuxerStats,
    Pts90khz,
    StreamCodecStats,
    VideoStreamCodecStats,
)

from _builders.mux_programs import video_audio_klv_config as _cfg
from _builders.nal import nal_aud as _nal_aud


def _klv_ls() -> bytes:
    # Minimal ST 0601 UL header (16-byte universal label) — not a valid
    # KLV record, but enough bytes for `push_klv` to wrap and bump the
    # records counter.
    return b"\x06\x0E\x2B\x34\x02\x0B\x01\x01\x0E\x01\x03\x01\x01\x00\x00\x00\x00"


def test_stats_initial_state():
    m = Muxer(_cfg())
    s = m.stats()
    assert isinstance(s, MuxerStats)
    assert s.ts_packets_emitted == 0
    assert s.ts_bytes_emitted == 0
    assert s.programs_configured == 1
    assert s.subtitle_streams_configured == 0


def test_stats_increments_after_push_pull():
    m = Muxer(_cfg())
    m.push_video(_nal_aud(), pts=Pts90khz.from_raw(900_000))
    n = int(m.pending_packets())
    buf = bytearray(n * 188)
    m.pull(buf)
    s = m.stats()
    assert s.ts_packets_emitted > 0
    assert s.ts_bytes_emitted == s.ts_packets_emitted * 188


def test_reset_stats_zeros_counters():
    m = Muxer(_cfg())
    m.push_video(_nal_aud(), pts=Pts90khz.from_raw(900_000))
    buf = bytearray(int(m.pending_packets()) * 188)
    m.pull(buf)
    m.reset_stats()
    s = m.stats()
    assert s.ts_packets_emitted == 0
    assert s.ts_bytes_emitted == 0


def test_stream_codec_stats_video_variant():
    m = Muxer(_cfg())
    m.push_video(_nal_aud(), pts=Pts90khz.from_raw(900_000))
    s = m.stream_codec_stats(0x101)
    assert s is not None
    assert isinstance(s, VideoStreamCodecStats)
    assert isinstance(s, StreamCodecStats)
    # AUD is one NAL; saw no key_frame, so random_access_aus = 0.
    assert s.nals_or_obus >= 1
    assert s.random_access_aus == 0


def test_stream_codec_stats_unknown_pid_returns_none():
    m = Muxer(_cfg())
    # PID 0x999 is not in the muxer config — must return None
    # (distinguishes from `Some(Unknown)` for configured-but-no-data).
    assert m.stream_codec_stats(0x999) is None


def test_stream_codec_stats_klv_variant():
    m = Muxer(_cfg())
    m.push_klv(_klv_ls(), pts=Pts90khz.from_raw(900_000))
    s = m.stream_codec_stats(0x103)
    assert s is not None
    assert isinstance(s, KlvStreamCodecStats)
    assert s.records >= 1
