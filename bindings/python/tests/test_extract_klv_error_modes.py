"""`extract_klv(parsed=True)` error mode matrix.

Splits the previously-overloaded `skip_unknown` kwarg into two distinct
knobs:

- `skip_unknown=True` (default): silently skip payloads whose UL is not
  recognized by `parse_klv_universal` (it returns `None`).
- `skip_malformed=False` (default): raise `KlvError` when a recognized
  UL has a payload that fails to decode (truncation, bad checksum, etc.).

Tests cover the 2×2 matrix of `(skip_unknown, skip_malformed)` against
two payload conditions: unknown UL and malformed-known UL.

A mux+demux round-trip seeds the test files: the muxer's KLV pid
preserves caller bytes verbatim (`push_klv` takes raw LS bytes per the
AU-cell auto-wrap contract), so a hand-built malformed
payload survives unchanged through the file.
"""

import tempfile
from pathlib import Path

import pytest

from tstrans.exceptions import KlvError
from tstrans.io import extract_klv
from tstrans.klv import UasDatalinkLs
from tstrans.mpegts import (
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)


# Canonical UAS Datalink LS UL (MISB ST 0601). Used by
# `parse_klv_universal` to dispatch to the ST 0601 decoder.
_UAS_DATALINK_UL = bytes.fromhex("060e2b34020b01010e01030101000000")

# Random 16-byte UL not registered in any of the four supported sets.
# `parse_klv_universal` returns None for this rather than raising.
_UNKNOWN_UL = bytes(range(16))

# 6-byte NAL Access Unit Delimiter — minimum video payload to satisfy
# the muxer's "at least one video sample before KLV" implicit timing.
_NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"


def _malformed_known_payload() -> bytes:
    """Canonical UAS Datalink UL + BER short-form length declaring 10
    bytes, but only 5 bytes of payload — triggers
    `KlvErrorKind.TRUNCATED_SET` on decode."""

    return _UAS_DATALINK_UL + bytes([0x0A]) + b"\x01\x02\x03\x04\x05"


def _unknown_ul_payload() -> bytes:
    """Random 16-byte UL + valid 5-byte length-prefixed payload. The
    bytes themselves parse fine; `parse_klv_universal` returns None
    because the UL doesn't match any known set."""

    return _UNKNOWN_UL + bytes([0x05]) + b"\x01\x02\x03\x04\x05"


def _build_ts_with_klv(klv_payloads: list[bytes]) -> Path:
    """Mux a tiny `.ts` file containing one video NAL plus the given
    KLV payloads (raw — push_klv passes them through verbatim per the
    H.222.0 AU cell auto-wrap contract on SYNCHRONOUS_METADATA streams).

    Returns the path. Caller is responsible for cleanup (uses a
    NamedTemporaryFile with delete=False so the path survives function
    return)."""

    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()

    tmp = tempfile.NamedTemporaryFile(suffix=".ts", delete=False)
    tmp.close()
    path = Path(tmp.name)

    m = Muxer(cfg)
    with m.write_file(path) as proxy:
        proxy.push_video(_NAL_AUD, pts=Pts90khz.from_raw(900_000))
        for i, payload in enumerate(klv_payloads):
            proxy.push_klv(payload, pts=Pts90khz.from_raw(900_000 + i * 9000))

    return path


@pytest.fixture
def malformed_ts(tmp_path: Path) -> Path:
    path = _build_ts_with_klv([_malformed_known_payload()])
    yield path
    path.unlink(missing_ok=True)


@pytest.fixture
def unknown_ts(tmp_path: Path) -> Path:
    path = _build_ts_with_klv([_unknown_ul_payload()])
    yield path
    path.unlink(missing_ok=True)


@pytest.fixture
def mixed_ts(tmp_path: Path) -> Path:
    """File containing one unknown-UL payload and one malformed-known
    payload. Lets a single iteration exercise both branches."""

    path = _build_ts_with_klv(
        [_unknown_ul_payload(), _malformed_known_payload()]
    )
    yield path
    path.unlink(missing_ok=True)


# ---------------------------------------------------------------------------
# Signature shape
# ---------------------------------------------------------------------------


def test_extract_klv_exposes_skip_malformed_kwarg():
    """The new `skip_malformed` kwarg exists and defaults to False."""

    import inspect

    sig = inspect.signature(extract_klv)
    assert "skip_malformed" in sig.parameters
    assert sig.parameters["skip_malformed"].default is False
    assert "skip_unknown" in sig.parameters
    assert sig.parameters["skip_unknown"].default is True


# ---------------------------------------------------------------------------
# Malformed-known payload behavior
# ---------------------------------------------------------------------------


def test_malformed_raises_by_default(malformed_ts: Path):
    """Default `skip_malformed=False` — KlvError propagates from a
    truncated ST 0601 payload."""

    with pytest.raises(KlvError):
        list(extract_klv(malformed_ts, parsed=True))


def test_malformed_skipped_when_skip_malformed_true(malformed_ts: Path):
    """`skip_malformed=True` drops the row silently."""

    yielded = list(extract_klv(malformed_ts, parsed=True, skip_malformed=True))
    assert yielded == []


def test_malformed_raises_even_with_skip_unknown_true(malformed_ts: Path):
    """`skip_unknown=True` (the default) MUST NOT mask malformed-known
    payloads — conflating the two would hide decoder bugs and corruption."""

    with pytest.raises(KlvError):
        list(extract_klv(malformed_ts, parsed=True, skip_unknown=True))


def test_malformed_with_with_pts_still_raises(malformed_ts: Path):
    """`with_pts=True` doesn't change error semantics."""

    with pytest.raises(KlvError):
        list(extract_klv(malformed_ts, parsed=True, with_pts=True))


# ---------------------------------------------------------------------------
# Unknown UL behavior
# ---------------------------------------------------------------------------


def test_unknown_skipped_by_default(unknown_ts: Path):
    """`skip_unknown=True` (default) drops payloads with unrecognized
    universal labels."""

    yielded = list(extract_klv(unknown_ts, parsed=True))
    assert yielded == []


def test_unknown_yields_none_when_skip_unknown_false(unknown_ts: Path):
    """With `skip_unknown=False`, unknown-UL rows yield `None` rather
    than being suppressed."""

    yielded = list(extract_klv(unknown_ts, parsed=True, skip_unknown=False))
    assert yielded == [None]


def test_unknown_with_pts_yields_pts_none_tuple(unknown_ts: Path):
    """`with_pts=True` + `skip_unknown=False` yields `(pts, None)`."""

    yielded = list(
        extract_klv(unknown_ts, parsed=True, with_pts=True, skip_unknown=False)
    )
    assert len(yielded) == 1
    pts, typed = yielded[0]
    assert typed is None
    # pts may be None or a Pts90khz instance depending on demuxer
    # output; either is fine — the test only pins the typed slot.


# ---------------------------------------------------------------------------
# Mixed payloads + matrix coverage
# ---------------------------------------------------------------------------


def test_mixed_default_skips_unknown_raises_on_malformed(mixed_ts: Path):
    """Default flags: unknown UL skipped silently; the malformed-known
    payload then raises before any yield."""

    with pytest.raises(KlvError):
        list(extract_klv(mixed_ts, parsed=True))


def test_mixed_skip_both_yields_empty(mixed_ts: Path):
    """`skip_unknown=True, skip_malformed=True` — both rows suppressed."""

    yielded = list(
        extract_klv(mixed_ts, parsed=True, skip_unknown=True, skip_malformed=True)
    )
    assert yielded == []


def test_mixed_yield_unknown_skip_malformed(mixed_ts: Path):
    """`skip_unknown=False, skip_malformed=True` — yield None for the
    unknown row, drop the malformed row."""

    yielded = list(
        extract_klv(mixed_ts, parsed=True, skip_unknown=False, skip_malformed=True)
    )
    assert yielded == [None]


def test_mixed_yield_unknown_raise_malformed(mixed_ts: Path):
    """`skip_unknown=False, skip_malformed=False` — yield None then
    raise on the malformed payload. The exception interrupts iteration
    mid-stream."""

    it = extract_klv(mixed_ts, parsed=True, skip_unknown=False, skip_malformed=False)
    first = next(it)
    assert first is None
    with pytest.raises(KlvError):
        next(it)


# ---------------------------------------------------------------------------
# Non-parsed mode is unaffected
# ---------------------------------------------------------------------------


def test_unparsed_mode_yields_raw_bytes_regardless_of_validity(mixed_ts: Path):
    """`parsed=False` (default) shouldn't trip the new error-handling
    code path at all — every KLV PES payload is yielded as raw bytes."""

    yielded = list(extract_klv(mixed_ts))
    assert len(yielded) == 2
    assert all(isinstance(b, bytes) for b in yielded)


# ---------------------------------------------------------------------------
# Round trip: valid payload still works
# ---------------------------------------------------------------------------


def test_valid_known_payload_round_trips(tmp_path: Path):
    """A well-formed ST 0601 payload still decodes cleanly under the
    new error handling — regression guard so we don't accidentally
    break the happy path."""

    from tstrans.klv import ST_0601_UL, encode_uas_datalink

    rec = UasDatalinkLs(
        universal_label=ST_0601_UL,
        timestamp_us=1_700_000_000_000_000,
    )
    valid = encode_uas_datalink(rec)

    path = _build_ts_with_klv([valid])
    try:
        yielded = list(extract_klv(path, parsed=True))
        assert len(yielded) == 1
        assert isinstance(yielded[0], UasDatalinkLs)
    finally:
        path.unlink(missing_ok=True)
