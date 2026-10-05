"""DemuxEvent DataFrame adapter tests.

Tests `tstrans.pandas.events_to_dataframe` — the union-schema dispatcher
across all DemuxEvent kinds. Fixture availability drives skips for the
real-stream paths; an unconditional hand-built path covers the empty and
NonConformantEvent cases.

There is no separate `_PatEvent`: PAT data lives on
`_ProgramMapEvent.programs`. Tests use
`audio/aac-adts.ts` (Video + Audio + ProgramMap) and
`subtitles/subtitle_with_klv_same_program.ts` (Subtitle + KLV +
ProgramMap).
"""

import pathlib

import pytest

pytestmark = pytest.mark.pandas

# The `pandas` marker filters at run-time, but pytest still imports test
# modules at collection. Skip the whole module when the [pandas] extra
# isn't installed (the python-core CI job runs without it).
pytest.importorskip("pandas")

import pandas as pd  # noqa: E402

from tstrans.io import parse_file  # noqa: E402
from tstrans.mpegts import (  # noqa: E402
    DiscontinuityKindTag,
    MetadataKindTag,
    NonConformantKind,
    Pts90khz,
    StreamId,
    StreamKindTag,
    VideoCodec,
    _DiscontinuityEvent,
    _MetadataEvent,
    _NonConformantEvent,
    _ReconnectDiscontinuityEvent,
    _UnknownSampleEvent,
)
from tstrans.pandas import events_to_dataframe  # noqa: E402


# --- fixtures ------------------------------------------------------------

_FIXTURE_ROOT = pathlib.Path(__file__).parent.parent.parent.parent / "crates" / "tst-core" / "tests" / "fixtures"


def _real_ts_fixture() -> pathlib.Path:
    """Return a real .ts fixture path. Hard error if none of the checked-in candidates exist."""
    candidates = [
        "audio/aac-adts.ts",
        "subtitles/subtitle_with_klv_same_program.ts",
    ]
    for candidate in candidates:
        p = _FIXTURE_ROOT / candidate
        if p.exists():
            return p
    raise FileNotFoundError(
        f"no checked-in .ts fixture from {candidates} found under {_FIXTURE_ROOT}"
    )


# --- tests ---------------------------------------------------------------


def test_events_to_dataframe_returns_dataframe():
    events = list(parse_file(_real_ts_fixture()))
    df = events_to_dataframe(events)
    assert isinstance(df, pd.DataFrame)


def test_events_to_dataframe_has_kind_column():
    events = list(parse_file(_real_ts_fixture()))
    df = events_to_dataframe(events)
    assert "kind" in df.columns


def test_events_to_dataframe_union_columns_present():
    events = list(parse_file(_real_ts_fixture()))
    df = events_to_dataframe(events)
    expected = {
        "kind", "pts_raw", "pts_ms", "dts_ms", "pid",
        "stream_type", "codec", "payload_len", "nal_count",
        "random_access", "has_codec_parse_error", "issue", "issue_kind",
    }
    assert expected.issubset(set(df.columns)), f"missing {expected - set(df.columns)}"


def test_events_to_dataframe_empty_returns_empty_df():
    df = events_to_dataframe([])
    assert isinstance(df, pd.DataFrame)
    assert len(df) == 0


def test_events_to_dataframe_sample_rows_have_pts_ms():
    events = list(parse_file(_real_ts_fixture()))
    df = events_to_dataframe(events)
    sample_rows = df[df["kind"] == "Sample"]
    assert len(sample_rows) > 0, (
        f"no Sample events in {_real_ts_fixture()} — "
        "aac-adts.ts is expected to produce Audio/Sample events"
    )
    # At least some Sample rows should have non-NaN pts_ms
    assert sample_rows["pts_ms"].notna().any()


def test_events_to_dataframe_non_conformant_rows_have_issue():
    # Hand-build a NonConformantEvent so this test runs unconditionally
    # (real fixtures don't reliably emit NonConformantEvent rows).
    sid = StreamId(
        pid=256,
        kind=StreamKindTag.VIDEO,
        codec=VideoCodec.H264,
        program_number=1,
    )
    nce = _NonConformantEvent(
        stream=sid,
        kind=NonConformantKind.MALFORMED_PES,
        issue="hand-built test row",
    )
    df = events_to_dataframe([nce])
    nci_rows = df[df["kind"] == "NonConformant"]
    assert len(nci_rows) == 1
    assert nci_rows["issue"].iloc[0] == "hand-built test row"
    assert nci_rows["issue_kind"].iloc[0] == "MALFORMED_PES"
    assert nci_rows["pid"].iloc[0] == 256


def test_events_to_dataframe_default_range_index():
    events = list(parse_file(_real_ts_fixture()))
    df = events_to_dataframe(events)
    assert isinstance(df.index, pd.RangeIndex)


# --- extra coverage for hand-built mixed batch ---------------------------


def test_events_to_dataframe_reconnect_discontinuity_has_kind_only():
    """ReconnectDiscontinuity carries no stream/pts/payload — only the kind label."""
    rde = _ReconnectDiscontinuityEvent()
    df = events_to_dataframe([rde])
    assert len(df) == 1
    assert df["kind"].iloc[0] == "ReconnectDiscontinuity"
    # All payload/stream columns should be NaN/None
    assert pd.isna(df["pid"].iloc[0])
    assert pd.isna(df["pts_ms"].iloc[0])


def test_events_to_dataframe_klv_event_has_metadata_kind():
    """Hand-built _MetadataEvent — verify kind=Metadata, payload_len set, nal_count is None."""
    sid = StreamId(
        pid=257,
        kind=StreamKindTag.KLV_SYNC,
        codec=None,
        program_number=1,
    )
    klv = _MetadataEvent(
        stream=sid,
        pts=Pts90khz(raw=900000),  # 10 seconds
        kind=MetadataKindTag.KLV_SYNC_AU_CELL,
        payload=b"\x06\x0e\x2b\x34\x02\x0b\x01\x01\x0e\x01\x03\x01\x01\x00\x00\x00",  # 16 bytes
    )
    df = events_to_dataframe([klv])
    row = df.iloc[0]
    assert row["kind"] == "Metadata"
    assert row["pts_ms"] is not None and not pd.isna(row["pts_ms"])
    assert row["payload_len"] == 16
    # nal_count must be None — KLV payload is bytes, not NAL units
    assert pd.isna(row["nal_count"])


def test_events_to_dataframe_discontinuity_has_kind_only():
    """Hand-built _DiscontinuityEvent — verify kind column populated, issue/issue_kind both None."""
    sid = StreamId(
        pid=256,
        kind=StreamKindTag.VIDEO,
        codec=VideoCodec.H264,
        program_number=1,
    )
    de = _DiscontinuityEvent(
        stream=sid,
        kind=DiscontinuityKindTag.CONTINUITY_JUMP,
    )
    df = events_to_dataframe([de])
    row = df.iloc[0]
    assert row["kind"] == "Discontinuity"
    # issue and issue_kind must both be None — DiscontinuityKindTag is NOT an "issue"
    assert pd.isna(row["issue"]) or row["issue"] is None
    assert pd.isna(row["issue_kind"]) or row["issue_kind"] is None


def test_events_to_dataframe_unknown_sample_row_shape():
    """UnknownSample rows must carry kind='unknown_sample',
    pid, raw stream_type int, and payload_len."""
    sid = StreamId(
        pid=0x101,
        kind=StreamKindTag.UNKNOWN,
        codec=None,
        program_number=1,
    )
    ev = _UnknownSampleEvent(
        stream=sid,
        pts=Pts90khz(raw=0),
        dts=None,
        stream_type=0x7F,
        payload=b"hello-private-payload",
    )
    df = events_to_dataframe([ev])
    assert len(df) == 1
    row = df.iloc[0]
    assert row["kind"] == "unknown_sample"
    assert row["pid"] == 0x101
    assert row["stream_type"] == 0x7F
    assert row["payload_len"] == len(b"hello-private-payload")
