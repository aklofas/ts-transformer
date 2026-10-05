"""Tests for codec.split_units and codec.parse_audio opt-in parsers
plus the DemuxEvent.Video/.Audio raw-first surface
plus an end-to-end transmux acceptance test."""

import dataclasses
import tempfile
from pathlib import Path

import pytest

import tstrans.codec as codec
from tstrans import _native
from tstrans import io as tio
from tstrans.exceptions import CodecError
from tstrans.mpegts import (
    AudioCodec,
    DemuxEvent,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    StreamId,
    StreamKindTag,
    SubtitleCodec,
    VideoCodec,
)


def test_split_units_h264_returns_nal_list():
    # Two H.264 NALs: SPS (nal_ref_idc=3, type=7) then IDR (type=5), 4-byte start codes.
    # NAL header byte 0x67 = (ref_idc=3 << 5) | (type=7); payload bytes: 0xAA, 0xBB
    # NAL header byte 0x65 = (ref_idc=3 << 5) | (type=5); payload bytes: 0xCC
    au = b"\x00\x00\x00\x01\x67\xAA\xBB\x00\x00\x00\x01\x65\xCC"
    units, issues = codec.split_units(au, VideoCodec.H264)
    assert len(units) == 2
    assert issues == []
    assert bytes(units[0].payload) == b"\xAA\xBB"


def test_split_units_strict_raises_on_bad_header():
    # forbidden_zero_bit set (0x80) → a NAL-header issue.
    au = b"\x00\x00\x00\x01\x80\x00"
    with pytest.raises(ValueError):
        codec.split_units(au, VideoCodec.H264, strict=True)


def test_split_units_lenient_does_not_raise_on_bad_header():
    # Lenient mode: split_units returns (units, issues) — the 0x80 forbidden-bit
    # input provably yields a conformance issue rather than raising.
    au = b"\x00\x00\x00\x01\x80\x00"
    units, issues = codec.split_units(au, VideoCodec.H264, strict=False)
    assert isinstance(units, list)
    assert isinstance(issues, list)
    assert len(issues) > 0


def test_parse_audio_aac_empty_returns_empty():
    frames = codec.parse_audio(b"", AudioCodec.AAC)
    assert frames == []


def test_parse_audio_mp2_empty_returns_empty():
    frames = codec.parse_audio(b"", AudioCodec.MP2)
    assert frames == []


def test_parse_audio_unknown_codec_returns_empty():
    # AAC_LATM has no typed parser — returns empty list.
    frames = codec.parse_audio(b"\xff\xff\xff", AudioCodec.AAC_LATM)
    assert frames == []


def test_parse_audio_aac_strict_raises_on_malformed():
    # An ADTS syncword (0xFFF1) followed by a truncated header raises under
    # strict mode (CodecError, the codec-domain exception — not ValueError).
    with pytest.raises(CodecError):
        codec.parse_audio(b"\xff\xf1\xff", AudioCodec.AAC, strict=True)


# ---------------------------------------------------------------------------
# DemuxEvent.Video/.Audio raw-first surface
# ---------------------------------------------------------------------------

# Real audio fixtures live under the tst-core fixtures tree (same accessor
# pattern as tests/test_sample_payload_typed.py).
_FIXTURE_BASE = (
    Path(__file__).parent.parent.parent.parent
    / "crates" / "tst-core" / "tests" / "fixtures"
)
_MP2_FIXTURE = _FIXTURE_BASE / "audio" / "mp2.ts"


def _sample_ts_path(tmp: Path) -> Path:
    """Build a small H.264 TS via the Muxer (mirrors the synthetic-TS
    accessor used by tests/test_sample_payload_typed.py::_make_h264_ts)."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    m = Muxer(cfg)
    path = tmp / "h264.ts"
    nal_aud = b"\x00\x00\x00\x01\x09\xF0"  # Annex-B AUD NAL
    pts0 = 900_000
    with m.write_file(path) as proxy:
        for i in range(4):
            proxy.push_video(nal_aud, pts=Pts90khz.from_raw(pts0 + i * 3000))
    return path


def test_video_event_exposes_raw_and_opt_in_parse():
    saw = False
    with tempfile.TemporaryDirectory() as tmp:
        for ev in tio.parse_file(_sample_ts_path(Path(tmp))):
            if isinstance(ev, DemuxEvent.Video):
                assert isinstance(ev.raw, (bytes, bytearray))
                assert ev.raw[:4] == b"\x00\x00\x00\x01"  # Annex-B start code
                units = ev.parse()                          # opt-in
                assert len(units) >= 1
                assert not hasattr(ev, "payload")           # removed
                saw = True
                break
    assert saw


def test_audio_event_exposes_raw():
    for ev in tio.parse_file(_MP2_FIXTURE):
        if isinstance(ev, DemuxEvent.Audio):
            assert isinstance(ev.raw, (bytes, bytearray))
            assert isinstance(ev.parse(), list)
            break


# ---------------------------------------------------------------------------
# Lazy native `.raw` materialization for Video / Audio
#
# The demuxer does not eagerly copy each media payload into a PyBytes; the
# `.raw` Python `bytes` is materialized on first access (pay-per-access) and
# cached, while value-equality + hashability are preserved over the content.
# ---------------------------------------------------------------------------

def test_demux_does_not_materialize_raw_until_first_access():
    """Events collected from the demuxer carry an unmaterialized `_raw`
    holder; the PyBytes copy only happens on the first `.raw` access, and
    repeated access returns the identical cached object."""
    saw_video = False
    saw_audio = False

    with tempfile.TemporaryDirectory() as tmp:
        events = list(tio.parse_file(_sample_ts_path(Path(tmp))))

    for ev in events:
        if isinstance(ev, DemuxEvent.Video):
            # Demuxed but `.raw` untouched → no PyBytes materialized yet.
            assert ev._raw._materialized is False
            first = ev.raw
            assert isinstance(first, (bytes, bytearray))
            assert ev._raw._materialized is True
            # Repeated access returns the SAME cached object (identity).
            assert ev.raw is first
            saw_video = True
            break

    for ev in tio.parse_file(_MP2_FIXTURE):
        if isinstance(ev, DemuxEvent.Audio):
            assert ev._raw._materialized is False
            first = ev.raw
            assert isinstance(first, (bytes, bytearray))
            assert ev._raw._materialized is True
            assert ev.raw is first
            saw_audio = True
            break

    assert saw_video
    assert saw_audio


def test_video_event_value_equality_and_hash_over_raw_content():
    """Two Video events built with equal `raw` compare equal and hash equal;
    a Video built from a `bytes` and one built from the holder are equal."""
    stream = StreamId(
        pid=256, kind=StreamKindTag.VIDEO, codec=VideoCodec.H264, program_number=1
    )
    pts = Pts90khz.from_ms(100)
    raw = b"\x00\x00\x00\x01\x65payload"

    def _mk(raw_arg):
        return DemuxEvent.Video(
            stream=stream,
            pts=pts,
            dts=None,
            codec=VideoCodec.H264,
            raw=raw_arg,
            random_access_indicator=True,
        )

    a = _mk(raw)
    b = _mk(bytes(raw))  # distinct bytes object, same content
    assert a == b
    assert hash(a) == hash(b)

    # An event built from a holder equals one built from the same bytes.
    holder = _native.RawBytes(raw)
    c = _mk(holder)
    assert a == c
    assert hash(a) == hash(c)
    assert isinstance(c.raw, (bytes, bytearray))
    assert bytes(c.raw) == raw

    # Different raw content → not equal.
    d = _mk(b"\x00\x00\x00\x01\x65different")
    assert a != d


def test_audio_event_value_equality_and_hash_over_raw_content():
    """Audio events with equal `raw` compare equal and hash equal."""
    stream = StreamId(
        pid=258, kind=StreamKindTag.AUDIO, codec=AudioCodec.AAC, program_number=1
    )
    pts = Pts90khz.from_ms(100)
    raw = b"adts frame bytes"

    def _mk(raw_arg):
        return DemuxEvent.Audio(
            stream=stream, pts=pts, dts=None, codec=AudioCodec.AAC, raw=raw_arg
        )

    a = _mk(raw)
    b = _mk(bytes(raw))
    assert a == b
    assert hash(a) == hash(b)
    assert a != _mk(b"other frame bytes")


# ---------------------------------------------------------------------------
# push_video_* accept dts=None (PTS-only PES, == push_video_to)
# ---------------------------------------------------------------------------

def _drain(mux) -> bytes:
    """Drain all queued TS packets from a Muxer into a single bytes blob."""
    out = bytearray()
    while True:
        buf = bytearray(1316)
        n = mux.pull(buf)
        if n == 0:
            break
        out += bytes(buf[:n])
    return bytes(out)


def _single_video_mux():
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .pcr_pid(0x101)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    return Muxer(cfg)


def test_push_video_accepts_dts_none():
    """Passing dts=None to push_video_to_with_dts produces a PTS-only PES.
    The muxer must accept it without error and produce TS output."""
    mux = _single_video_mux()
    vh = mux.video_stream_handle(0)
    # Valid H.264 Annex-B IDR NAL (IDR slice, nal_unit_type=5).
    au = b"\x00\x00\x00\x01\x65\x88\x84\x00\x10"
    pts = Pts90khz.from_raw(9000)
    mux.push_video_to_with_dts(vh, au, pts=pts, dts=None, key_frame=True)
    buf = bytearray(1316)
    assert mux.pull(buf) > 0


def test_push_video_dts_none_equals_push_video_to():
    """dts=None routes to the PTS-only path: byte-identical to push_video_to
    for the same AU + pts (pins the 5-byte PtsOnly PES, not a 10-byte PtsAndDts
    header with dts==pts)."""
    au = b"\x00\x00\x00\x01\x65\x88\x84\x00\x10"
    pts = Pts90khz.from_raw(9000)

    mux_a = _single_video_mux()
    mux_a.push_video_to(mux_a.video_stream_handle(0), au, pts=pts, key_frame=True)
    ref = _drain(mux_a)

    mux_b = _single_video_mux()
    mux_b.push_video_to_with_dts(
        mux_b.video_stream_handle(0), au, pts=pts, dts=None, key_frame=True
    )
    got = _drain(mux_b)

    assert len(ref) > 0
    assert got == ref


# ---------------------------------------------------------------------------
# End-to-end transmux acceptance test
#
# Motivating workflow: demux SRC → for each KLV event, edit one metadata
# field and re-emit; for each video event, forward the raw AU bytes
# verbatim → re-mux to OUT.
#
# Assertions:
#   1. Video byte-faithful: every OUT video AU equals the corresponding
#      SRC video AU byte-for-byte.
#   2. KLV edit present: the edited field has the new value in OUT.
#   3. Unedited KLV field preserved: proves selective edit, not clobber.
# ---------------------------------------------------------------------------

def test_transmux_edit_klv_copy_video_byte_faithful():
    """Demux a synthetic H.264+KLV TS, edit one ST 0601 field (frame_center_lat_deg),
    forward all video AUs verbatim via the raw-first push API, re-mux to OUT,
    then assert video is byte-faithful and the KLV edit is present."""
    from tstrans.mpegts import (
        KlvStreamType,
        Muxer,
        MuxerConfigBuilder,
        MuxerProgramConfigBuilder,
        Pts90khz,
        VideoCodec,
        DemuxEvent,
        Demuxer,
    )
    from tstrans.klv import (
        ST_0601_UL,
        UasDatalinkLs,
        decode_uas_datalink,
        encode_uas_datalink,
    )

    # ------------------------------------------------------------------
    # Step 1: build synthetic SRC TS with H.264 video + ST 0601 KLV.
    # Each video AU is a distinct 4-byte-start-code NAL so we can tell
    # them apart byte-for-byte after the round-trip.
    # ------------------------------------------------------------------

    # Three distinct H.264 Annex-B AUs: one IDR (key frame) + two P-frames.
    # These are minimal but structurally valid NALs for the muxer.
    ORIG_AUS = [
        # IDR slice (nal_unit_type=5); key frame
        b"\x00\x00\x00\x01\x65\x88\x84\x00\x10\xAA\xBB",
        # Non-IDR slice (nal_unit_type=1)
        b"\x00\x00\x00\x01\x41\x9A\x00\x34\xCC",
        # Another non-IDR slice with different payload
        b"\x00\x00\x00\x01\x41\x9A\x01\x35\xDD\xEE",
    ]
    KEY_FRAMES = [True, False, False]

    # ST 0601 record to embed in SRC.  We use two populated fields so we
    # can confirm one is edited (frame_center_lat_deg) and one is
    # preserved (sensor_lat_deg) after the transmux.
    ORIG_LAT = 47.6097          # Seattle-ish
    ORIG_SENSOR_LAT = 47.6200   # sensor position, stays unchanged

    def _make_klv_bytes(lat: float) -> bytes:
        rec = UasDatalinkLs(
            universal_label=ST_0601_UL,
            declared_version=19,
            timestamp_us=1_700_000_000_000_000,
            frame_center_lat_deg=lat,
            frame_center_lon_deg=-122.3321,
            sensor_lat_deg=ORIG_SENSOR_LAT,
            sensor_lon_deg=-122.3000,
            sensor_alt_m=500.0,
        )
        return encode_uas_datalink(rec)

    src_cfg = (
        MuxerConfigBuilder()
        .add_program(
            MuxerProgramConfigBuilder(1, 0x100)
            .add_video(0x101, VideoCodec.H264)
            .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
            .build()
        )
        .build()
    )

    pts0 = 900_000  # ~10 s at 90 kHz
    pts_step = 3_000  # ~33 ms per frame

    with tempfile.TemporaryDirectory() as tmp:
        src_path = Path(tmp) / "src.ts"
        out_path = Path(tmp) / "out.ts"

        # Write SRC: interleave video + KLV at matching timestamps.
        src_mux = Muxer(src_cfg)
        with src_mux.write_file(src_path) as proxy:
            for i, (au, key) in enumerate(zip(ORIG_AUS, KEY_FRAMES)):
                pts = Pts90khz.from_raw(pts0 + i * pts_step)
                proxy.push_video(au, pts=pts, key_frame=key)
                # One KLV per video frame; same PTS.
                proxy.push_klv(_make_klv_bytes(ORIG_LAT), pts=pts)

        # ------------------------------------------------------------------
        # Step 2: demux SRC → transmux to OUT.
        # Video: forwarded raw via push_video_to_with_dts (dts=None → PTS-only).
        # KLV: decode ST 0601, edit frame_center_lat_deg, re-encode, push.
        # ------------------------------------------------------------------
        EDITED_LAT = 37.7749  # San Francisco — clearly different from Seattle

        out_cfg = (
            MuxerConfigBuilder()
            .add_program(
                MuxerProgramConfigBuilder(1, 0x100)
                .add_video(0x101, VideoCodec.H264)
                .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
                .build()
            )
            .build()
        )
        out_mux = Muxer(out_cfg)
        vh = out_mux.video_stream_handle(0)
        kh = out_mux.klv_stream_handle(0)

        with out_mux.write_file(out_path) as proxy:
            for ev in tio.parse_file(src_path):
                if isinstance(ev, DemuxEvent.Video):
                    # Raw-first: forward the exact bytes received; no re-encode.
                    proxy.push_video_to_with_dts(
                        vh,
                        ev.raw,
                        pts=ev.pts,
                        dts=ev.dts,
                        key_frame=ev.random_access_indicator,
                    )
                elif isinstance(ev, DemuxEvent.Metadata):
                    # Decode, patch one field, re-encode.
                    original = decode_uas_datalink(bytes(ev.payload))
                    edited = dataclasses.replace(
                        original, frame_center_lat_deg=EDITED_LAT
                    )
                    proxy.push_klv_to(kh, encode_uas_datalink(edited), pts=ev.pts)

        # ------------------------------------------------------------------
        # Step 3: demux OUT and assert correctness.
        # ------------------------------------------------------------------
        out_video_aus: list[bytes] = []
        out_klv_recs: list[UasDatalinkLs] = []

        for ev in tio.parse_file(out_path):
            if isinstance(ev, DemuxEvent.Video):
                out_video_aus.append(bytes(ev.raw))
            elif isinstance(ev, DemuxEvent.Metadata):
                out_klv_recs.append(decode_uas_datalink(bytes(ev.payload)))

        # Structural sanity: we must have recovered events for both streams.
        assert len(out_video_aus) >= 1, (
            f"no video events recovered from OUT TS; got {len(out_video_aus)}"
        )
        assert len(out_klv_recs) >= 1, (
            f"no KLV events recovered from OUT TS; got {len(out_klv_recs)}"
        )

        # ---- Assertion 1: video AUs byte-faithful (per-AU) -----------
        # The round-trip preserves AU boundaries 1:1, so compare each AU
        # individually — stronger than a concatenation check, which would
        # mask an AU split/merge that kept the total byte count the same.
        assert len(out_video_aus) == len(ORIG_AUS), (
            f"AU count mismatch: SRC={len(ORIG_AUS)}, OUT={len(out_video_aus)}"
        )
        for i, (out_au, src_au) in enumerate(zip(out_video_aus, ORIG_AUS)):
            assert out_au == src_au, f"AU {i} mismatch: SRC={src_au!r}, OUT={out_au!r}"

        # ---- Assertion 2: KLV edit present ---------------------------
        # Every decoded KLV record in OUT must carry the edited latitude.
        for rec in out_klv_recs:
            assert rec.frame_center_lat_deg is not None, (
                "frame_center_lat_deg is None in OUT KLV — field was dropped"
            )
            assert abs(rec.frame_center_lat_deg - EDITED_LAT) < 0.01, (
                f"edited lat in OUT ({rec.frame_center_lat_deg}) != EDITED_LAT "
                f"({EDITED_LAT}) — KLV edit was not written"
            )

        # ---- Assertion 3: unedited KLV field preserved ---------------
        # sensor_lat_deg was NOT touched by the transmux; it must survive.
        for rec in out_klv_recs:
            assert rec.sensor_lat_deg is not None, (
                "sensor_lat_deg is None in OUT KLV — unedited field was dropped"
            )
            assert abs(rec.sensor_lat_deg - ORIG_SENSOR_LAT) < 0.01, (
                f"sensor_lat_deg in OUT ({rec.sensor_lat_deg}) != ORIG "
                f"({ORIG_SENSOR_LAT}) — unedited field was corrupted"
            )


# ---------------------------------------------------------------------------
# Lazy RawBytes for Subtitle / UnknownSample demux events
#
# `SamplePayload::Subtitle.payload` and `SamplePayload::Unknown.raw` are
# `SharedBytes` in tst-core, so the Rust side passes a cheap Arc clone to a
# `RawBytes` holder. The Python `bytes` is materialized only on first
# `.payload` access. This mirrors the Video / Audio lazy pattern.
#
# Metadata is NOT lazy: `DemuxEvent::Metadata.payload` is `Vec<u8>` in
# tst-core and `convert_event` takes `&DemuxEvent`, so no SharedBytes source
# is available without a copy — an eager PyBytes is still the right call there.
# ---------------------------------------------------------------------------


def _make_stream_id_subtitle():
    return StreamId(pid=0x102, kind=StreamKindTag.SUBTITLE,
                    codec=SubtitleCodec.CEA708_STANDALONE, program_number=1)


def _make_stream_id_unknown():
    return StreamId(pid=0x103, kind=StreamKindTag.UNKNOWN, codec=None, program_number=1)


def test_subtitle_event_lazy_payload_materialization_direct_construction():
    """_SubtitleEvent constructed with payload=b'...' wraps the bytes in a
    RawBytes holder — the PyBytes is not materialized until the first .payload
    access, then cached. Byte value is identical to the input."""
    payload_bytes = b"\xfe\xed\xca\xfe subtitle data"
    ev = DemuxEvent.Subtitle(
        stream=_make_stream_id_subtitle(),
        pts=Pts90khz.from_ms(500),
        dts=None,
        codec=SubtitleCodec.CEA708_STANDALONE,
        payload=payload_bytes,
    )
    # Unmaterialized: `_payload._materialized` must be False before first access.
    assert ev._payload._materialized is False, "PyBytes must not be materialized before first access"
    first = ev.payload
    assert isinstance(first, (bytes, bytearray))
    assert bytes(first) == payload_bytes, "payload bytes must match input"
    assert ev._payload._materialized is True
    # Repeated access returns the SAME cached object.
    assert ev.payload is first


def test_subtitle_event_lazy_payload_from_raw_bytes_holder():
    """When a RawBytes holder is passed directly (the demuxer path), the holder
    is stored as-is and .payload returns its byte content."""
    holder = _native.RawBytes(b"\x00\x01\x02\x03 subtitle")
    ev = DemuxEvent.Subtitle(
        stream=_make_stream_id_subtitle(),
        pts=Pts90khz.from_ms(100),
        dts=None,
        codec=SubtitleCodec.CEA708_STANDALONE,
        payload=holder,
    )
    # Holder was passed in already; _payload IS the holder.
    assert ev._payload is holder
    # Content is correct.
    assert bytes(ev.payload) == b"\x00\x01\x02\x03 subtitle"


def test_subtitle_event_equality_and_hash():
    """Two Subtitle events with equal payload compare equal and hash equal;
    different payload → not equal."""
    stream = _make_stream_id_subtitle()
    pts = Pts90khz.from_ms(100)
    p = b"sub payload bytes"

    def _mk(payload_arg):
        return DemuxEvent.Subtitle(
            stream=stream, pts=pts, dts=None,
            codec=SubtitleCodec.CEA708_STANDALONE, payload=payload_arg
        )

    a = _mk(p)
    b = _mk(bytes(p))  # distinct bytes object, same content
    assert a == b
    assert hash(a) == hash(b)
    assert a != _mk(b"different subtitle bytes")


def test_unknown_event_lazy_payload_materialization_direct_construction():
    """_UnknownSampleEvent constructed with payload=b'...' wraps the bytes in a
    RawBytes holder; the PyBytes is not materialized until .payload access."""
    payload_bytes = b"\xde\xad\xbe\xef unknown stream data"
    ev = DemuxEvent.UnknownSample(
        stream=_make_stream_id_unknown(),
        pts=Pts90khz.from_ms(200),
        dts=None,
        stream_type=0x80,
        payload=payload_bytes,
    )
    assert ev._payload._materialized is False
    first = ev.payload
    assert isinstance(first, (bytes, bytearray))
    assert bytes(first) == payload_bytes
    assert ev._payload._materialized is True
    assert ev.payload is first


def test_unknown_event_lazy_payload_from_raw_bytes_holder():
    """When a RawBytes holder is passed directly (the demuxer path), the holder
    is stored as-is and .payload returns its byte content."""
    holder = _native.RawBytes(b"\xff\xfe unknown")
    ev = DemuxEvent.UnknownSample(
        stream=_make_stream_id_unknown(),
        pts=Pts90khz.from_ms(200),
        dts=None,
        stream_type=0x81,
        payload=holder,
    )
    assert ev._payload is holder
    assert bytes(ev.payload) == b"\xff\xfe unknown"


def test_unknown_event_equality_and_hash():
    """Two UnknownSample events with equal payload compare equal and hash equal."""
    stream = _make_stream_id_unknown()
    pts = Pts90khz.from_ms(100)
    p = b"unknown payload"

    def _mk(payload_arg):
        return DemuxEvent.UnknownSample(
            stream=stream, pts=pts, dts=None, stream_type=0xAB, payload=payload_arg
        )

    a = _mk(p)
    b = _mk(bytes(p))
    assert a == b
    assert hash(a) == hash(b)
    assert a != _mk(b"different")


def _mux_subtitle_ts() -> bytes:
    """Build a minimal TS with one CEA-708 subtitle stream and one subtitle
    PES packet. Returns the raw TS bytes."""
    from tstrans.mpegts import Cea708StandaloneConfig
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_subtitle(0x102, Cea708StandaloneConfig())
        .pcr_pid(0x101)
        .build()
    )
    cfg = MuxerConfigBuilder().add_program(prog).build()
    mux = Muxer(cfg)
    pts = Pts90khz.from_ms(1000)
    # Push one H.264 key frame so the demuxer has a PMT context.
    au = b"\x00\x00\x00\x01\x65\x88\x84\x00\x10"
    vh = mux.video_stream_handle(0)
    mux.push_video_to(vh, au, pts=pts, key_frame=True)
    sh = mux.subtitle_handles()[0]
    mux.push_subtitle_to(sh, b"\xAA\xBB\xCC\xDD subtitle payload", pts=pts)
    out = bytearray()
    while True:
        buf = bytearray(1316)
        n = mux.pull(buf)
        if n == 0:
            break
        out += bytes(buf[:n])
    return bytes(out)


def test_subtitle_event_demuxer_path_lazy():
    """Demux a synthetic TS with a CEA-708 subtitle stream and verify that
    the resulting DemuxEvent.Subtitle carries a lazy RawBytes holder (not an
    eager PyBytes) and that .payload returns the correct bytes."""
    from tstrans.mpegts import Demuxer
    ts_bytes = _mux_subtitle_ts()
    demuxer = Demuxer()
    demuxer.feed(ts_bytes)
    demuxer.flush()
    found = False
    for ev in demuxer:
        if isinstance(ev, DemuxEvent.Subtitle):
            # The Rust side passes a RawBytes holder — _payload must exist and
            # start unmaterialized.
            assert hasattr(ev, "_payload"), "demuxer-path Subtitle must carry _payload holder"
            assert ev._payload._materialized is False, (
                "PyBytes must not be materialized before first .payload access on demuxer path"
            )
            payload = ev.payload
            assert isinstance(payload, (bytes, bytearray))
            # CEA-708 standalone uses Passthrough shape — no envelope wrapping
            # on either the mux or demux side, so bytes round-trip verbatim.
            assert payload == b"\xAA\xBB\xCC\xDD subtitle payload", (
                f"subtitle payload did not round-trip: got {payload!r}"
            )
            assert ev._payload._materialized is True
            assert ev.payload is payload  # cached
            found = True
            break
    assert found, "expected at least one DemuxEvent.Subtitle in the synthetic TS"
