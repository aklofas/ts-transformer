"""tstrans.io convenience helpers — parse_file (iterator) and probe (summary)."""

from pathlib import Path

import pytest

from tstrans.io import parse_file, probe, ProbeResult
from tstrans.mpegts import DemuxEvent, DemuxerConfig, StrictMode

FIXTURE = (
    Path(__file__).parent.parent.parent.parent
    / "crates" / "tst-core" / "tests" / "fixtures" / "audio" / "mp2.ts"
)


def test_parse_file_yields_events():
    events = list(parse_file(FIXTURE))
    assert len(events) > 0


def test_parse_file_accepts_str_or_path():
    a = list(parse_file(str(FIXTURE)))
    b = list(parse_file(FIXTURE))
    assert len(a) == len(b)


def test_parse_file_accepts_config():
    cfg = DemuxerConfig(strict_mode=StrictMode.OFF)
    events = list(parse_file(FIXTURE, config=cfg))
    assert len(events) > 0


def test_parse_file_first_program_map_appears_early():
    found = False
    for i, ev in enumerate(parse_file(FIXTURE)):
        if isinstance(ev, DemuxEvent.ProgramMap):
            found = True
            break
        if i > 50:
            break
    assert found


def test_probe_returns_probe_result():
    r = probe(FIXTURE)
    assert isinstance(r, ProbeResult)


def test_probe_finds_at_least_one_program():
    r = probe(FIXTURE)
    assert len(r.programs) >= 1


def test_probe_size_bytes_matches_file():
    r = probe(FIXTURE)
    assert r.size_bytes == FIXTURE.stat().st_size


def test_probe_packet_count_nonzero():
    r = probe(FIXTURE)
    assert r.packet_count > 0


def test_probe_packet_count_matches_file_size_div_188():
    # `probe().packet_count` is computed from bytes read by the probe scan
    # / 188, not from unrelated demuxer stats (`program_maps_seen` +
    # `pmt_versions_seen` + ...) — exact for properly-aligned TS files like the
    # fixture, which is 141940 bytes = 755 packets.
    r = probe(FIXTURE)
    fixture_size = FIXTURE.stat().st_size
    # mp2.ts (141940 bytes) is smaller than the 5 MiB probe cap, so the
    # whole file is scanned; packet_count == size / 188 exactly.
    assert fixture_size < 5 * 1024 * 1024, "fixture exceeds probe scan budget"
    assert fixture_size % 188 == 0, "fixture not 188-aligned (TS sync)"
    assert r.packet_count == fixture_size // 188 == 755


def test_probe_has_audio_codec_for_mp2_fixture():
    r = probe(FIXTURE)
    # mp2.ts has MP2 audio
    assert len(r.audio_codecs) > 0


# ---------------------------------------------------------------------------
# extract_klv parsed=True path
# ---------------------------------------------------------------------------

import pytest

from tstrans.io import extract_klv as _extract_klv_phase3


def test_extract_klv_parsed_kwarg_accepted():
    """The parsed= and skip_unknown= kwargs exist on extract_klv."""
    import inspect

    sig = inspect.signature(_extract_klv_phase3)
    assert "parsed" in sig.parameters
    assert "skip_unknown" in sig.parameters


def test_extract_klv_parsed_returns_typed_when_klv_present():
    """If FIXTURE has KLV PIDs, parsed=True should yield typed objects.
    If FIXTURE has no KLV (true for `audio/mp2.ts`), the iterator is
    simply empty — both outcomes are valid."""
    from pathlib import Path

    from tstrans.klv import PrecisionTimeStampPack, SecurityLs, UasDatalinkLs, VmtiLs

    fx = (
        Path(__file__).parent.parent.parent.parent
        / "crates" / "tst-core" / "tests" / "fixtures" / "audio" / "mp2.ts"
    )
    assert fx.is_file(), f"checked-in fixture missing: {fx}"

    yielded = list(_extract_klv_phase3(fx, parsed=True))
    for item in yielded:
        assert isinstance(
            item,
            (UasDatalinkLs, SecurityLs, PrecisionTimeStampPack, VmtiLs),
        )
