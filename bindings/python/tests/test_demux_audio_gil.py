"""Raw-first audio smoke test.

In the raw-first model `Demuxer.next_event()` no longer parses audio frames
during conversion — every Audio event carries raw bytes on `.raw`, and frame
parsing is opt-in via `.parse()`. This file verifies that observable shape.

There is no GIL-held frame parsing inside `next_event()` to guard; GIL
release happens inside the opt-in `tstrans.codec.parse_audio` / `.parse()`.
"""

from pathlib import Path

import pytest

import tstrans
from tstrans.mpegts import Demuxer, DemuxerConfig


def test_next_event_audio_typed_parse_unchanged() -> None:
    """Smoke — raw-first surface: every Audio event carries `.raw` bytes and
    the opt-in `.parse()` returns a typed frame list.

    Uses an existing small AAC TS fixture if present; skips if absent.
    """
    p = Path("bindings/python/tests/fixtures/aac_minimal.ts")
    if not p.exists():
        pytest.skip("aac_minimal.ts not present")
    dx = Demuxer(DemuxerConfig())
    dx.feed(p.read_bytes())
    dx.flush()
    audio_events = [e for e in dx if isinstance(e, tstrans.mpegts.DemuxEvent.Audio)]
    assert audio_events, "expected at least one Audio event"
    for ev in audio_events:
        assert isinstance(ev.raw, (bytes, bytearray)), (
            f"expected bytes raw, got {type(ev.raw).__name__}"
        )
        assert isinstance(ev.parse(), list), (
            f"expected list from parse(), got {type(ev.parse()).__name__}"
        )
