"""Python-side coverage for multi-cell AU cell reassembly.

End-to-end reassembly behavior is verified in the Rust integration tests.
Python tests here cover only the binding surface — that the new
fields exist on `_MetadataEvent`, that `MultiCellAuReason` is importable as a
PyO3 `eq_int` enum, and that `_NonConformantEvent` carries the optional
typed reason.
"""

from __future__ import annotations

import enum

from tstrans.mpegts import (
    DemuxEvent,
    MultiCellAuReason,
    NonConformantKind,
    Pts90khz,
    StreamId,
    StreamKindTag,
)


def test_multi_cell_au_reason_is_an_enum_with_six_variants() -> None:
    """The reason enum mirrors Rust's `MultiCellAuReason`."""
    assert MultiCellAuReason.ORPHAN is not None
    assert MultiCellAuReason.SEQUENCE_GAP is not None
    assert MultiCellAuReason.CONCURRENT_FIRST is not None
    assert MultiCellAuReason.OVERFLOW is not None
    assert MultiCellAuReason.OVERFLOW_TOTAL is not None
    assert MultiCellAuReason.TOO_MANY_PIDS is not None


def test_multi_cell_au_reason_equality_uses_eq_int_semantics() -> None:
    """PyO3 `eq_int` enums compare with `==` (not `is`).

    See `reference_pyo3_0_22_rust_2024_patterns` memory.
    """
    assert MultiCellAuReason.ORPHAN == MultiCellAuReason.ORPHAN
    assert MultiCellAuReason.ORPHAN != MultiCellAuReason.OVERFLOW
    assert MultiCellAuReason.SEQUENCE_GAP != MultiCellAuReason.CONCURRENT_FIRST
    assert MultiCellAuReason.OVERFLOW_TOTAL != MultiCellAuReason.OVERFLOW
    assert MultiCellAuReason.TOO_MANY_PIDS != MultiCellAuReason.OVERFLOW_TOTAL


def test_metadata_event_has_was_reassembled_and_cell_count_with_defaults() -> None:
    """Reassembly fields exist on `_MetadataEvent` with backward-compat defaults."""
    from tstrans.mpegts import MetadataKindTag, _MetadataEvent

    stream = StreamId(
        pid=0x100,
        kind=StreamKindTag.KLV_SYNC,
        codec=None,
        program_number=1,
    )
    # Construct without the new fields — defaults must keep the call working.
    ev = _MetadataEvent(
        stream=stream,
        pts=Pts90khz.from_raw(90000),
        kind=MetadataKindTag.KLV_SYNC_AU_CELL,
        payload=b"",
    )
    assert ev.was_reassembled is False
    assert ev.cell_count == 1


def test_metadata_event_accepts_explicit_reassembly_fields() -> None:
    """Multi-cell reassembled AU populates the new fields."""
    from tstrans.mpegts import MetadataKindTag, _MetadataEvent

    stream = StreamId(
        pid=0x100,
        kind=StreamKindTag.KLV_SYNC,
        codec=None,
        program_number=1,
    )
    ev = _MetadataEvent(
        stream=stream,
        pts=Pts90khz.from_raw(90000),
        kind=MetadataKindTag.KLV_SYNC_AU_CELL,
        payload=b"\x01\x02\x03",
        was_reassembled=True,
        cell_count=3,
    )
    assert ev.was_reassembled is True
    assert ev.cell_count == 3


def test_non_conformant_event_has_optional_multi_cell_reason() -> None:
    """`_NonConformantEvent` carries the typed reason on MULTI_CELL_AU."""
    from tstrans.mpegts import _NonConformantEvent

    stream = StreamId(
        pid=0x100,
        kind=StreamKindTag.KLV_SYNC,
        codec=None,
        program_number=1,
    )

    # Default: None for any non-MultiCellAu issue.
    ev = _NonConformantEvent(
        stream=stream,
        issue="some other issue",
        kind=NonConformantKind.PCR_ANOMALY,
    )
    assert ev.multi_cell_au_reason is None

    # MultiCellAu issues carry the typed reason.
    ev2 = _NonConformantEvent(
        stream=stream,
        issue="orphan continuation",
        kind=NonConformantKind.MULTI_CELL_AU,
        multi_cell_au_reason=MultiCellAuReason.ORPHAN,
    )
    assert ev2.multi_cell_au_reason == MultiCellAuReason.ORPHAN


def test_multi_cell_au_reason_in_module_all() -> None:
    """`MultiCellAuReason` is in `tstrans.mpegts.__all__`."""
    from tstrans import mpegts

    assert "MultiCellAuReason" in mpegts.__all__


def test_klv_alias_is_same_object_as_metadata() -> None:
    """`DemuxEvent.Klv` is the same class object as `DemuxEvent.Metadata` (deprecated alias).

    The alias exists so existing `isinstance(e, DemuxEvent.Klv)` and
    `case DemuxEvent.Klv(...)` patterns continue to work unchanged. Remove at 1.0.
    """
    from tstrans.mpegts import _MetadataEvent

    assert DemuxEvent.Klv is DemuxEvent.Metadata
    assert DemuxEvent.Klv is _MetadataEvent
    # dataclass field set — sanity check on the public surface.
    field_names = {f.name for f in DemuxEvent.Metadata.__dataclass_fields__.values()}
    assert "was_reassembled" in field_names
    assert "cell_count" in field_names
