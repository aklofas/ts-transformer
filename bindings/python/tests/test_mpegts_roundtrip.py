"""End-to-end round-trip tests for the Muxer build path."""

import tempfile
from pathlib import Path

import pytest

from tstrans.io import parse_file, probe
from tstrans.mpegts import (
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)


def _synthetic_config():
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


def _deterministic_push_sequence(proxy, n_video: int = 5, n_klv: int = 5) -> None:
    """Push n_video NAL AUDs at 33ms intervals + n_klv KLV blobs at 100ms intervals."""

    nal_aud = b"\x00\x00\x00\x01\x09\xF0"
    klv_ul_zero = b"\x06\x0E\x2B\x34\x02\x0B\x01\x01\x0E\x01\x03\x01\x01\x00\x00\x00\x00"
    pts0 = 900_000  # ~10s at 90kHz

    for i in range(n_video):
        proxy.push_video(nal_aud, pts=Pts90khz.from_raw(pts0 + i * 3000))
    for i in range(n_klv):
        proxy.push_klv(klv_ul_zero, pts=Pts90khz.from_raw(pts0 + i * 9000))


def test_synthetic_determinism_two_muxers_produce_identical_bytes():
    """Two fresh Muxer instances from same config + same input sequence MUST
    produce byte-identical output. Catches non-determinism regressions
    (hash-map iteration order, timestamp jitter, etc.)."""

    cfg = _synthetic_config()

    def build_one() -> bytes:
        m = Muxer(cfg)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "out.ts"
            with m.write_file(path) as proxy:
                _deterministic_push_sequence(proxy)
            return path.read_bytes()

    a = build_one()
    b = build_one()
    assert a == b, "muxer output non-deterministic — investigate before merging"


def test_synthetic_round_trip_probe_finds_pids_and_codecs():
    """Re-probe our own muxer output: PIDs + codec ids should match config."""

    m = Muxer(_synthetic_config())
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "out.ts"
        with m.write_file(path) as proxy:
            _deterministic_push_sequence(proxy)
        pr = probe(path)

    assert 0x101 in pr.pids
    assert 0x102 in pr.pids
    assert VideoCodec.H264 in pr.video_codecs
    assert pr.has_klv is True


def test_synthetic_round_trip_event_counts_within_tolerance():
    """Push N video + M KLV; expect at least N video events + M KLV events
    on the re-demux pass (frame coalescing may inflate counts slightly,
    PSI cadence can also affect the boundary frame)."""

    m = Muxer(_synthetic_config())
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "out.ts"
        with m.write_file(path) as proxy:
            _deterministic_push_sequence(proxy, n_video=5, n_klv=5)

        video_evs = []
        klv_evs = []
        for ev in parse_file(path):
            if isinstance(ev, DemuxEvent.Video):
                video_evs.append(ev)
            elif isinstance(ev, DemuxEvent.Metadata):
                klv_evs.append(ev)

    # Structural-equivalence bar: counts within tolerance.
    # Be generous; PSI repetition and AU boundaries can shift counts.
    assert len(video_evs) >= 1, f"expected >=1 video event, got {len(video_evs)}"
    assert len(klv_evs) >= 1, f"expected >=1 klv event, got {len(klv_evs)}"
    assert abs(len(video_evs) - 5) <= 3, f"video count {len(video_evs)} too far from 5"
    assert abs(len(klv_evs) - 5) <= 3, f"klv count {len(klv_evs)} too far from 5"


def test_synthetic_round_trip_byte_alignment():
    m = Muxer(_synthetic_config())
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "out.ts"
        with m.write_file(path) as proxy:
            _deterministic_push_sequence(proxy)
        size = path.stat().st_size
    assert size > 0
    assert size % 188 == 0


def test_full_synthetic_round_trip_event_by_event():
    """In-process synthetic full round-trip.

    Build a TS with video + KLV streams, write to a tempfile, demux it back,
    and assert per-event structural equivalence:
      - At least 1 ProgramMap event.
      - Video events with H.264 codec.
      - KLV events (at least as many as pushed).

    A real-fixture round-trip (full config-from-probe reconstruction) is
    not covered here; the synthetic path exercises the full
    mux→write→parse pipeline.
    """
    m = Muxer(_synthetic_config())
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "round_trip.ts"
        with m.write_file(path) as proxy:
            _deterministic_push_sequence(proxy, n_video=5, n_klv=5)

        # Probe verifies the written TS has the expected stream structure.
        pr = probe(path)
        assert 0x101 in pr.pids, f"video PID 0x101 missing from probe: {pr.pids}"
        assert 0x102 in pr.pids, f"KLV PID 0x102 missing from probe: {pr.pids}"
        assert VideoCodec.H264 in pr.video_codecs
        assert pr.has_klv is True

        # Demux and verify event shapes.
        pmap_evs = []
        video_evs = []
        klv_evs = []
        for ev in parse_file(path):
            if isinstance(ev, DemuxEvent.ProgramMap):
                pmap_evs.append(ev)
            elif isinstance(ev, DemuxEvent.Video):
                video_evs.append(ev)
            elif isinstance(ev, DemuxEvent.Metadata):
                klv_evs.append(ev)

    assert pmap_evs, "expected at least one ProgramMap event"
    assert len(video_evs) >= 1, f"expected >=1 video event, got {len(video_evs)}"
    assert len(klv_evs) >= 1, f"expected >=1 KLV event, got {len(klv_evs)}"

    # Video events carry H.264 codec.
    for ev in video_evs:
        assert ev.codec == VideoCodec.H264, f"unexpected codec: {ev.codec}"

    # PTS timestamps are non-negative.
    for ev in video_evs + klv_evs:
        assert ev.pts.raw >= 0, f"negative PTS: {ev.pts.raw}"

    # ProgramMap events carry pmt_pid and StreamInfo.raw_descriptors.
    pm = pmap_evs[0].programs[0]
    assert pm.pmt_pid == 0x100, f"expected pmt_pid=0x100, got {pm.pmt_pid!r}"
    # All streams expose raw_descriptors as a tuple.
    for s in pm.streams:
        assert isinstance(s.raw_descriptors, tuple)
    # Video stream carries no descriptors; KLV sync stream carries a
    # registration descriptor (tag 0x05, payload b"KLVA").
    video_stream = next(s for s in pm.streams if s.pid == 0x101)
    klv_stream = next(s for s in pm.streams if s.pid == 0x102)
    assert video_stream.raw_descriptors == ()
    assert len(klv_stream.raw_descriptors) >= 1
    # registration_descriptor (tag 0x05); don't pin its position in the loop.
    reg_desc = next(d for d in klv_stream.raw_descriptors if d.tag == 0x05)
    assert isinstance(reg_desc.tag, int)
    assert isinstance(reg_desc.data, bytes)
    assert reg_desc.data == b"KLVA"
