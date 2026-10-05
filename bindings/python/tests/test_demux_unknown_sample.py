"""SamplePayload::Unknown must surface as a typed
event carrying raw stream_type + payload bytes, not collapse to a
NonConformant diagnostic."""

import pytest
import tstrans
from tstrans.mpegts import DemuxEvent, Demuxer, DemuxerConfig


def _build_ts_with_unknown_stream() -> bytes:
    """Mux a single PES of an unrecognized stream_type (0x7F, ITU-T defined
    user private). Demuxer cannot classify it as Video/Audio/Subtitle/KLV."""
    # Minimal synthetic builder lives at tests/_builders/unknown_stream.py
    from _builders.unknown_stream import build_unknown_stream_ts
    return build_unknown_stream_ts(stream_type=0x7F, payload=b"hello-private-payload")


def test_unknown_sample_event_emitted_with_raw_bytes() -> None:
    ts = _build_ts_with_unknown_stream()
    dx = Demuxer(DemuxerConfig())
    dx.feed(ts)
    dx.flush()

    events = list(dx)
    unknown = [e for e in events if isinstance(e, DemuxEvent.UnknownSample)]
    assert len(unknown) == 1, f"expected 1 UnknownSample, got {events!r}"
    ev = unknown[0]
    assert ev.stream_type == 0x7F
    assert ev.payload == b"hello-private-payload"
    assert isinstance(ev.payload, bytes)


def test_unknown_sample_event_not_collapsed_to_nonconformant() -> None:
    ts = _build_ts_with_unknown_stream()
    dx = Demuxer(DemuxerConfig())
    dx.feed(ts)
    dx.flush()
    events = list(dx)
    # The whole point: don't surface this as NonConformant anymore.
    ncs = [e for e in events if isinstance(e, DemuxEvent.NonConformant)
           and "unknown stream_type" in e.issue]
    assert ncs == [], f"unknown sample should not appear as NonConformant; got {ncs!r}"
