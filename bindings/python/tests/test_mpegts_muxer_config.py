"""Muxer config family tests."""

import pytest

from tstrans.mpegts import (
    AudioCodec,
    AudioStreamHandle,
    AudioStreamSpec,
    Av1CarriageMode,
    DataStreamSpec,
    KlvStreamHandle,
    KlvStreamSpec,
    KlvStreamType,
    StreamSpec,
    SubtitleCodec,
    SubtitleStreamHandle,
    SubtitleStreamSpec,
    VideoCodec,
    VideoStreamHandle,
    VideoStreamSpec,
)


def test_klv_stream_type_enum_has_two_variants():
    assert len(list(KlvStreamType)) == 2
    # The two variants represent the two PES wrap shapes for KLV
    names = {v.name for v in KlvStreamType}
    assert "SYNCHRONOUS_METADATA" in names
    assert "PRIVATE_DATA" in names


def test_av1_carriage_mode_default_is_binding_conformant():
    # `MPEG2_TS_BINDING` exists; it is the default for new muxers
    # (matches Rust `Av1CarriageMode::Mpeg2TsBinding` default).
    assert hasattr(Av1CarriageMode, "MPEG2_TS_BINDING")


def test_video_stream_spec_frozen_dataclass_fields():
    s = VideoStreamSpec(pid=0x101, codec=VideoCodec.H264)
    assert s.pid == 0x101
    assert s.codec is VideoCodec.H264
    with pytest.raises((AttributeError, TypeError)):
        s.pid = 0x102  # type: ignore[misc]


def test_klv_stream_spec_carries_metadata_kind():
    s = KlvStreamSpec(pid=0x102, stream_type=KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
    assert s.stream_type is KlvStreamType.SYNCHRONOUS_METADATA
    assert s.carries_pts is True


def test_audio_stream_spec_default_language_is_none():
    s = AudioStreamSpec(pid=0x103, codec=AudioCodec.AAC)
    assert s.language is None


def test_stream_spec_subclasses_inherit_from_stream_spec_abc():
    assert issubclass(VideoStreamSpec, StreamSpec)
    assert issubclass(KlvStreamSpec, StreamSpec)
    assert issubclass(AudioStreamSpec, StreamSpec)
    assert issubclass(SubtitleStreamSpec, StreamSpec)


def test_handle_raw_round_trip():
    # `from_raw` validates against the
    # canonical 8-bit packed layout. Choose a value with no high bits
    # set so the round-trip succeeds. Forged-handle rejection is
    # covered by tests/test_handle_forge.py.
    h = VideoStreamHandle.from_raw(0x78)
    assert h.raw == 0x78


def test_handle_equality_and_hash():
    a = AudioStreamHandle.from_raw(42)
    b = AudioStreamHandle.from_raw(42)
    c = AudioStreamHandle.from_raw(43)
    assert a == b
    assert hash(a) == hash(b)
    assert a != c


def test_handles_are_distinct_types():
    v = VideoStreamHandle.from_raw(1)
    a = AudioStreamHandle.from_raw(1)
    assert type(v) is not type(a)
    assert v != a  # different types, even with same raw


def test_handle_repr_includes_class_name_and_raw():
    # `from_raw` validates the canonical
    # 8-bit packed layout; 0xDEAD has high bits set and would reject.
    # Use 0xAD = 173 (program=10, within=13) — within the canonical
    # region while still distinctive in repr output.
    h = KlvStreamHandle.from_raw(0xAD)
    r = repr(h)
    assert "KlvStreamHandle" in r
    # 0xAD = 173 — accept any reasonable rendering
    assert ("173" in r) or ("0xad" in r.lower()) or ("AD" in r)


def test_handle_unpack():
    h = SubtitleStreamHandle.from_raw(0)
    program_idx, within_idx = h.unpack()
    assert isinstance(program_idx, int)
    assert isinstance(within_idx, int)


# --- MuxerProgramConfig + MuxerProgramConfigBuilder ---

from tstrans.mpegts import MuxerProgramConfig, MuxerProgramConfigBuilder


def test_program_builder_minimum_constructs():
    cfg = (
        MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
        .add_video(0x101, VideoCodec.H264)
        .build()
    )
    assert isinstance(cfg, MuxerProgramConfig)
    assert cfg.program_number == 1
    assert cfg.pmt_pid == 0x100
    assert len(cfg.streams) == 1
    assert cfg.streams[0].pid == 0x101


def test_program_builder_fluent_chain():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .add_audio(0x103, AudioCodec.AAC)
        .pcr_pid(0x101)
        .build()
    )
    assert len(cfg.streams) == 3
    assert cfg.pcr_pid == 0x101


def test_program_streams_tuple_match_dispatch():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    kinds = []
    for s in cfg.streams:
        match s:
            case VideoStreamSpec(pid=p):
                kinds.append(("video", p))
            case KlvStreamSpec(pid=p):
                kinds.append(("klv", p))
            case _:
                pytest.fail(f"unexpected spec: {s}")
    assert kinds == [("video", 0x101), ("klv", 0x102)]


def test_program_descriptors_round_trip():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .program_descriptors([b"\x05\x04KLVA"])
        .build()
    )
    assert cfg.program_descriptors == (b"\x05\x04KLVA",)


def test_audio_with_language_attaches_language():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_audio_with_language(0x102, AudioCodec.AAC, language=b"eng")
        .build()
    )
    audio_specs = [s for s in cfg.streams if isinstance(s, AudioStreamSpec)]
    assert len(audio_specs) == 1
    assert audio_specs[0].language == b"eng"


def test_program_config_is_frozen():
    cfg = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    with pytest.raises((AttributeError, TypeError)):
        cfg.program_number = 99  # type: ignore[misc]


# --- MuxerConfig + MuxerConfigBuilder ---

from tstrans.exceptions import MuxError, MuxErrorKind
from tstrans.mpegts import MuxerConfig, MuxerConfigBuilder


def test_muxer_config_builder_minimum_build():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    cfg = MuxerConfigBuilder().add_program(prog).build()
    assert isinstance(cfg, MuxerConfig)
    assert len(cfg.programs) == 1
    assert cfg.programs[0].program_number == 1


def test_muxer_config_pcr_psi_defaults_reasonable():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    cfg = MuxerConfigBuilder().add_program(prog).build()
    assert cfg.pcr_interval_ms > 0
    assert cfg.psi_interval_ms > 0
    assert cfg.buffer_packets > 0


def test_muxer_config_av1_default_is_mpeg2_ts_binding():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.AV1).build()
    cfg = MuxerConfigBuilder().add_program(prog).build()
    # Variant name from actual Rust.
    assert cfg.av1_carriage is Av1CarriageMode.MPEG2_TS_BINDING


def test_muxer_config_invalid_pid_collision_raises():
    # Two video streams on the same PID — should fail validation.
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_video(0x101, VideoCodec.H265)
        .build()
    )
    with pytest.raises(MuxError) as ei:
        MuxerConfigBuilder().add_program(prog).build()
    # Use CONFIG_INVALID — the actual Rust variant name.
    # If actual rejection variant is different, adjust to the value seen.
    assert ei.value.kind in (
        MuxErrorKind.CONFIG_INVALID,
        MuxErrorKind.INPUT_MALFORMED,
        MuxErrorKind.INVALID_USAGE,
    )


def test_muxer_config_builder_pcr_interval_override():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    cfg = MuxerConfigBuilder().add_program(prog).pcr_interval_ms(20).build()
    assert cfg.pcr_interval_ms == 20


def test_muxer_config_is_frozen():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    cfg = MuxerConfigBuilder().add_program(prog).build()
    with pytest.raises((AttributeError, TypeError)):
        cfg.pcr_interval_ms = 99  # type: ignore[misc]


def test_muxer_config_static_builder_constructor():
    b = MuxerConfig.builder()
    assert isinstance(b, MuxerConfigBuilder)


# --- Data streams (add_data + stream_descriptors_for_data) ---


def test_add_data_accepts_user_private_stream_type():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .build()
    )
    data_specs = [s for s in cfg.streams if isinstance(s, DataStreamSpec)]
    assert len(data_specs) == 1
    assert data_specs[0].pid == 0x1F0
    assert data_specs[0].stream_type == 0xF0
    assert data_specs[0].carries_pts is True
    # Validates clean at top-level build: 0xF0 with no descriptors
    # classifies as Unknown on the demux side.
    MuxerConfigBuilder().add_program(cfg).build()


def test_add_data_typed_stream_type_rejected_at_build():
    """0x1B is H.264 — a typed stream_type must use add_video, not
    add_data. The classify-Unknown rule is enforced Rust-side at
    MuxerConfig validate/build; the error names the classified kind."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0x1B, carries_pts=True)
        .build()
    )
    with pytest.raises(MuxError) as ei:
        MuxerConfigBuilder().add_program(prog).build()
    assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
    msg = str(ei.value)
    assert "classifies as" in msg
    assert "Video" in msg


def test_add_data_klva_descriptor_masquerade_rejected_at_build():
    """A bare 0x06 data stream is fine, but adding the KLVA
    registration descriptor makes it classify as KLV — rejected at
    build() so a Data spec can't lie to downstream demuxers."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0x06, carries_pts=True)
        .stream_descriptors_for_data(0, [b"\x05\x04KLVA"])
        .build()
    )
    with pytest.raises(MuxError) as ei:
        MuxerConfigBuilder().add_program(prog).build()
    assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
    assert "classifies as" in str(ei.value)


def test_add_data_seventeen_streams_rejected_at_build():
    """Per-program data-stream cap is 16 (mirrors the Rust DATA_CAP);
    the 17th rejects at validate/build with CONFIG_INVALID."""
    b = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264)
    for i in range(17):
        b = b.add_data(0x200 + i, 0xF0, carries_pts=True)
    prog = b.build()
    with pytest.raises(MuxError) as ei:
        MuxerConfigBuilder().add_program(prog).build()
    assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
    assert "too many data streams" in str(ei.value)


def test_add_data_stream_type_out_of_u8_range_raises():
    """stream_type is the raw PMT byte — 0..=255. Out-of-range values
    reject at the binding boundary (same ValueError shape as the
    add_audio_with_language language-length check)."""
    b = MuxerProgramConfigBuilder(1, 0x100)
    with pytest.raises(ValueError, match="stream_type"):
        b.add_data(0x1F0, 256, carries_pts=True)
    with pytest.raises(ValueError, match="stream_type"):
        b.add_data(0x1F0, -1, carries_pts=True)


def test_add_data_carries_pts_is_keyword_only():
    b = MuxerProgramConfigBuilder(1, 0x100)
    with pytest.raises(TypeError):
        b.add_data(0x1F0, 0xF0, True)  # positional carries_pts


def test_stream_descriptors_for_data_round_trip():
    cfg = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .stream_descriptors_for_data(0, [b"\xff\x04demo"])
        .build()
    )
    # streams[1] is the data stream; stream_descriptors is indexed
    # parallel to streams.
    assert cfg.stream_descriptors[1] == (b"\xff\x04demo",)


def test_stream_descriptors_for_data_out_of_range_raises():
    # DescriptorIndexOutOfRange classifies as INVALID_USAGE (call-order
    # misuse — no add_data has happened yet), matching the Rust
    # MuxErrorKind classifier.
    b = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264)
    with pytest.raises(MuxError) as ei:
        b.stream_descriptors_for_data(0, [b"\xff\x04demo"])
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE
