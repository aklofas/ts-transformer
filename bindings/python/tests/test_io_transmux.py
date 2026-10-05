"""tio.transmux — the demux→edit→remux workflow.

Covers: lazy muxer construction from the first ProgramMap, byte-faithful
copy semantics, the corrector acceptance workflow (patch_uas_datalink),
v1 single-program scope errors, drop= filtering, atomic mode, and
lifecycle misuse errors.
"""
from __future__ import annotations

from pathlib import Path

import pytest

import tstrans.io as tio
from tstrans.mpegts import (
    DemuxEvent,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    StreamKindTag,
    VideoCodec,
)

# Three distinct minimal-but-valid H.264 Annex-B AUs (one IDR + two
# non-IDR) so byte-faithfulness is distinguishable per AU. Same shape as
# test_raw_first.py's acceptance fixture.
ORIG_AUS = [
    b"\x00\x00\x00\x01\x65\x88\x84\x00\x10\xaa\xbb",
    b"\x00\x00\x00\x01\x41\x9a\x00\x34\xcc",
    b"\x00\x00\x00\x01\x41\x9a\x01\x35\xdd\xee",
]
KEY_FRAMES = [True, False, False]
PTS0 = 900_000
PTS_STEP = 3_000


def _make_klv_bytes(lat: float) -> bytes:
    from tstrans.klv import ST_0601_UL, UasDatalinkLs, encode_uas_datalink

    rec = UasDatalinkLs(
        universal_label=ST_0601_UL,
        declared_version=19,
        timestamp_us=1_700_000_000_000_000,
        frame_center_lat_deg=lat,
        frame_center_lon_deg=-122.3321,
        sensor_lat_deg=47.6200,
        sensor_lon_deg=-122.3000,
        sensor_alt_m=500.0,
    )
    return encode_uas_datalink(rec)


def _write_video_klv_src(path: Path, lat: float = 47.6097) -> None:
    """Synthetic single-program H.264 + sync-KLV source."""
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    with mux.write_file(path) as proxy:
        for i, (au, key) in enumerate(zip(ORIG_AUS, KEY_FRAMES)):
            pts = Pts90khz.from_raw(PTS0 + i * PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=key)
            proxy.push_klv(_make_klv_bytes(lat), pts=pts)


def _collect(path: Path):
    """(video_aus, klv_payloads) from a TS file, in event order."""
    videos: list[bytes] = []
    klvs: list[bytes] = []
    for ev in tio.parse_file(path):
        if isinstance(ev, DemuxEvent.Video):
            videos.append(bytes(ev.raw))
        elif isinstance(ev, DemuxEvent.Metadata):
            klvs.append(bytes(ev.payload))
    return videos, klvs


def test_transmux_pass_through_copies_everything_byte_faithfully(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)

    with tio.transmux(src, dst) as tx:
        # Single-pass iterator contract (file-object semantics).
        assert iter(tx) is iter(tx)
        for ev in tx:
            tx.write(ev)

    src_videos, src_klvs = _collect(src)
    out_videos, out_klvs = _collect(dst)
    assert out_videos == ORIG_AUS == src_videos
    assert out_klvs == src_klvs
    assert len(out_klvs) == len(ORIG_AUS)


def test_transmux_dst_not_created_for_psi_less_source(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    src.write_bytes(b"")  # no PSI → no ProgramMap → lazy sink never opens
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)
    assert not dst.exists()


# ---------------------------------------------------------------------------
# Acceptance: the corrector workflow end-to-end.
# Patch frame-center tags on a synthetic fixture via klv.patch_uas_datalink,
# byte-compare every video AU out vs in, and verify the patched
# KLV differs ONLY at the edited TLVs + checksum.
# ---------------------------------------------------------------------------

def _walk_tlvs(ls: bytes) -> list[tuple[int, bytes]]:
    """Top-level (tag, full-TLV-bytes) pairs of a KLV local set:
    16-byte UL + BER outer length, then BER-OID tag / BER length / value
    triplets. Mirrors the wire layout patch_uas_datalink preserves."""

    def read_ber(buf: bytes, i: int) -> tuple[int, int]:
        b = buf[i]
        if b < 0x80:
            return b, i + 1
        n = b & 0x7F
        return int.from_bytes(buf[i + 1 : i + 1 + n], "big"), i + 1 + n

    def read_oid(buf: bytes, i: int) -> tuple[int, int]:
        val = 0
        while True:
            b = buf[i]
            i += 1
            val = (val << 7) | (b & 0x7F)
            if not (b & 0x80):
                return val, i

    body_len, i = read_ber(ls, 16)
    end = i + body_len
    out: list[tuple[int, bytes]] = []
    while i < end:
        start = i
        tag, i = read_oid(ls, i)
        vlen, i = read_ber(ls, i)
        i += vlen
        out.append((tag, ls[start:i]))
    return out


def test_transmux_acceptance_patch_corner_tags_video_byte_faithful(tmp_path):
    from tstrans.klv import decode_uas_datalink, patch_uas_datalink

    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src, lat=47.6097)

    EDITED_LAT, EDITED_LON = 37.7749, -122.4194
    # Tag numbers per MISB ST 0601: 23 = frame center lat, 24 = frame
    # center lon, 1 = checksum.
    EDITED_TAGS = {23, 24}

    src_klvs: list[bytes] = []
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            if isinstance(ev, DemuxEvent.Metadata):
                src_klvs.append(bytes(ev.payload))
                patched = patch_uas_datalink(
                    bytes(ev.payload),
                    {
                        "frame_center_lat_deg": EDITED_LAT,
                        "frame_center_lon_deg": EDITED_LON,
                    },
                )
                tx.write_klv(ev, patched)
            else:
                tx.write(ev)

    out_videos, out_klvs = _collect(dst)

    # 1) Every video AU byte-identical, count preserved.
    assert out_videos == ORIG_AUS

    # 2) Patched KLV differs ONLY at the edited TLVs + checksum.
    assert len(out_klvs) == len(src_klvs) == len(ORIG_AUS)
    for src_ls, out_ls in zip(src_klvs, out_klvs):
        assert out_ls[:16] == src_ls[:16]  # UL verbatim
        src_tlvs, out_tlvs = _walk_tlvs(src_ls), _walk_tlvs(out_ls)
        assert [t for t, _ in out_tlvs] == [t for t, _ in src_tlvs]
        differing = {
            t_out
            for (t_out, tlv_out), (_, tlv_src) in zip(out_tlvs, src_tlvs)
            if tlv_out != tlv_src
        }
        # Edited TLVs MUST differ; checksum may legitimately differ;
        # nothing else may.
        assert EDITED_TAGS <= differing <= EDITED_TAGS | {1}

        rec = decode_uas_datalink(out_ls)
        assert rec.frame_center_lat_deg == pytest.approx(EDITED_LAT, abs=1e-4)
        assert rec.frame_center_lon_deg == pytest.approx(EDITED_LON, abs=1e-4)
        # Untouched field survives bit-exact (byte-faithful patcher).
        assert rec.sensor_lat_deg == pytest.approx(47.6200, abs=1e-4)


# ---------------------------------------------------------------------------
# v1 scope + error semantics.
# ---------------------------------------------------------------------------

def _write_two_program_src(path: Path) -> None:
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .build()
        )
        .add_program(
            MuxerProgramConfigBuilder(2, 0x200)
            .add_video(0x201, VideoCodec.H264)
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    handles = mux.video_handles()
    with mux.write_file(path) as proxy:
        for i, au in enumerate(ORIG_AUS):
            pts = Pts90khz.from_raw(PTS0 + i * PTS_STEP)
            for h in handles:
                proxy.push_video_to(h, au, pts=pts, key_frame=KEY_FRAMES[i])


def test_transmux_rejects_second_program(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_two_program_src(src)
    with pytest.raises(ValueError, match="single-program"):
        with tio.transmux(src, dst) as tx:
            for ev in tx:
                tx.write(ev)


def test_transmux_tolerates_identical_pm_reemission(tmp_path):
    # A mid-file PMT version bump with unchanged content re-emits an
    # identical ProgramMap. The public Muxer emits a fixed PSI version,
    # so that sequence cannot be built from a file — drive the private
    # handler directly with a hand-built duplicate event.
    import dataclasses

    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)
            if isinstance(ev, DemuxEvent.ProgramMap):
                first_pm = ev.programs[0]
                # Distinct-but-equal clone: a broken identity-based
                # comparison (`is not`) would reject it, value equality
                # tolerates it — real re-emissions are always fresh
                # objects from the demuxer.
                duplicate = dataclasses.replace(first_pm)
                tx._on_program_map(DemuxEvent.ProgramMap(programs=(duplicate,)))
                changed = dataclasses.replace(first_pm, program_number=7)
                with pytest.raises(ValueError, match="different"):
                    tx._on_program_map(DemuxEvent.ProgramMap(programs=(changed,)))
    out_videos, _ = _collect(dst)
    assert out_videos == ORIG_AUS


# Private-data pass-through: two data streams — one user-private
# 0xF0 with a 0xFF name descriptor (carries_pts=True), one bare
# ISO/IEC 13818-1 private-PES 0x06 (carries_pts=False). Distinct
# payloads per stream so byte-faithfulness is distinguishable per
# sample.
DATA_DESCRIPTOR = b"\xff\x0aSERIAL_ADF"
DATA_PAYLOADS_A = [b"\x01\x02record-a0", b"\x03\x04record-a1", b"\x05\x06record-a2"]
DATA_PAYLOADS_B = [b"\xaa\xbb-b0", b"\xcc\xdd-b1", b"\xee\xff-b2"]


def _write_video_data_src(path: Path) -> None:
    """Synthetic single-program source: H.264 video + the two private
    data streams above, built via the muxer data surface."""
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_data(0x1F0, 0xF0, carries_pts=True)
            .add_data(0x1F1, 0x06, carries_pts=False)
            .stream_descriptors_for_data(0, [DATA_DESCRIPTOR])
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    handle_a, handle_b = mux.data_handles()
    with mux.write_file(path) as proxy:
        for i, (au, key) in enumerate(zip(ORIG_AUS, KEY_FRAMES)):
            pts = Pts90khz.from_raw(PTS0 + i * PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=key)
            proxy.push_data_to(handle_a, DATA_PAYLOADS_A[i], pts=pts)
            proxy.push_data_to(handle_b, DATA_PAYLOADS_B[i], pts=pts)


def test_transmux_passes_unknown_streams_through_byte_faithfully(tmp_path):
    # Pass-through contract: private/application data streams survive a
    # transmux
    # byte-faithfully — PMT identity (raw stream_type + descriptor
    # bytes) via from_program_map, sample payloads via push_data_to.
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_data_src(src)

    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)

    # PMT identity: raw stream_type bytes + verbatim descriptor loop.
    out_pm = tio.probe(dst).programs[0]
    by_pid = {s.pid: s for s in out_pm.streams}
    assert by_pid[0x1F0].kind is StreamKindTag.UNKNOWN
    assert by_pid[0x1F0].stream_type == 0xF0
    assert [(d.tag, d.data) for d in by_pid[0x1F0].raw_descriptors] == [
        (0xFF, b"SERIAL_ADF")
    ]
    assert by_pid[0x1F1].kind is StreamKindTag.UNKNOWN
    assert by_pid[0x1F1].stream_type == 0x06
    assert by_pid[0x1F1].raw_descriptors == ()

    # Samples: every payload byte-identical, in order; video untouched.
    out_videos: list[bytes] = []
    out_data: dict[int, list[tuple[int, bytes]]] = {0x1F0: [], 0x1F1: []}
    for ev in tio.parse_file(dst):
        if isinstance(ev, DemuxEvent.Video):
            out_videos.append(bytes(ev.raw))
        elif isinstance(ev, DemuxEvent.UnknownSample):
            out_data[ev.stream.pid].append((ev.pts.raw, bytes(ev.payload)))
    assert out_videos == ORIG_AUS
    # carries_pts stream: payloads in push order WITH the source pts.
    assert out_data[0x1F0] == [
        (PTS0 + i * PTS_STEP, p) for i, p in enumerate(DATA_PAYLOADS_A)
    ]
    # The 0x06 stream was carries_pts=False at the source: its PES
    # carried no PTS, so the demuxer substituted pts=0 on the way in
    # and transmux re-pushed that 0 (re-muxed data streams always
    # carry PTS). The pts values pushed at the source are therefore
    # NOT recoverable — the samples re-emerge with a literal PTS of 0.
    assert out_data[0x1F1] == [(0, p) for p in DATA_PAYLOADS_B]


def test_transmux_mixed_klv_and_data_streams_survive(tmp_path):
    # Mixed-kind pass-through: a single program carrying video +
    # sync-KLV + a private data stream exercises the mixed-kind
    # _build_handles zip (KLV and data handles built from the same
    # ProgramMap must not cross-wire). Both kinds must survive a
    # no-drop transmux with payload AND pts intact.
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
            .add_data(0x1F0, 0xF0, carries_pts=True)
            .build()
        )
        .build()
    )
    mux = Muxer(cfg)
    (handle,) = mux.data_handles()
    klv_bytes = _make_klv_bytes(47.6097)
    with mux.write_file(src) as proxy:
        for i, (au, key) in enumerate(zip(ORIG_AUS, KEY_FRAMES)):
            pts = Pts90khz.from_raw(PTS0 + i * PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=key)
            proxy.push_klv(klv_bytes, pts=pts)
            proxy.push_data_to(handle, DATA_PAYLOADS_A[i], pts=pts)

    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)

    out_videos: list[bytes] = []
    out_klvs: list[tuple[int, bytes]] = []
    out_data: list[tuple[int, bytes]] = []
    for ev in tio.parse_file(dst):
        if isinstance(ev, DemuxEvent.Video):
            out_videos.append(bytes(ev.raw))
        elif isinstance(ev, DemuxEvent.Metadata):
            out_klvs.append((ev.pts.raw, bytes(ev.payload)))
        elif isinstance(ev, DemuxEvent.UnknownSample):
            out_data.append((ev.pts.raw, bytes(ev.payload)))
    assert out_videos == ORIG_AUS
    expected_pts = [PTS0 + i * PTS_STEP for i in range(len(ORIG_AUS))]
    assert out_klvs == [(p, klv_bytes) for p in expected_pts]
    assert out_data == list(zip(expected_pts, DATA_PAYLOADS_A))


def test_transmux_drop_unknown_still_sheds(tmp_path):
    # drop=[UNKNOWN] remains the escape hatch now that pass-through is
    # the default: the output PMT declares no unknown streams and the
    # source's UnknownSample events are skipped silently by write()
    # (dropped-stream no-handle path — no exception).
    from _builders.unknown_stream import build_unknown_stream_ts

    idr_au = ORIG_AUS[0]
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    src.write_bytes(
        build_unknown_stream_ts(
            stream_type=0x7F, payload=b"private-bytes", video_au=idr_au
        )
    )
    with tio.transmux(src, dst, drop=(StreamKindTag.UNKNOWN,)) as tx:
        for ev in tx:
            tx.write(ev)  # UnknownSample → no handle (dropped) → skipped
    out_videos, _ = _collect(dst)
    assert out_videos == [idr_au]
    # The unknown stream is shed: the output PMT declares no UNKNOWN kind.
    out_pm = tio.probe(dst).programs[0]
    assert all(s.kind is not StreamKindTag.UNKNOWN for s in out_pm.streams)


def test_transmux_drop_klv_skips_dropped_stream_events(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with tio.transmux(src, dst, drop=(StreamKindTag.KLV_SYNC,)) as tx:
        for ev in tx:
            tx.write(ev)  # KLV events hit the no-handle path → skipped
            if isinstance(ev, DemuxEvent.Metadata):
                # Substituted writes for dropped streams are no-ops too.
                tx.write_klv(ev, bytes(ev.payload))
    out_videos, out_klvs = _collect(dst)
    assert out_videos == ORIG_AUS
    assert out_klvs == []
    assert not tio.probe(dst).has_klv


def test_transmux_write_before_program_map_raises(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    src_video = next(
        ev for ev in tio.parse_file(src) if isinstance(ev, DemuxEvent.Video)
    )
    with tio.transmux(src, dst) as tx:
        with pytest.raises(RuntimeError, match="no ProgramMap"):
            tx.write(src_video)


def test_transmux_write_after_close_raises(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    events = []
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)
            events.append(ev)
    video = next(ev for ev in events if isinstance(ev, DemuxEvent.Video))
    with pytest.raises(RuntimeError, match="closed"):
        tx.write(video)


def test_transmux_write_rejects_non_events(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)
        with pytest.raises(TypeError, match="unsupported event type"):
            tx.write(b"not an event")
        with pytest.raises(TypeError, match="expected a DemuxEvent.Metadata"):
            tx.write_klv(b"not an event", b"payload")


# ---------------------------------------------------------------------------
# atomic= passes through the MuxerFileSink temp-file + os.replace machinery.
# ---------------------------------------------------------------------------

def test_transmux_atomic_success_promotes_dst(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with tio.transmux(src, dst, atomic=True) as tx:
        for ev in tx:
            tx.write(ev)
            # While inside the block, output lives in a *.partial temp.
            assert not dst.exists()
    out_videos, _ = _collect(dst)
    assert out_videos == ORIG_AUS
    assert not list(tmp_path.glob("*.partial"))


def test_transmux_atomic_failure_leaves_nothing(tmp_path):
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with pytest.raises(RuntimeError, match="boom"):
        with tio.transmux(src, dst, atomic=True) as tx:
            for ev in tx:
                tx.write(ev)
                if isinstance(ev, DemuxEvent.Video):
                    raise RuntimeError("boom")
    assert not dst.exists()
    assert not list(tmp_path.glob("*.partial"))


def test_transmux_iter_and_reenter_after_close_raise(tmp_path):
    # A never-iterated-but-closed transmuxer must NOT start demuxing on
    # a later iter() — the first ProgramMap would open a sink that no
    # __exit__ will ever clean up (stray *.partial / clobbered dst).
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    _write_video_klv_src(src)
    with tio.transmux(src, dst) as tx:
        pass  # never iterated
    with pytest.raises(RuntimeError, match="closed"):
        iter(tx)
    with pytest.raises(RuntimeError, match="closed"):
        with tx:
            pass
    assert not dst.exists()
    assert not list(tmp_path.glob("*.partial"))


def test_transmux_audio_copied_byte_faithfully(tmp_path):
    from tstrans.mpegts import AudioCodec

    # Smallest valid parseable MPEG-2 AAC-LC 44100 Hz stereo ADTS frame
    # (header only) — same literal as test_codec_aac.py /
    # _builders/build_audio_aac_large.py.
    adts = bytes.fromhex("fff95080021ffc000000000000000000")
    audio_pes = [adts, adts * 2, adts * 3]  # distinct per-PES frame runs

    cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_audio(0x103, AudioCodec.AAC)
            .build()
        )
        .build()
    )
    src, dst = tmp_path / "src.ts", tmp_path / "out.ts"
    mux = Muxer(cfg)
    with mux.write_file(src) as proxy:
        for i, (au, frames) in enumerate(zip(ORIG_AUS, audio_pes)):
            pts = Pts90khz.from_raw(PTS0 + i * PTS_STEP)
            proxy.push_video(au, pts=pts, key_frame=KEY_FRAMES[i])
            proxy.push_audio(frames, pts=pts)

    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)

    out_videos: list[bytes] = []
    out_audio: list[bytes] = []
    for ev in tio.parse_file(dst):
        if isinstance(ev, DemuxEvent.Video):
            out_videos.append(bytes(ev.raw))
        elif isinstance(ev, DemuxEvent.Audio):
            out_audio.append(bytes(ev.raw))
    assert out_videos == ORIG_AUS
    assert out_audio == audio_pes


# ---------------------------------------------------------------------------
# AV1 transmux acceptance tests
# ---------------------------------------------------------------------------
#
# Reuse the AV1 fixture builders from test_demux_config_parity.py — pytest
# puts tests/ on sys.path, so they import directly. Keeping the carriage-
# sensitive OBU fixture in one place avoids two-place updates.
from test_demux_config_parity import _build_av1_ts  # noqa: E402


def _collect_av1(path: Path, demux_mode):
    """Demux `path` under `demux_mode`; return (raw_bytes, av1_carriage) per Video."""
    from tstrans.mpegts import DemuxerConfig
    cfg = DemuxerConfig(av1_carriage=demux_mode)
    videos = []
    for ev in tio.parse_file(path, config=cfg):
        if isinstance(ev, DemuxEvent.Video):
            videos.append((bytes(ev.raw), ev.av1_carriage))
    return videos


def test_av1_binding_transmux_fixpoint(tmp_path: Path) -> None:
    """Binding-mode AV1 transmux: output AU must be non-empty and
    byte-identical to the source AU."""
    from tstrans.mpegts import Av1CarriageMode
    src = tmp_path / "av1_binding_src.ts"
    src.write_bytes(_build_av1_ts(Av1CarriageMode.MPEG2_TS_BINDING))
    dst = tmp_path / "av1_binding_dst.ts"

    # Transmux — no config kwarg needed for binding (default is MPEG2_TS_BINDING).
    with tio.transmux(src, dst) as tx:
        for ev in tx:
            tx.write(ev)

    src_videos = _collect_av1(src, Av1CarriageMode.MPEG2_TS_BINDING)
    out_videos = _collect_av1(dst, Av1CarriageMode.MPEG2_TS_BINDING)
    assert len(out_videos) >= 1, "at least one AV1 Video event must survive transmux"
    src_raw = src_videos[0][0]
    out_raw, out_carriage = out_videos[0]
    assert len(out_raw) > 0, "transmux must NOT produce an empty AV1 AU (AV1-01)"
    assert out_raw == src_raw, "output AU must be byte-identical to source"
    assert out_carriage is Av1CarriageMode.MPEG2_TS_BINDING


def test_av1_interop_transmux_fixpoint(tmp_path: Path) -> None:
    """Interop-mode AV1 transmux: output AU must be non-empty and
    byte-identical to the source AU (interop mode)."""
    from tstrans.mpegts import Av1CarriageMode, DemuxerConfig
    src = tmp_path / "av1_interop_src.ts"
    src.write_bytes(_build_av1_ts(Av1CarriageMode.INTEROP_RAW_OBU))
    dst = tmp_path / "av1_interop_dst.ts"

    interop_cfg = DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)
    with tio.transmux(src, dst, config=interop_cfg) as tx:
        for ev in tx:
            tx.write(ev)

    src_videos = _collect_av1(src, Av1CarriageMode.INTEROP_RAW_OBU)
    out_videos = _collect_av1(dst, Av1CarriageMode.INTEROP_RAW_OBU)
    assert len(out_videos) >= 1, "at least one AV1 Video event must survive interop transmux"
    src_raw = src_videos[0][0]
    out_raw, out_carriage = out_videos[0]
    assert len(out_raw) > 0, "interop transmux must NOT produce an empty AV1 AU (AV1-01)"
    assert out_raw == src_raw, "interop AU must be byte-identical to source"
    assert out_carriage is Av1CarriageMode.INTEROP_RAW_OBU


def test_av1_carriage_provenance_on_binding_event(tmp_path: Path) -> None:
    """AV1 binding-mode demux event carries MPEG2_TS_BINDING provenance (Part A)."""
    from tstrans.mpegts import Av1CarriageMode
    ts_path = tmp_path / "binding.ts"
    ts_path.write_bytes(_build_av1_ts(Av1CarriageMode.MPEG2_TS_BINDING))
    videos = _collect_av1(ts_path, Av1CarriageMode.MPEG2_TS_BINDING)
    assert len(videos) >= 1
    _, carriage = videos[0]
    assert carriage is Av1CarriageMode.MPEG2_TS_BINDING


def test_h264_event_av1_carriage_is_none(tmp_path: Path) -> None:
    """H.264 Video events must have av1_carriage == None (Part A provenance)."""
    src = tmp_path / "h264.ts"
    _write_video_klv_src(src)
    for ev in tio.parse_file(src):
        if isinstance(ev, DemuxEvent.Video):
            assert ev.av1_carriage is None, (
                f"H.264 event must have av1_carriage=None, got {ev.av1_carriage!r}"
            )
            break
    else:
        pytest.fail("no Video event found in H.264 source")


def test_av1_parse_binding_mode_no_spurious_issues(tmp_path: Path) -> None:
    """Binding-mode ev.parse(strict=True) succeeds without
    raising (the demuxer carriage is forwarded to split_units automatically)."""
    from tstrans.mpegts import Av1CarriageMode, DemuxerConfig
    ts_path = tmp_path / "binding.ts"
    ts_path.write_bytes(_build_av1_ts(Av1CarriageMode.MPEG2_TS_BINDING))
    cfg = DemuxerConfig(av1_carriage=Av1CarriageMode.MPEG2_TS_BINDING)
    for ev in tio.parse_file(ts_path, config=cfg):
        if isinstance(ev, DemuxEvent.Video):
            # strict=True must not raise for a conformant binding-mode AU.
            units = ev.parse(strict=True)
            assert len(units) >= 1, "parse must recover at least one OBU"
            break
    else:
        pytest.fail("no Video event found in binding-mode AV1 source")


def test_av1_parse_interop_mode_no_spurious_issues(tmp_path: Path) -> None:
    """Interop-mode ev.parse(strict=True) succeeds without
    raising (carriage is threaded so raw-OBU AU parses cleanly)."""
    from tstrans.mpegts import Av1CarriageMode, DemuxerConfig
    ts_path = tmp_path / "interop.ts"
    ts_path.write_bytes(_build_av1_ts(Av1CarriageMode.INTEROP_RAW_OBU))
    cfg = DemuxerConfig(av1_carriage=Av1CarriageMode.INTEROP_RAW_OBU)
    for ev in tio.parse_file(ts_path, config=cfg):
        if isinstance(ev, DemuxEvent.Video):
            units = ev.parse(strict=True)
            assert len(units) >= 1, "interop parse must recover at least one OBU"
            break
    else:
        pytest.fail("no Video event found in interop-mode AV1 source")
