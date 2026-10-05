"""MuxerConfig.from_program_map — the transmux bridge.

Covers: demux→mux round-trip via a captured DemuxEvent.ProgramMap,
strict offender rejection (DVB subtitling/teletext) + the `drop`
filter, the Unknown→DataStreamSpec mapping, exact
descriptor preservation for all typed stream kinds,
drop-argument validation, and sync-KLV kind reconstruction (codec=None)."""

import pytest

from tstrans.exceptions import MuxError, MuxErrorKind
from tstrans.mpegts import (
    AudioCodec,
    AudioStreamSpec,
    DataStreamSpec,
    Demuxer,
    DemuxEvent,
    KlvStreamSpec,
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    ProgramMap,
    Pts90khz,
    RawDescriptor,
    StreamInfo,
    StreamKindTag,
    SubtitleCodec,
    SubtitleStreamSpec,
    VideoCodec,
    VideoStreamSpec,
)

NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"
# A minimal-but-valid H.264 IDR access unit — payload for the synthetic
# unknown-stream TS builder's companion video stream.
IDR_AU = b"\x00\x00\x00\x01\x65\x88\x84\x00\x10\xaa\xbb"


def _video_si(pid=0x101):
    return StreamInfo(
        pid=pid,
        stream_type=0x1B,
        kind=StreamKindTag.VIDEO,
        codec=VideoCodec.H264,
        program_number=1,
    )


def _pm(*extra_streams):
    """A valid single-program pm: video at 0x101 (the PCR PID) + extras."""
    return ProgramMap(
        program_number=1,
        pcr_pid=0x101,
        pmt_pid=0x100,
        streams=(_video_si(), *extra_streams),
        klv_links=(),
    )


def test_roundtrip_demuxed_program_map_rebuilds_working_muxer():
    """Mux → demux → capture ProgramMap → from_program_map → new Muxer works."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    m = Muxer(MuxerConfigBuilder().add_program(prog).build())
    for i in range(5):
        m.push_video(NAL_AUD, pts=Pts90khz.from_raw(900_000 + i * 3000))
    buf = bytearray(188 * 4096)
    n = m.pull(buf)
    assert n > 0

    d = Demuxer()
    d.feed(bytes(buf[:n]))
    d.flush()
    pm_events = [ev for ev in d if isinstance(ev, DemuxEvent.ProgramMap)]
    assert pm_events, "expected at least one ProgramMap event"
    pm = pm_events[0].programs[0]
    assert pm.pmt_pid == 0x100

    cfg = MuxerConfig.from_program_map(pm)
    assert cfg.programs[0].program_number == 1
    assert cfg.programs[0].pmt_pid == 0x100
    assert sorted(s.pid for s in cfg.programs[0].streams) == [0x101, 0x102]

    # The rebuilt config must drive a working Muxer.
    m2 = Muxer(cfg)
    m2.push_video(NAL_AUD, pts=Pts90khz.from_raw(900_000))


def test_unknown_stream_maps_to_data_spec_and_droppable():
    # Deliberate pre-1.0 behavior change: unknown
    # stream types are no longer strict offenders — they map to
    # DataStreamSpec PES pass-through entries keeping the raw PMT
    # stream_type byte. drop=[UNKNOWN] still excludes them entirely.
    unknown = StreamInfo(
        pid=0x1F1,
        stream_type=0xC0,
        kind=StreamKindTag.UNKNOWN,
        codec=None,
        program_number=1,
    )
    pm = ProgramMap(
        program_number=1,
        pcr_pid=0x101,
        pmt_pid=0x100,
        streams=(_video_si(), unknown),
        klv_links=(),
    )

    cfg = MuxerConfig.from_program_map(pm)
    spec = cfg.programs[0].streams[1]
    assert spec == DataStreamSpec(pid=0x1F1, stream_type=0xC0, carries_pts=True)

    cfg = MuxerConfig.from_program_map(pm, drop=[StreamKindTag.UNKNOWN])
    assert [s.pid for s in cfg.programs[0].streams] == [0x101]


def test_from_program_map_unknown_to_data_introspection():
    """A demuxed unknown-stream map converts to a config whose data
    stream introspects as a DataStreamSpec; the stream's PMT descriptors
    are preserved byte-identical on the config's descriptor surface
    (stream_descriptors), not on the spec."""
    from _builders.unknown_stream import build_unknown_stream_ts

    ts = build_unknown_stream_ts(
        stream_type=0xF0,
        payload=b"private-bytes",
        descriptors=b"\xff\x0aSERIAL_ADF",
        video_au=IDR_AU,
    )
    d = Demuxer()
    d.feed(ts)
    d.flush()
    pm_events = [ev for ev in d if isinstance(ev, DemuxEvent.ProgramMap)]
    assert pm_events, "expected at least one ProgramMap event"
    pm = pm_events[0].programs[0]

    cfg = MuxerConfig.from_program_map(pm)
    prog = cfg.programs[0]
    assert DataStreamSpec(pid=0x101, stream_type=0xF0, carries_pts=True) in prog.streams
    i = [s.pid for s in prog.streams].index(0x101)
    assert prog.stream_descriptors[i] == (b"\xff\x0aSERIAL_ADF",)

    # drop=[UNKNOWN] excludes the data stream entirely.
    cfg = MuxerConfig.from_program_map(pm, drop=[StreamKindTag.UNKNOWN])
    assert [s.pid for s in cfg.programs[0].streams] == [0x102]
    assert not any(
        isinstance(s, DataStreamSpec) for s in cfg.programs[0].streams
    )


def test_audio_iso639_descriptor_preserved_verbatim_language_none():
    # from_program_map uses add_audio (language=None) for
    # every audio stream. The raw ISO-639 0x0A descriptor passes through
    # verbatim in stream_descriptors — it is NOT parsed into the `language`
    # field. This avoids re-encoding ambiguities (uppercase codes, multiple
    # entries, audio_type bytes) and guarantees byte-identical round-trips.
    audio = StreamInfo(
        pid=0x103,
        stream_type=0x0F,
        kind=StreamKindTag.AUDIO,
        codec=AudioCodec.AAC,
        program_number=1,
        # ISO 639 language descriptor (tag 0x0A): 3-byte code + audio_type byte.
        raw_descriptors=(RawDescriptor(tag=0x0A, data=b"eng\x00"),),
    )
    pm = ProgramMap(
        program_number=1,
        pcr_pid=0x101,
        pmt_pid=0x100,
        streams=(_video_si(), audio),
        klv_links=(),
    )
    cfg = MuxerConfig.from_program_map(pm)
    prog = cfg.programs[0]
    spec = prog.streams[1]
    assert isinstance(spec, AudioStreamSpec)
    assert spec.codec is AudioCodec.AAC
    # language field is None — add_audio is used, not add_audio_with_language.
    assert spec.language is None
    # The 0x0A descriptor is preserved verbatim in stream_descriptors as a
    # TLV byte string: tag (0x0A) + length (4) + data (b"eng\x00").
    audio_idx = [s.pid for s in prog.streams].index(0x103)
    assert prog.stream_descriptors[audio_idx] == (b"\x0a\x04eng\x00",)


def test_drop_rejects_non_enum_items():
    audio = StreamInfo(
        pid=0x103,
        stream_type=0x0F,
        kind=StreamKindTag.AUDIO,
        codec=AudioCodec.AAC,
        program_number=1,
    )
    pm = ProgramMap(
        program_number=1,
        pcr_pid=0x101,
        pmt_pid=0x100,
        streams=(_video_si(), audio),
        klv_links=(),
    )

    # Plain strings are not StreamKindTag members — ValueError, not a
    # silent no-match.
    with pytest.raises(ValueError, match="StreamKindTag"):
        MuxerConfig.from_program_map(pm, drop=["video"])

    # The enum member works: video dropped, audio kept.
    cfg = MuxerConfig.from_program_map(pm, drop=[StreamKindTag.VIDEO])
    assert [s.pid for s in cfg.programs[0].streams] == [0x103]


# Drift guard for the string-matched reverse maps: every representable
# enum member must survive Python → Rust → MuxerConfig → Python intact.
@pytest.mark.parametrize(
    "kind,codec,stream_type",
    [
        (StreamKindTag.VIDEO, VideoCodec.H264, 0x1B),
        (StreamKindTag.VIDEO, VideoCodec.H265, 0x24),
        (StreamKindTag.VIDEO, VideoCodec.H266, 0x33),
        (StreamKindTag.VIDEO, VideoCodec.AV1, 0x06),
        (StreamKindTag.AUDIO, AudioCodec.MP2, 0x03),
        (StreamKindTag.AUDIO, AudioCodec.AAC, 0x0F),
        (StreamKindTag.AUDIO, AudioCodec.AAC_LATM, 0x11),
        (StreamKindTag.AUDIO, AudioCodec.AC3, 0x81),
        (StreamKindTag.SUBTITLE, SubtitleCodec.CEA708_STANDALONE, 0x06),
        (StreamKindTag.SUBTITLE, SubtitleCodec.WEBVTT_IN_TS, 0x06),
        (StreamKindTag.KLV_SYNC, None, 0x15),
        (StreamKindTag.KLV_ASYNC, None, 0x06),
    ],
    ids=lambda v: getattr(v, "name", v),
)
def test_every_representable_member_round_trips(kind, codec, stream_type):
    si = StreamInfo(
        pid=0x200,
        stream_type=stream_type,
        kind=kind,
        codec=codec,
        program_number=1,
    )
    cfg = MuxerConfig.from_program_map(_pm(si))
    spec = cfg.programs[0].streams[1]
    assert spec.pid == 0x200
    if kind is StreamKindTag.VIDEO:
        assert isinstance(spec, VideoStreamSpec)
        assert spec.codec is codec
    elif kind is StreamKindTag.AUDIO:
        assert isinstance(spec, AudioStreamSpec)
        assert spec.codec is codec
    elif kind is StreamKindTag.SUBTITLE:
        assert isinstance(spec, SubtitleStreamSpec)
        assert spec.codec is codec
    else:
        assert isinstance(spec, KlvStreamSpec)
        expected = (
            KlvStreamType.SYNCHRONOUS_METADATA
            if kind is StreamKindTag.KLV_SYNC
            else KlvStreamType.PRIVATE_DATA
        )
        assert spec.stream_type is expected
        assert spec.carries_pts is True


@pytest.mark.parametrize(
    "codec", [SubtitleCodec.DVB_SUBTITLING, SubtitleCodec.DVB_TELETEXT]
)
def test_dvb_subtitle_members_are_offenders(codec):
    """DVB subtitle params aren't recoverable from the PMT — offender path."""
    si = StreamInfo(
        pid=0x200,
        stream_type=0x06,
        kind=StreamKindTag.SUBTITLE,
        codec=codec,
        program_number=1,
    )
    with pytest.raises(MuxError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
    assert "cannot represent" in str(ei.value)
    assert "0x0200" in str(ei.value)


def test_kind_codec_mismatch_errors_name_the_stream():
    # VIDEO with codec=None.
    si = StreamInfo(
        pid=0x205,
        stream_type=0x1B,
        kind=StreamKindTag.VIDEO,
        codec=None,
        program_number=1,
    )
    with pytest.raises(ValueError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "stream pid 0x0205" in str(ei.value)
    assert "VideoCodec" in str(ei.value)

    # VIDEO with a member of the wrong codec enum.
    si = StreamInfo(
        pid=0x206,
        stream_type=0x1B,
        kind=StreamKindTag.VIDEO,
        codec=AudioCodec.AAC,
        program_number=1,
    )
    with pytest.raises(ValueError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "stream pid 0x0206" in str(ei.value)
    assert "unknown VideoCodec: AAC" in str(ei.value)

    # VIDEO with a non-enum codec (no `.name` at all).
    si = StreamInfo(
        pid=0x207,
        stream_type=0x1B,
        kind=StreamKindTag.VIDEO,
        codec=42,
        program_number=1,
    )
    with pytest.raises(ValueError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "stream pid 0x0207" in str(ei.value)
    assert "expected a VideoCodec member; got 42" in str(ei.value)


@pytest.mark.parametrize(
    "kind,stream_type",
    [
        (StreamKindTag.KLV_SYNC, 0x15),
        (StreamKindTag.KLV_ASYNC, 0x06),
        (StreamKindTag.UNKNOWN, 0xC0),
    ],
    ids=lambda v: getattr(v, "name", v),
)
def test_codec_on_codecless_kind_is_rejected(kind, stream_type):
    """KLV/UNKNOWN kinds carry no codec — a stray one is a kind/codec
    mismatch, not silently ignored."""
    si = StreamInfo(
        pid=0x209,
        stream_type=stream_type,
        kind=kind,
        codec=AudioCodec.AAC,
        program_number=1,
    )
    with pytest.raises(ValueError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "stream pid 0x0209" in str(ei.value)
    assert f"kind={kind.name} must have codec=None" in str(ei.value)
    assert "AudioCodec.AAC" in str(ei.value)


def test_field_extract_errors_name_field_and_stream():
    # pid out of u16 range — identified by index (pid isn't known yet).
    si = StreamInfo(
        pid=70000,
        stream_type=0x1B,
        kind=StreamKindTag.VIDEO,
        codec=VideoCodec.H264,
        program_number=1,
    )
    with pytest.raises(OverflowError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "streams[1].pid" in str(ei.value)

    # stream_type out of u8 range — identified by pid.
    si = StreamInfo(
        pid=0x208,
        stream_type=0x1FF,
        kind=StreamKindTag.VIDEO,
        codec=VideoCodec.H264,
        program_number=1,
    )
    with pytest.raises(OverflowError) as ei:
        MuxerConfig.from_program_map(_pm(si))
    assert "stream pid 0x0208: stream_type" in str(ei.value)


def test_sync_klv_reconstructs_without_codec():
    klv = StreamInfo(
        pid=0x102,
        stream_type=0x15,
        kind=StreamKindTag.KLV_SYNC,
        codec=None,
        program_number=1,
    )
    pm = ProgramMap(
        program_number=1,
        pcr_pid=0x101,
        pmt_pid=0x100,
        streams=(_video_si(), klv),
        klv_links=(),
    )
    cfg = MuxerConfig.from_program_map(pm)
    spec = cfg.programs[0].streams[1]
    assert isinstance(spec, KlvStreamSpec)
    assert spec.stream_type is KlvStreamType.SYNCHRONOUS_METADATA
    # carries_pts is always True from from_program_map (PES-level
    # property the PMT cannot declare; STANAG 4609 norm).
    assert spec.carries_pts is True
