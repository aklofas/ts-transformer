"""DemuxerConfig parity — plan #96 Wave B.

Three Rust-side `DemuxerConfig` fields that were previously not bridged
to Python (per the explicit "not yet bridged" warning in the dataclass
docstring) are now exposed:

- `av1_carriage: Optional[Av1CarriageMode]`
- `au_cell_cap_per_pid: Optional[int]`
- `lenient_psi_reassembly: bool`

Coverage strategy:

- Dataclass-shape tests: defaults, kwarg acceptance, frozen-ness for each
  new field.
- `build_demuxer()` plumbing: a Python `DemuxerConfig(av1_carriage=...)`
  must surface the matching carriage mode on the underlying Rust demuxer.
  We assert via an end-to-end AV1 round-trip — only a properly-wired
  demuxer can recover the AV1 OBUs from a sender of the matching mode
  without raising binding-nonconformance diagnostics.
- `au_cell_cap_per_pid`: smoke that an explicit cap doesn't break a
  normal demux (the Rust side has its own dedicated overflow tests).
- `lenient_psi_reassembly`: smoke (the Rust side covers the strict-vs-
  lenient behavior gap directly).
"""

from __future__ import annotations

import dataclasses
from pathlib import Path

import pytest

from tstrans.mpegts import (
    Av1CarriageMode,
    DemuxEvent,
    Demuxer,
    DemuxerConfig,
    Muxer,
    MuxerConfig,
    MuxerProgramConfigBuilder,
    NonConformantKind,
    Pts90khz,
    StrictMode,
    VideoCodec,
    _NonConformantEvent,
    _VideoEvent,
)


# ---------------------------------------------------------------------------
# Dataclass-shape tests for the 3 new fields
# ---------------------------------------------------------------------------


def test_demuxer_config_av1_carriage_defaults_to_none() -> None:
    """`None` means defer to Rust default (`Mpeg2TsBinding`)."""
    cfg = DemuxerConfig()
    assert cfg.av1_carriage is None


def test_demuxer_config_au_cell_cap_per_pid_defaults_to_none() -> None:
    """`None` means defer to Rust default of 1 MiB."""
    cfg = DemuxerConfig()
    assert cfg.au_cell_cap_per_pid is None


def test_demuxer_config_lenient_psi_reassembly_defaults_to_false() -> None:
    """Spec-strict by default — matches the ffmpeg parity stance."""
    cfg = DemuxerConfig()
    assert cfg.lenient_psi_reassembly is False


def test_demuxer_config_accepts_av1_carriage_kwarg() -> None:
    cfg = DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)
    assert cfg.av1_carriage is Av1CarriageMode.INTEROP_RAW_OBU


def test_demuxer_config_accepts_au_cell_cap_per_pid_kwarg() -> None:
    cfg = DemuxerConfig(au_cell_cap_per_pid=2 * 1024 * 1024)
    assert cfg.au_cell_cap_per_pid == 2 * 1024 * 1024


def test_demuxer_config_accepts_lenient_psi_reassembly_kwarg() -> None:
    cfg = DemuxerConfig(lenient_psi_reassembly=True)
    assert cfg.lenient_psi_reassembly is True


def test_demuxer_config_new_fields_are_frozen() -> None:
    """The dataclass is `frozen=True, slots=True` — assignment must fail."""
    cfg = DemuxerConfig()
    for field in ("av1_carriage", "au_cell_cap_per_pid", "lenient_psi_reassembly"):
        with pytest.raises(dataclasses.FrozenInstanceError):
            setattr(cfg, field, None)


# ---------------------------------------------------------------------------
# Demuxer constructor accepts configs with the new fields
# ---------------------------------------------------------------------------


def test_demuxer_accepts_config_with_av1_carriage() -> None:
    """Passing the dataclass to `Demuxer(config=...)` must not raise."""
    Demuxer(DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU))


def test_demuxer_accepts_config_with_au_cell_cap_per_pid() -> None:
    Demuxer(DemuxerConfig(au_cell_cap_per_pid=512 * 1024))


def test_demuxer_accepts_config_with_lenient_psi_reassembly() -> None:
    Demuxer(DemuxerConfig(lenient_psi_reassembly=True))


def test_demuxer_accepts_all_new_fields_together() -> None:
    Demuxer(
        DemuxerConfig(
            strict_mode=StrictMode.OFF,
            av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU,
            au_cell_cap_per_pid=512 * 1024,
            lenient_psi_reassembly=True,
        )
    )


# ---------------------------------------------------------------------------
# AV1 carriage mode end-to-end parity: the actual point of Wave B
# ---------------------------------------------------------------------------


def _synthetic_av1_au() -> bytes:
    """Build a 4-OBU AV1 access unit (TD + SeqHeader + FrameHeader +
    TileGroup). Mirrors the corpus used by
    `crates/tst-core/tests/av1_carriage_roundtrip.rs`."""

    def obu(obu_type: int, body: bytes) -> bytes:
        # AV1 OBU header: (obu_type << 3) | 0b010 — obu_has_size_field=1.
        header = (obu_type << 3) | 0x02
        return bytes([header, len(body)]) + body

    return (
        obu(2, b"")  # Temporal Delimiter
        + obu(1, b"\x00\x00")  # Sequence Header (placeholder)
        + obu(3, b"\x00")  # Frame Header (placeholder)
        + obu(4, b"\x00\x01\x02")  # Tile Group (placeholder)
    )


def _build_av1_ts(mux_mode: Av1CarriageMode) -> bytes:
    """Synthesize a single-AU TS stream under the given mux carriage."""
    program = MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
    program.add_video(pid=0x101, codec=VideoCodec.AV1)
    cfg_builder = MuxerConfig.builder()
    cfg_builder.add_program(program.build())
    cfg_builder.av1_carriage(mux_mode)
    mux = Muxer(cfg_builder.build())
    handle = mux.video_handles()[0]
    mux.push_video_to(
        handle,
        _synthetic_av1_au(),
        pts=Pts90khz(90_000),
        key_frame=True,
    )
    # Drain the muxer in 1316-byte (7-TS-packet) chunks to mirror the
    # SRT bundle boundary used elsewhere. `Muxer.pull` takes a
    # caller-supplied bytearray and returns the byte count written.
    out = bytearray()
    buf = bytearray(1316)
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _classify_demux_events(ts_bytes: bytes, config: DemuxerConfig) -> dict:
    """Feed `ts_bytes` through a `Demuxer(config)` and return a small
    summary that the carriage-mode tests below assert against."""
    demux = Demuxer(config)
    demux.feed(ts_bytes)
    demux.flush()

    saw_sample = False
    saw_wrong_stream_id = False
    saw_missing_framing = False
    while True:
        ev = demux.next_event()
        if ev is None:
            break
        if isinstance(ev, _VideoEvent):
            saw_sample = True
        elif isinstance(ev, _NonConformantEvent):
            if ev.kind is NonConformantKind.AV1_WRONG_STREAM_ID:
                saw_wrong_stream_id = True
            elif ev.kind is NonConformantKind.AV1_MISSING_TS_OBU_FRAMING:
                saw_missing_framing = True
    return {
        "saw_sample": saw_sample,
        "saw_wrong_stream_id": saw_wrong_stream_id,
        "saw_missing_framing": saw_missing_framing,
    }


def test_av1_interop_round_trip_with_matching_demux_carriage() -> None:
    """Interop sender + interop demuxer: Sample arrives, no binding
    nonconformance fires."""
    ts = _build_av1_ts(Av1CarriageMode.INTEROP_RAW_OBU)
    summary = _classify_demux_events(
        ts, DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)
    )
    assert summary["saw_sample"], "AV1 sample must round-trip"
    assert not summary["saw_wrong_stream_id"], (
        "matching interop carriage must not raise AV1_WRONG_STREAM_ID"
    )
    assert not summary["saw_missing_framing"], (
        "matching interop carriage must not raise AV1_MISSING_TS_OBU_FRAMING"
    )


def test_av1_binding_round_trip_with_matching_demux_carriage() -> None:
    """Binding sender + binding demuxer (explicit kwarg, not just
    default): same proof as above for the spec-conformant mode. Set
    the kwarg explicitly to exercise the `None -> not-set` plumbing
    fork in `build_demuxer()` against the `Some(mode)` fork — both
    must produce a binding-mode demuxer."""
    ts = _build_av1_ts(Av1CarriageMode.MPEG2_TS_BINDING)
    summary = _classify_demux_events(
        ts, DemuxerConfig(av1_carriage=Av1CarriageMode.MPEG2_TS_BINDING)
    )
    assert summary["saw_sample"]
    assert not summary["saw_wrong_stream_id"]
    assert not summary["saw_missing_framing"]


def test_av1_interop_sender_into_binding_demuxer_surfaces_both_issues() -> None:
    """The mismatch case proves the carriage-mode plumbing affects the
    demuxer — but in the raw-first model the diagnostic surface is SPLIT
    across two layers (mirrors the Rust/tst-c rewrites, plan Tasks 3.1/3.2):

    - `AV1_WRONG_STREAM_ID` is a PES-layer issue (stream_id=0xE0 vs 0xBD)
      → still a demuxer `NonConformant` event.
    - `Av1MissingTsObuFraming` is an ES-content issue (no
      ts_open_bitstream_unit framing) → no longer a demux event; it now
      surfaces from the opt-in `codec.split_units(raw, VideoCodec.AV1)`.

    The Sample still arrives via the lenient raw-OBU fallback, carrying the
    raw AU on `.raw`."""
    import tstrans.codec as codec

    ts = _build_av1_ts(Av1CarriageMode.INTEROP_RAW_OBU)
    # No `av1_carriage=` kwarg — exercises the `None` default path that
    # defers to the Rust `Mpeg2TsBinding` default (the binding demuxer).
    demux = Demuxer(DemuxerConfig())
    demux.feed(ts)
    demux.flush()

    saw_wrong_stream_id = False
    raw_au = None
    while True:
        ev = demux.next_event()
        if ev is None:
            break
        if isinstance(ev, _NonConformantEvent):
            if ev.kind is NonConformantKind.AV1_WRONG_STREAM_ID:
                saw_wrong_stream_id = True
            # The demuxer no longer raises the ES-content missing-framing
            # issue as an event — that moved to split_units (asserted below).
            assert ev.kind is not NonConformantKind.AV1_MISSING_TS_OBU_FRAMING, (
                "demuxer must not raise the ES-content AV1_MISSING_TS_OBU_FRAMING"
            )
        elif isinstance(ev, _VideoEvent):
            raw_au = ev.raw

    # PES-layer issue is still a demux event — proves the carriage knob is
    # wired (a silently-ignored kwarg would not surface AV1_WRONG_STREAM_ID).
    assert saw_wrong_stream_id, (
        "mismatched carriage must surface AV1_WRONG_STREAM_ID — if it doesn't, "
        "build_demuxer() is silently ignoring the av1_carriage field"
    )
    # The lenient raw-OBU fallback still emits the raw AU Sample.
    assert raw_au is not None, "lenient raw-OBU fallback should still emit the Sample"

    # The opt-in ES split now carries the missing-framing conformance signal
    # (and still recovers the OBUs via the raw-OBU fallback). The issues are
    # the Rust `NonConformantIssue` Display strings; match the real substring
    # from `Av1MissingTsObuFraming`'s message ("missing ts_open_bitstream_unit").
    units, issues = codec.split_units(raw_au, VideoCodec.AV1)
    assert len(units) >= 1, "split_units should recover the raw-OBU AU"
    assert any("missing ts_open_bitstream_unit" in i for i in issues), (
        f"split_units should report the AV1 missing-framing issue for raw-OBU "
        f"carriage; got issues={issues!r}"
    )


# ---------------------------------------------------------------------------
# io.parse_file / io.probe / io.extract_klv config plumbing — smoke
# ---------------------------------------------------------------------------


def test_io_parse_file_threads_av1_carriage(tmp_path: Path) -> None:
    """`tstrans.io.parse_file(path, config=...)` must honor the new
    fields. The audit found this kwarg already plumbs through, but
    the previous DemuxerConfig didn't expose `av1_carriage` so the
    end-to-end path was never exercised."""
    import tstrans.io as io

    ts_path = tmp_path / "av1_interop.ts"
    ts_path.write_bytes(_build_av1_ts(Av1CarriageMode.INTEROP_RAW_OBU))

    config = DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)
    saw_sample = False
    saw_wrong_stream_id = False
    for ev in io.parse_file(ts_path, config=config):
        if isinstance(ev, _VideoEvent):
            saw_sample = True
        elif isinstance(ev, _NonConformantEvent):
            if ev.kind is NonConformantKind.AV1_WRONG_STREAM_ID:
                saw_wrong_stream_id = True
    assert saw_sample
    assert not saw_wrong_stream_id, (
        "io.parse_file must thread av1_carriage from config → demuxer"
    )


# ---------------------------------------------------------------------------
# unwrap_timestamps — dataclass shape + wire-level parity
#
# Mirrors the Rust wire test
# `crates/tst-core/tests/mpegts/pts_unwrap.rs::
# reordered_pts_across_wrap_does_not_double_the_epoch`: mux a real TS with
# `Muxer` so the raw 33-bit wire PTS values land exactly where a real
# encoder would put them, then demux with the knob on and off.
# ---------------------------------------------------------------------------

_WRAP = 1 << 33  # H.222.0 §2.4.3.6 33-bit PTS/DTS rollover boundary


def test_demuxer_config_unwrap_timestamps_defaults_to_false() -> None:
    cfg = DemuxerConfig()
    assert cfg.unwrap_timestamps is False


def test_demuxer_config_accepts_unwrap_timestamps_kwarg() -> None:
    cfg = DemuxerConfig(unwrap_timestamps=True)
    assert cfg.unwrap_timestamps is True


def test_demuxer_accepts_config_with_unwrap_timestamps() -> None:
    Demuxer(DemuxerConfig(unwrap_timestamps=True))


def _minimal_h264_au() -> bytes:
    # Annex-B: AUD (nal_type=9) + IDR (nal_type=5). The raw-first demuxer
    # doesn't parse the AU contents — any bytes round-trip.
    return bytes(
        [0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC]
    )


def _wrap_crossing_reordered_ts() -> bytes:
    """Mux a 4-sample video stream whose composition order straddles the
    33-bit PTS wrap: two pre-wrap values (`WRAP-100`, `WRAP-50`)
    interleaved with two post-wrap ones (`100`, `200`) — i.e. the
    pre-wrap `WRAP-50` is delivered late, after the wrap has already
    been observed. Same sequence as the Rust wire test."""
    program = MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    program.add_video(pid=0x100, codec=VideoCodec.H264)
    builder = MuxerConfig.builder()
    builder.add_program(program.build())
    mux = Muxer(builder.build())
    au = _minimal_h264_au()
    for raw in (_WRAP - 100, 100, _WRAP - 50, 200):
        mux.push_video(au, pts=Pts90khz(raw), key_frame=True)

    out = bytearray()
    buf = bytearray(1316)
    while True:
        n = mux.pull(buf)
        if n == 0:
            break
        out.extend(buf[:n])
    return bytes(out)


def _video_pts_ticks(ts_bytes: bytes, *, unwrap: bool) -> list[int]:
    """Ordered raw `pts.raw` (ticks) of every video Sample on PID 0x100."""
    demux = Demuxer(DemuxerConfig(unwrap_timestamps=unwrap))
    demux.feed(ts_bytes)
    demux.flush()
    pts: list[int] = []
    while True:
        ev = demux.next_event()
        if ev is None:
            break
        if isinstance(ev, _VideoEvent) and ev.stream.pid == 0x100:
            pts.append(ev.pts.raw)
    return pts


def test_unwrap_timestamps_off_preserves_raw_wire_values_across_reorder() -> None:
    """Default off: byte-for-byte today's behavior — the raw wire values
    come through unchanged, reorder and all."""
    ts = _wrap_crossing_reordered_ts()
    assert _video_pts_ticks(ts, unwrap=False) == [
        _WRAP - 100,
        100,
        _WRAP - 50,
        200,
    ]


def test_unwrap_timestamps_on_unwraps_reorder_without_doubling_the_epoch() -> None:
    """Opt-in: each sample lands in the epoch its signed 33-bit delta
    implies; a second epoch (2*WRAP) would mean the reorder was mistaken
    for a wrap."""
    ts = _wrap_crossing_reordered_ts()
    got = _video_pts_ticks(ts, unwrap=True)
    assert got == [_WRAP - 100, _WRAP + 100, _WRAP - 50, _WRAP + 200]
