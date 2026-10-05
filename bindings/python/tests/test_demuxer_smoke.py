"""Smoke tests for the PyDemuxer wrap. Feeds a small real fixture
from tst-core's test suite and asserts we get sensible events."""

from pathlib import Path

import pytest

from tstrans.mpegts import (
    Demuxer,
    DemuxerConfig,
    DemuxEvent,
    StreamKindTag,
    VideoCodec,
    AudioCodec,
)

# Path to a small real fixture committed in tst-core's test tree.
# 141 KB, MP2 audio + small video.
FIXTURE = (
    Path(__file__).parent.parent.parent.parent
    / "crates" / "tst-core" / "tests" / "fixtures" / "audio" / "mp2.ts"
)


def test_fixture_exists():
    assert FIXTURE.is_file(), f"missing fixture: {FIXTURE}"


def test_demuxer_constructs_with_default_config():
    d = Demuxer()
    assert d is not None


def test_demuxer_constructs_with_config():
    d = Demuxer(DemuxerConfig())
    assert d is not None


def test_feed_small_fixture_yields_events():
    d = Demuxer()
    data = FIXTURE.read_bytes()
    d.feed(data)
    d.flush()
    events = list(d)
    assert len(events) > 0


def test_first_event_is_program_map():
    d = Demuxer()
    d.feed(FIXTURE.read_bytes())
    d.flush()
    events = list(d)
    # The first thing the demuxer emits when it discovers the PSI is
    # a ProgramMap event.
    assert isinstance(events[0], DemuxEvent.ProgramMap)


def test_some_event_is_audio_or_video_sample():
    d = Demuxer()
    d.feed(FIXTURE.read_bytes())
    d.flush()
    events = list(d)
    sample_kinds = {
        type(e).__name__ for e in events
        if isinstance(e, (DemuxEvent.Video, DemuxEvent.Audio, DemuxEvent.Subtitle))
    }
    # mp2.ts has at least audio
    assert sample_kinds, f"no audio/video/subtitle samples in {len(events)} events"


def test_demuxer_iterator_drains():
    d = Demuxer()
    d.feed(FIXTURE.read_bytes())
    d.flush()
    first_pass = list(d)
    second_pass = list(d)
    # After draining, the iterator is empty until more bytes are fed.
    assert second_pass == []


def test_demuxer_stats_returns_dict():
    d = Demuxer()
    d.feed(FIXTURE.read_bytes())
    d.flush()
    list(d)  # drain
    stats = d.stats()
    assert isinstance(stats, dict)
    # Stats should have at least some keys populated
    assert len(stats) > 0


# ---------------------------------------------------------------------------
# Bytes-like input matrix for Demuxer.feed.
#
# Demuxer.feed accepts
# any object that exposes the Python buffer protocol: `bytes`, `bytearray`,
# `memoryview`, NumPy arrays, etc. These tests pin the contract by feeding
# the same fixture as four different bytes-like wrappers.
# ---------------------------------------------------------------------------


def _drain(d: Demuxer) -> list:
    d.flush()
    return list(d)


def test_feed_accepts_bytes():
    d = Demuxer()
    d.feed(FIXTURE.read_bytes())  # bytes
    assert len(_drain(d)) > 0


def test_feed_accepts_bytearray():
    d = Demuxer()
    d.feed(bytearray(FIXTURE.read_bytes()))
    assert len(_drain(d)) > 0


def test_feed_accepts_memoryview_of_bytes():
    d = Demuxer()
    d.feed(memoryview(FIXTURE.read_bytes()))
    assert len(_drain(d)) > 0


def test_feed_accepts_memoryview_of_bytearray():
    d = Demuxer()
    d.feed(memoryview(bytearray(FIXTURE.read_bytes())))
    assert len(_drain(d)) > 0


def test_feed_bytes_vs_bytearray_event_count_matches():
    """Equivalence: same payload fed as bytes vs bytearray yields the
    same number of events. Cheap sanity that the buffer-protocol path
    doesn't silently truncate or duplicate."""
    data = FIXTURE.read_bytes()
    d_bytes = Demuxer()
    d_bytes.feed(data)
    n_bytes = len(_drain(d_bytes))

    d_ba = Demuxer()
    d_ba.feed(bytearray(data))
    n_ba = len(_drain(d_ba))

    assert n_bytes == n_ba
