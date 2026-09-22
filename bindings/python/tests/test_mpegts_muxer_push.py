"""Phase 4 Muxer push + drain tests."""

import pytest

from tstrans.exceptions import MuxError, MuxErrorKind
from tstrans.mpegts import (
    AudioCodec,
    KlvStreamType,
    Muxer,
    MuxerConfig,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)


def _simple_config() -> MuxerConfig:
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_audio(0x102, AudioCodec.AAC)
        .add_klv(0x103, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


def test_muxer_constructs_with_valid_config():
    m = Muxer(_simple_config())
    assert isinstance(m, Muxer)


def test_muxer_initial_pending_is_non_negative():
    m = Muxer(_simple_config())
    # Initial state may pre-emit PAT/PMT; either 0 or a small number is OK
    assert m.pending_packets() >= 0


def test_pull_with_empty_bytearray_returns_zero():
    m = Muxer(_simple_config())
    buf = bytearray(0)
    n = m.pull(buf)
    assert n == 0


def test_pull_with_small_buffer_pre_push_returns_zero():
    # No data yet — pull returns 0 regardless.
    m = Muxer(_simple_config())
    buf = bytearray(50)
    n = m.pull(buf)
    assert n == 0


def test_capacity_packets_is_positive():
    m = Muxer(_simple_config())
    assert m.capacity_packets() > 0


# ---------------------------------------------------------------------------
# Task 7 — push_video / push_video_to / push_video_to_with_dts
# ---------------------------------------------------------------------------


def _minimal_h264_nal_aud() -> bytes:
    """Annex B Access Unit Delimiter NAL — smallest valid H.264 NAL.

    `00 00 00 01` Annex-B 4-byte start code, then `09` (nal_unit_type=9
    AUD) and `F0` (primary_pic_type=7, trailing rbsp_trailing_bits).
    Two body bytes satisfy `validate_annex_b`'s "non-empty NAL body"
    rule and is small enough to fit in a single TS packet.
    """
    return b"\x00\x00\x00\x01\x09\xF0"


def test_push_video_single_target_form_increments_pending():
    m = Muxer(_simple_config())
    before = m.pending_packets()
    m.push_video(_minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > before


def test_push_video_invalid_nal_raises_invalid_nal():
    m = Muxer(_simple_config())
    with pytest.raises(MuxError) as ei:
        # No Annex-B start code — should fail validate_annex_b.
        m.push_video(b"\xDE\xAD\xBE\xEF", pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind == MuxErrorKind.INVALID_NAL


def test_push_video_to_with_invalid_handle_raises_invalid_usage():
    """An out-of-range handle surfaces as InvalidStreamHandle →
    INVALID_USAGE per the MuxErrorKind classifier in
    tst-core/src/error.rs. Uses a within-canonical-layout-but-not-configured
    handle (program=15, within=15 = 0xFF) so the closeout audit's
    `from_raw` validation passes and the push-time range check fires.
    Forged handles with bits outside the canonical layout are covered
    by `tests/test_handle_forge.py`."""
    from tstrans.mpegts import VideoStreamHandle

    m = Muxer(_simple_config())
    # 0xFF = (program=15, within=15) — canonical layout, but no such
    # stream is configured. Push-time range check rejects.
    bogus = VideoStreamHandle.from_raw(0xFF)
    with pytest.raises(MuxError) as ei:
        m.push_video_to(bogus, _minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind == MuxErrorKind.INVALID_USAGE


def test_push_video_then_pull_emits_188_aligned_bytes():
    m = Muxer(_simple_config())
    m.push_video(_minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    n_packets = int(m.pending_packets())
    assert n_packets > 0
    buf = bytearray(n_packets * 188)
    n = m.pull(buf)
    assert n > 0
    assert n % 188 == 0


def test_push_video_to_handle_form_works():
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_to(handle, _minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0


def test_push_video_to_with_dts_b_frame_schedule():
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_to_with_dts(
        handle,
        _minimal_h264_nal_aud(),
        pts=Pts90khz.from_raw(990_000),
        dts=Pts90khz.from_raw(900_000),
    )
    assert m.pending_packets() > 0


def test_push_video_key_frame_arg_accepted():
    """key_frame=True must be a valid kwarg (random_access_indicator
    semantics tested in Rust-side tests; here we just verify the Python
    surface accepts it)."""
    m = Muxer(_simple_config())
    m.push_video(
        _minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000), key_frame=True
    )
    assert m.pending_packets() > 0


# ---------------------------------------------------------------------------
# Task 8 — push_audio + push_klv + push_subtitle (single-stream + handle forms)
# ---------------------------------------------------------------------------


def _minimal_aac_frame() -> bytes:
    """ADTS header — syncword + LC profile + 44.1kHz + mono + length=7.

    Smallest valid AAC-ADTS frame: 7-byte header with no payload. The
    mux-side audio path just frames bytes into PES; the parser is
    receiver-side, so any well-shaped ADTS header is enough to exercise
    the push path.
    """
    return b"\xFF\xF1\x4C\x40\x00\x1F\xFC"


def _minimal_klv_ls() -> bytes:
    """16-byte SMPTE UL + 1-byte BER + 0-byte body = minimal LS bytes.

    The muxer auto-prepends the 5-byte `Metadata_AU_cell` header per
    ITU-T H.222.0 §2.12.4.2 for SynchronousMetadata streams (CLAUDE.md
    "KLV AU cell auto-wrap"), so callers pass raw KLV LS bytes only.
    """
    return b"\x06\x0E\x2B\x34\x02\x0B\x01\x01\x0E\x01\x03\x01\x01\x00\x00\x00\x00"


def test_push_audio_works_with_single_audio_stream():
    m = Muxer(_simple_config())
    m.push_audio(_minimal_aac_frame(), pts=Pts90khz.from_raw(900_000))
    ts = _pull_all(m)
    assert len(ts) > 0, "audio push emitted no TS bytes"
    assert len(ts) % 188 == 0


def test_push_klv_works_with_single_klv_stream():
    m = Muxer(_simple_config())
    m.push_klv(
        _minimal_klv_ls(), pts=Pts90khz.from_raw(900_000), metadata_service_id=0
    )
    ts = _pull_all(m)
    assert len(ts) > 0, "klv push emitted no TS bytes"
    assert len(ts) % 188 == 0


def test_push_klv_default_metadata_service_id():
    """metadata_service_id defaults to 0 — the most common single-service
    case. Callers needing a non-zero value pass it explicitly."""
    ref = Muxer(_simple_config())
    ref.push_klv(_minimal_klv_ls(), pts=Pts90khz.from_raw(900_000), metadata_service_id=0)
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "explicit metadata_service_id=0 push emitted no TS"
    m = Muxer(_simple_config())
    m.push_klv(_minimal_klv_ls(), pts=Pts90khz.from_raw(900_000))
    assert _pull_all(m) == ref_ts, "default metadata_service_id differs from explicit 0"


def test_push_audio_invalid_handle_raises():
    # See test_push_video_to_with_invalid_handle_raises_invalid_usage for
    # rationale on the 0xFF (canonical-but-unconfigured) handle choice.
    from tstrans.mpegts import AudioStreamHandle

    m = Muxer(_simple_config())
    bad = AudioStreamHandle.from_raw(0xFF)
    with pytest.raises(MuxError) as ei:
        # Audit #9 normalized arg order: (handle, frames, *, pts).
        m.push_audio_to(bad, _minimal_aac_frame(), pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


def test_push_klv_invalid_handle_raises():
    # See test_push_video_to_with_invalid_handle_raises_invalid_usage for
    # rationale on the 0xFF (canonical-but-unconfigured) handle choice.
    from tstrans.mpegts import KlvStreamHandle

    m = Muxer(_simple_config())
    bad = KlvStreamHandle.from_raw(0xFF)
    with pytest.raises(MuxError) as ei:
        m.push_klv_to(bad, _minimal_klv_ls(), pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


def _subtitle_config(codec_config) -> MuxerConfig:
    """Single-program muxer with one video + one subtitle stream.

    Pair the subtitle stream with a video stream because the muxer's
    PCR pid defaults to the first video — without one, builder validation
    would surface a config error.
    """
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_subtitle(0x200, codec_config)
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


def test_push_subtitle_works_webvtt_round_trip():
    """End-to-end: build a WebVTT-in-TS subtitle stream, push a cue,
    pull TS bytes, demux, assert payload + spec match."""
    from tstrans.mpegts import (
        Demuxer,
        DemuxerConfig,
        DemuxEvent,
        StreamKindTag,
        SubtitleCodec as SubtitleCodecEnum,
        WebVttInTsConfig,
    )

    cfg = _subtitle_config(WebVttInTsConfig())
    m = Muxer(cfg)

    # Sanity: builder produced a subtitle stream the muxer recognizes.
    assert m.stats().subtitle_streams_configured == 1
    assert len(m.subtitle_handles()) == 1

    cue = b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n"
    m.push_subtitle(cue, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0

    # Drain all TS bytes (PAT + PMT + subtitle PES).
    buf = bytearray(m.pending_packets() * 188)
    n = m.pull(buf)
    assert n > 0
    assert n % 188 == 0
    ts_bytes = bytes(buf[:n])

    # Demux and find the subtitle event.
    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    events = list(d)

    # PMT should advertise the WebVtt subtitle stream.
    pmts = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert pmts, "expected at least one ProgramMap event"
    subtitle_streams = [
        s
        for s in pmts[-1].programs[0].streams
        if s.kind is StreamKindTag.SUBTITLE
    ]
    assert len(subtitle_streams) == 1, f"got {subtitle_streams}"
    assert subtitle_streams[0].codec is SubtitleCodecEnum.WEBVTT_IN_TS
    assert subtitle_streams[0].pid == 0x200

    # Subtitle event payload round-trips byte-for-byte.
    sub_events = [e for e in events if isinstance(e, DemuxEvent.Subtitle)]
    assert sub_events, "expected at least one Subtitle event"
    assert sub_events[0].payload == cue


def test_push_subtitle_works_dvb_subtitling_round_trip():
    """DVB subtitling — exercises the struct-variant config path with
    full language + page-id parameters."""
    from tstrans.mpegts import (
        Demuxer,
        DemuxerConfig,
        DemuxEvent,
        DvbSubtitlingConfig,
        StreamKindTag,
        SubtitleCodec as SubtitleCodecEnum,
    )

    cfg = _subtitle_config(
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=0x10,
            composition_page_id=0x1234,
            ancillary_page_id=0x5678,
        )
    )
    m = Muxer(cfg)
    assert m.stats().subtitle_streams_configured == 1

    # 3-byte DVB sub data: data_identifier=0x20, subtitle_stream_id=0x00,
    # end_of_PES_data_field_marker=0xFF. The muxer auto-wraps the PES
    # envelope per ETSI EN 300 743 §6.2, so callers pass raw segments.
    payload = b"\x42\x42\x42"
    m.push_subtitle(payload, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0

    buf = bytearray(m.pending_packets() * 188)
    n = m.pull(buf)
    ts_bytes = bytes(buf[:n])

    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    events = list(d)

    pmts = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert pmts
    subtitle_streams = [
        s
        for s in pmts[-1].programs[0].streams
        if s.kind is StreamKindTag.SUBTITLE
    ]
    # The demuxer collapses struct-variant subtitle codecs to the flat
    # enum tag — the per-stream descriptor bytes carry the language /
    # page IDs (not surfaced via this Python `StreamInfo` listing).
    assert len(subtitle_streams) == 1
    assert subtitle_streams[0].codec is SubtitleCodecEnum.DVB_SUBTITLING
    assert subtitle_streams[0].pid == 0x200


def test_push_subtitle_works_dvb_teletext_round_trip():
    """DVB teletext — exercises the second struct-variant config path."""
    from tstrans.mpegts import (
        Demuxer,
        DemuxerConfig,
        DemuxEvent,
        DvbTeletextConfig,
        StreamKindTag,
        SubtitleCodec as SubtitleCodecEnum,
    )

    cfg = _subtitle_config(
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,  # subtitle page
            magazine_number=1,
            page_number=0x88,
        )
    )
    m = Muxer(cfg)
    assert m.stats().subtitle_streams_configured == 1

    # 1-byte data identifier (per ETSI EN 300 472 §4.1).
    payload = b"\x10"
    m.push_subtitle(payload, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0

    buf = bytearray(m.pending_packets() * 188)
    n = m.pull(buf)
    ts_bytes = bytes(buf[:n])

    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    events = list(d)

    pmts = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert pmts
    subtitle_streams = [
        s
        for s in pmts[-1].programs[0].streams
        if s.kind is StreamKindTag.SUBTITLE
    ]
    assert len(subtitle_streams) == 1
    assert subtitle_streams[0].codec is SubtitleCodecEnum.DVB_TELETEXT
    assert subtitle_streams[0].pid == 0x200


def test_push_subtitle_works_cea708_standalone_round_trip():
    """CEA-708 standalone — exercises the second unit-variant config."""
    from tstrans.mpegts import (
        Cea708StandaloneConfig,
        Demuxer,
        DemuxerConfig,
        DemuxEvent,
        StreamKindTag,
        SubtitleCodec as SubtitleCodecEnum,
    )

    cfg = _subtitle_config(Cea708StandaloneConfig())
    m = Muxer(cfg)
    assert m.stats().subtitle_streams_configured == 1

    payload = b"\xFF\xFF\xFF"
    m.push_subtitle(payload, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0

    buf = bytearray(m.pending_packets() * 188)
    n = m.pull(buf)
    ts_bytes = bytes(buf[:n])

    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    events = list(d)

    pmts = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert pmts
    subtitle_streams = [
        s
        for s in pmts[-1].programs[0].streams
        if s.kind is StreamKindTag.SUBTITLE
    ]
    assert len(subtitle_streams) == 1
    assert subtitle_streams[0].codec is SubtitleCodecEnum.CEA708_STANDALONE
    assert subtitle_streams[0].pid == 0x200


def test_push_subtitle_to_handle_form_works():
    """Handle-form push works after `add_subtitle` plumbing."""
    from tstrans.mpegts import SubtitleStreamHandle, WebVttInTsConfig

    cfg = _subtitle_config(WebVttInTsConfig())
    m = Muxer(cfg)
    handles = m.subtitle_handles()
    assert len(handles) == 1
    assert isinstance(handles[0], SubtitleStreamHandle)

    m.push_subtitle_to(
        handles[0],
        b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n",
        pts=Pts90khz.from_raw(900_000),
    )
    assert m.pending_packets() > 0


def test_push_subtitle_to_invalid_handle_raises():
    """Within-canonical-layout but unconfigured handle surfaces as
    INVALID_USAGE at push-time via the MuxErrorKind classifier.
    (Forged-high-bit handles are rejected earlier at `from_raw` itself —
    that contract is covered in `test_handle_forge.py`.)"""
    from tstrans.mpegts import SubtitleStreamHandle, WebVttInTsConfig

    cfg = _subtitle_config(WebVttInTsConfig())
    m = Muxer(cfg)
    bogus = SubtitleStreamHandle.from_raw(0xFF)
    with pytest.raises(MuxError) as ei:
        m.push_subtitle_to(
            bogus,
            b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nx\n",
            pts=Pts90khz.from_raw(900_000),
        )
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


# ---------------------------------------------------------------------------
# Subtitle codec config dataclass validation (__post_init__ guards)
# ---------------------------------------------------------------------------


def test_dvb_subtitling_config_rejects_out_of_range_page_ids():
    """Page IDs are u16; values >= 0x10000 must be rejected at
    construction (not later inside the muxer)."""
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(ValueError, match="composition_page_id"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=0x10,
            composition_page_id=0x10000,  # one over u16
            ancillary_page_id=0,
        )

    with pytest.raises(ValueError, match="ancillary_page_id"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=0x10,
            composition_page_id=0,
            ancillary_page_id=0x10000,
        )


def test_dvb_subtitling_config_rejects_wrong_language_length():
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(ValueError, match="language"):
        DvbSubtitlingConfig(
            language=b"en",  # 2 bytes — should be 3
            subtitling_type=0x10,
            composition_page_id=0,
            ancillary_page_id=0,
        )


def test_dvb_subtitling_config_rejects_subtitling_type_out_of_u8():
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(ValueError, match="subtitling_type"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=256,  # one over u8
            composition_page_id=0,
            ancillary_page_id=0,
        )


def test_dvb_teletext_config_rejects_magazine_over_seven():
    """magazine_number is 3 bits — 0..=7. Higher values are wire-invalid."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(ValueError, match="magazine_number"):
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,
            magazine_number=8,  # out of 3-bit range
            page_number=0x88,
        )


def test_dvb_teletext_config_rejects_non_bcd_page_number():
    """page_number is BCD-encoded — each nibble must be 0..=9."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(ValueError, match="page_number"):
        # Low nibble = 0xA (10) — invalid BCD.
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,
            magazine_number=1,
            page_number=0x8A,
        )

    with pytest.raises(ValueError, match="page_number"):
        # Over 0x99.
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,
            magazine_number=1,
            page_number=0x100,
        )


def test_dvb_teletext_config_rejects_teletext_type_over_31():
    """teletext_type is a 5-bit field (0..=31)."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(ValueError, match="teletext_type"):
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=32,  # one over 5-bit range
            magazine_number=1,
            page_number=0x88,
        )


def test_add_subtitle_rejects_non_subtitle_codec_config():
    """`add_subtitle` raises TypeError when passed an arbitrary object."""

    with pytest.raises(TypeError, match="DvbSubtitlingConfig"):
        MuxerProgramConfigBuilder(1, 0x100).add_video(
            0x101, VideoCodec.H264
        ).add_subtitle(0x200, "not_a_config")  # type: ignore[arg-type]


# bool-as-int + bytearray-as-bytes rejection — DvbSubtitlingConfig
# -----------------------------------------------------------------------
# `bool` is a subclass of `int` in Python, so `True` / `False` would pass
# a plain `isinstance(x, int)` check. The dataclass must reject them with
# TypeError so callers get a clear signal instead of silently muxing 0/1.
# `bytearray` was previously accepted for `language` despite the dataclass
# being frozen=True, slots=True — storing a mutable reference breaks hashing
# and weakens the immutability contract.


def test_dvb_subtitling_config_rejects_bool_subtitling_type():
    """bool is not a valid int for subtitling_type — TypeError expected."""
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(TypeError, match="subtitling_type"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=True,  # bool — must be rejected
            composition_page_id=0,
            ancillary_page_id=0,
        )

    with pytest.raises(TypeError, match="subtitling_type"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=False,
            composition_page_id=0,
            ancillary_page_id=0,
        )


def test_dvb_subtitling_config_rejects_bool_composition_page_id():
    """bool is not a valid int for composition_page_id — TypeError expected."""
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(TypeError, match="composition_page_id"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=0x10,
            composition_page_id=True,
            ancillary_page_id=0,
        )


def test_dvb_subtitling_config_rejects_bool_ancillary_page_id():
    """bool is not a valid int for ancillary_page_id — TypeError expected."""
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(TypeError, match="ancillary_page_id"):
        DvbSubtitlingConfig(
            language=b"eng",
            subtitling_type=0x10,
            composition_page_id=0,
            ancillary_page_id=True,
        )


def test_dvb_subtitling_config_rejects_bytearray_language():
    """bytearray is not accepted for language; bytes only."""
    from tstrans.mpegts import DvbSubtitlingConfig

    with pytest.raises(ValueError, match="language"):
        DvbSubtitlingConfig(
            language=bytearray(b"eng"),  # was previously accepted — now rejected
            subtitling_type=0x10,
            composition_page_id=0,
            ancillary_page_id=0,
        )


# bool-as-int + bytearray-as-bytes rejection — DvbTeletextConfig
# -----------------------------------------------------------------------


def test_dvb_teletext_config_rejects_bool_teletext_type():
    """bool is not a valid int for teletext_type — TypeError expected."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(TypeError, match="teletext_type"):
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=True,
            magazine_number=1,
            page_number=0x88,
        )


def test_dvb_teletext_config_rejects_bool_magazine_number():
    """bool is not a valid int for magazine_number — TypeError expected."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(TypeError, match="magazine_number"):
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,
            magazine_number=False,
            page_number=0x88,
        )


def test_dvb_teletext_config_rejects_bool_page_number():
    """bool is not a valid int for page_number — TypeError expected."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(TypeError, match="page_number"):
        DvbTeletextConfig(
            language=b"eng",
            teletext_type=0x02,
            magazine_number=1,
            page_number=True,
        )


def test_dvb_teletext_config_rejects_bytearray_language():
    """bytearray is not accepted for language; bytes only."""
    from tstrans.mpegts import DvbTeletextConfig

    with pytest.raises(ValueError, match="language"):
        DvbTeletextConfig(
            language=bytearray(b"eng"),  # was previously accepted — now rejected
            teletext_type=0x02,
            magazine_number=1,
            page_number=0x88,
        )


# ---------------------------------------------------------------------------
# Task 9 — handle getters (video/audio/klv/subtitle × list + by_program + by_index)
# ---------------------------------------------------------------------------
#
# Rust surface coverage (verified against tst-core/src/mpegts/mux/*.rs):
#   video_*    — list + by_program + by_index
#   audio_*    — list + by_program             (no by-index getter Rust-side)
#   klv_*      — list + by_program + by_index
#   subtitle_* — list + by_program             (no by-index getter Rust-side)
#
# `*_handles_for_program` returns `Result<Vec<_>, MuxError>` in Rust and
# raises `MuxError(INVALID_USAGE)` (via the MuxErrorKind classifier
# mapping `ProgramNotFound`) on a non-existent program number; the Python
# wraps propagate that error rather than returning an empty list, so the
# call-site sees the same shape as the Rust API.


def test_video_handles_returns_one_per_video_stream():
    from tstrans.mpegts import VideoStreamHandle

    m = Muxer(_simple_config())
    h = m.video_handles()
    assert len(h) == 1
    assert isinstance(h[0], VideoStreamHandle)


def test_audio_handles_returns_one_per_audio_stream():
    from tstrans.mpegts import AudioStreamHandle

    m = Muxer(_simple_config())
    h = m.audio_handles()
    assert len(h) == 1
    assert isinstance(h[0], AudioStreamHandle)


def test_klv_handles_returns_one_per_klv_stream():
    from tstrans.mpegts import KlvStreamHandle

    m = Muxer(_simple_config())
    h = m.klv_handles()
    assert len(h) == 1
    assert isinstance(h[0], KlvStreamHandle)


def test_subtitle_handles_empty_when_no_subtitle_stream():
    # _simple_config configures no subtitle stream — list-form getter
    # returns an empty list (not an error), matching the Rust contract.
    m = Muxer(_simple_config())
    assert m.subtitle_handles() == []


def test_video_handles_for_program_returns_program_streams():
    m = Muxer(_simple_config())
    h_p1 = m.video_handles_for_program(1)
    assert len(h_p1) == 1


def test_video_handles_for_program_unknown_raises_invalid_usage():
    # Rust returns `Err(MuxError::ProgramNotFound)`, classified as
    # INVALID_USAGE by the MuxErrorKind classifier in
    # tst-core/src/error.rs. The Python wrap propagates that error.
    m = Muxer(_simple_config())
    with pytest.raises(MuxError) as ei:
        m.video_handles_for_program(99)
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


def test_audio_handles_for_program_returns_program_streams():
    m = Muxer(_simple_config())
    h_p1 = m.audio_handles_for_program(1)
    assert len(h_p1) == 1


def test_klv_handles_for_program_returns_program_streams():
    m = Muxer(_simple_config())
    h_p1 = m.klv_handles_for_program(1)
    assert len(h_p1) == 1


def test_subtitle_handles_for_program_empty_program_returns_empty():
    # Program 1 exists but has no subtitle streams — empty list, no error.
    m = Muxer(_simple_config())
    assert m.subtitle_handles_for_program(1) == []


def test_video_stream_handle_by_index_returns_handle():
    from tstrans.mpegts import VideoStreamHandle

    m = Muxer(_simple_config())
    h = m.video_stream_handle(0)
    assert h is not None
    assert isinstance(h, VideoStreamHandle)


def test_video_stream_handle_by_index_oob_returns_none():
    m = Muxer(_simple_config())
    assert m.video_stream_handle(99) is None


def test_klv_stream_handle_by_index_returns_handle():
    from tstrans.mpegts import KlvStreamHandle

    m = Muxer(_simple_config())
    h = m.klv_stream_handle(0)
    assert h is not None
    assert isinstance(h, KlvStreamHandle)


def test_klv_stream_handle_by_index_oob_returns_none():
    m = Muxer(_simple_config())
    assert m.klv_stream_handle(99) is None


def test_video_handle_round_trips_through_push_video_to():
    # End-to-end: getter → push_video_to → muxer accepts.
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_to(handle, _minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0


# ---------------------------------------------------------------------------
# Audit #9 — pts is keyword-only on every push_* method, and push_audio_to
# now takes `frames` positionally before its kw-only `pts` (normalized to
# match push_audio's `(frames, *, pts)` shape).
#
# These tests pin the Pythonic signature shape: positional `pts` MUST raise
# `TypeError` at the PyO3 argument-extraction boundary; the kwarg form MUST
# succeed.
# ---------------------------------------------------------------------------


def test_push_video_pts_positional_raises_type_error():
    """Audit #9: pts must be passed as a kwarg on push_video."""
    m = Muxer(_simple_config())
    nal = _minimal_h264_nal_aud()
    with pytest.raises(TypeError):
        m.push_video(nal, Pts90khz.from_raw(900_000), True)  # positional pts + key_frame
    # Kw form works.
    m.push_video(nal, pts=Pts90khz.from_raw(900_000), key_frame=True)
    assert m.pending_packets() > 0


def test_push_video_to_pts_positional_raises_type_error():
    """Audit #9: pts must be passed as a kwarg on push_video_to."""
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    nal = _minimal_h264_nal_aud()
    with pytest.raises(TypeError):
        m.push_video_to(handle, nal, Pts90khz.from_raw(900_000))  # positional pts
    # Kw form works.
    m.push_video_to(handle, nal, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0


def test_push_audio_pts_positional_raises_type_error():
    """Audit #9: pts must be passed as a kwarg on push_audio."""
    m = Muxer(_simple_config())
    frames = _minimal_aac_frame()
    with pytest.raises(TypeError):
        m.push_audio(frames, Pts90khz.from_raw(900_000))  # positional pts
    # Kw form works.
    m.push_audio(frames, pts=Pts90khz.from_raw(900_000))
    ref = Muxer(_simple_config())
    ref.push_audio(frames, pts=Pts90khz.from_raw(900_000))
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference audio push emitted no TS bytes"
    assert _pull_all(m) == ref_ts


def test_push_audio_to_normalized_arg_order_and_kwonly_pts():
    """Audit #9: push_audio_to is now (handle, frames, *, pts) — `frames`
    moved before `pts` to match push_audio's `(frames, *, pts)` shape; pts
    is keyword-only."""
    m = Muxer(_simple_config())
    handle = m.audio_handles()[0]
    frames = _minimal_aac_frame()
    # New normalized form works.
    m.push_audio_to(handle, frames, pts=Pts90khz.from_raw(900_000))
    ref = Muxer(_simple_config())
    ref.push_audio(frames, pts=Pts90khz.from_raw(900_000))
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference audio push emitted no TS bytes"
    assert _pull_all(m) == ref_ts
    # Old (handle, pts, frames) shape must now raise. A Pts90khz handed
    # as the second positional (where `frames` lives now) fails byte
    # extraction at the PyO3 boundary → TypeError.
    with pytest.raises(TypeError):
        m.push_audio_to(handle, Pts90khz.from_raw(900_000), frames)


def test_push_klv_pts_positional_raises_type_error():
    """Audit #9: pts must be passed as a kwarg on push_klv."""
    m = Muxer(_simple_config())
    klv = _minimal_klv_ls()
    with pytest.raises(TypeError):
        m.push_klv(klv, Pts90khz.from_raw(900_000))  # positional pts
    # Kw form works (including the default metadata_service_id).
    m.push_klv(klv, pts=Pts90khz.from_raw(900_000))
    ref = Muxer(_simple_config())
    ref.push_klv(klv, pts=Pts90khz.from_raw(900_000))
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference klv push emitted no TS bytes"
    assert _pull_all(m) == ref_ts


def test_push_klv_to_pts_positional_raises_type_error():
    """Audit #9: pts must be passed as a kwarg on push_klv_to."""
    m = Muxer(_simple_config())
    handle = m.klv_handles()[0]
    klv = _minimal_klv_ls()
    with pytest.raises(TypeError):
        m.push_klv_to(handle, klv, Pts90khz.from_raw(900_000))  # positional pts
    # Kw form works.
    m.push_klv_to(handle, klv, pts=Pts90khz.from_raw(900_000))
    ref = Muxer(_simple_config())
    ref.push_klv(klv, pts=Pts90khz.from_raw(900_000))
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference klv push emitted no TS bytes"
    assert _pull_all(m) == ref_ts


# ---------------------------------------------------------------------------
# W3 — push_data / push_data_to + data handle accessors
# ---------------------------------------------------------------------------
#
# Data streams are a PES pass-through (no AU-cell wrap, no framing, one
# push = one PES on stream_id 0xBD). The demux-side dual is
# `DemuxEvent.UnknownSample`; carries_pts=False streams re-demux with
# pts == 0 (the demuxer's no-PTS substitute).


def _data_config() -> "MuxerConfig":
    """Video + two data streams: one user-private 0xF0 with a 0xFF
    descriptor (carries_pts=True), one bare 0x06 (carries_pts=False)."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .add_data(0x1F1, 0x06, carries_pts=False)
        .stream_descriptors_for_data(0, [b"\xff\x04demo"])
        .build()
    )
    return MuxerConfigBuilder().add_program(prog).build()


def test_push_data_to_round_trips_payload_pts_and_pmt():
    """End-to-end: push payloads on both data streams, drain, demux,
    assert UnknownSample payload/pts fidelity (incl. pts==0 for the
    no-PTS stream) and PMT stream_type + descriptor bytes."""
    from tstrans.mpegts import Demuxer, DemuxerConfig, DemuxEvent, StreamKindTag

    m = Muxer(_data_config())
    handles = m.data_handles()
    assert len(handles) == 2

    payload_a = b"\x01\x02\x03\x04record-a"
    payload_b = b"\xaa\xbb\xcc"
    m.push_video(_minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    m.push_data_to(handles[0], payload_a, pts=Pts90khz.from_raw(900_000))
    m.push_data_to(handles[1], payload_b, pts=Pts90khz.from_raw(901_000))

    buf = bytearray(m.pending_packets() * 188)
    n = m.pull(buf)
    assert n > 0 and n % 188 == 0
    ts_bytes = bytes(buf[:n])

    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    events = list(d)

    # PMT advertises both data streams as Unknown with the raw
    # stream_type bytes and the verbatim descriptor loop.
    pmts = [e for e in events if isinstance(e, DemuxEvent.ProgramMap)]
    assert pmts, "expected at least one ProgramMap event"
    by_pid = {s.pid: s for s in pmts[-1].programs[0].streams}
    assert by_pid[0x1F0].kind is StreamKindTag.UNKNOWN
    assert by_pid[0x1F0].stream_type == 0xF0
    assert [(desc.tag, desc.data) for desc in by_pid[0x1F0].raw_descriptors] == [
        (0xFF, b"demo")
    ]
    assert by_pid[0x1F1].kind is StreamKindTag.UNKNOWN
    assert by_pid[0x1F1].stream_type == 0x06
    assert by_pid[0x1F1].raw_descriptors == ()

    # Payload + pts fidelity. carries_pts=False → pts comes back as 0
    # (the demuxer's no-PTS substitute), NOT the pushed 901_000.
    samples = [e for e in events if isinstance(e, DemuxEvent.UnknownSample)]
    by_stream = {s.stream.pid: s for s in samples}
    assert by_stream[0x1F0].payload == payload_a
    assert by_stream[0x1F0].pts.raw == 900_000
    assert by_stream[0x1F0].stream_type == 0xF0
    assert by_stream[0x1F1].payload == payload_b
    assert by_stream[0x1F1].pts.raw == 0
    assert by_stream[0x1F1].stream_type == 0x06


def test_data_handle_accessors_and_forged_from_raw():
    from tstrans.mpegts import DataStreamHandle

    m = Muxer(_data_config())
    handles = m.data_handles()
    assert len(handles) == 2
    assert all(isinstance(h, DataStreamHandle) for h in handles)

    # by-index getter (program 0 only) + out-of-range → None.
    h0 = m.data_stream_handle(0)
    assert h0 is not None
    assert h0 == handles[0]
    assert m.data_stream_handle(99) is None

    # by-program getter + unknown program raises INVALID_USAGE.
    assert m.data_handles_for_program(1) == handles
    with pytest.raises(MuxError) as ei:
        m.data_handles_for_program(99)
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE

    # raw round-trip + unpack.
    again = DataStreamHandle.from_raw(handles[1].raw)
    assert again == handles[1]
    assert again.unpack() == (0, 1)
    assert "DataStreamHandle" in repr(again)

    # Forged raw with bits outside the canonical 8-bit packed layout
    # rejects at from_raw itself (same contract as the other handles).
    with pytest.raises(MuxError) as ei:
        DataStreamHandle.from_raw(handles[0].raw | 0x100)
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


def test_push_data_single_stream_shorthand_works():
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .build()
    )
    m = Muxer(MuxerConfigBuilder().add_program(prog).build())
    m.push_data(b"\x42\x43", pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0


def test_push_data_ambiguous_with_two_data_streams_raises():
    m = Muxer(_data_config())
    with pytest.raises(MuxError) as ei:
        m.push_data(b"\x42", pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE
    assert "ambiguous" in str(ei.value).lower()


def test_push_data_on_video_only_muxer_raises_no_data_streams():
    prog = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
    m = Muxer(MuxerConfigBuilder().add_program(prog).build())
    with pytest.raises(MuxError) as ei:
        m.push_data(b"\x42", pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE
    assert "no data streams" in str(ei.value)


def test_push_data_too_large_payload_raises_input_malformed():
    """70_000 bytes exceeds the PES_packet_length ceiling (65527 with a
    PTS field) — DataTooLarge → INPUT_MALFORMED, with the size and the
    ceiling in the message."""
    prog = (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .build()
    )
    m = Muxer(MuxerConfigBuilder().add_program(prog).build())
    with pytest.raises(MuxError) as ei:
        m.push_data(b"\x00" * 70_000, pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INPUT_MALFORMED
    msg = str(ei.value)
    assert "70000" in msg
    assert "65527" in msg


def test_push_data_to_invalid_handle_raises_invalid_usage():
    # See test_push_video_to_with_invalid_handle_raises_invalid_usage for
    # rationale on the 0xFF (canonical-but-unconfigured) handle choice.
    from tstrans.mpegts import DataStreamHandle

    m = Muxer(_data_config())
    bogus = DataStreamHandle.from_raw(0xFF)
    with pytest.raises(MuxError) as ei:
        m.push_data_to(bogus, b"\x42", pts=Pts90khz.from_raw(900_000))
    assert ei.value.kind is MuxErrorKind.INVALID_USAGE


def test_push_data_pts_positional_raises_type_error():
    """pts is keyword-only on push_data / push_data_to (audit #9 shape)."""
    m = Muxer(_data_config())
    handle = m.data_handles()[0]
    with pytest.raises(TypeError):
        m.push_data_to(handle, b"\x42", Pts90khz.from_raw(900_000))  # positional pts
    m.push_data_to(handle, b"\x42", pts=Pts90khz.from_raw(900_000))
    ref = Muxer(_data_config())
    ref_handle = ref.data_handles()[0]
    ref.push_data_to(ref_handle, b"\x42", pts=Pts90khz.from_raw(900_000))
    ref_ts = _pull_all(ref)
    assert len(ref_ts) > 0, "reference data push emitted no TS bytes"
    assert _pull_all(m) == ref_ts


def test_push_video_to_with_dts_signature_unchanged():
    """Audit #9: push_video_to_with_dts was already kw-only — this test
    pins that the audit-9 sweep didn't accidentally regress its shape."""
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_to_with_dts(
        handle,
        _minimal_h264_nal_aud(),
        pts=Pts90khz.from_raw(990_000),
        dts=Pts90khz.from_raw(900_000),
    )
    assert m.pending_packets() > 0
    # Positional pts/dts still rejected.
    with pytest.raises(TypeError):
        m.push_video_to_with_dts(
            handle,
            _minimal_h264_nal_aud(),
            Pts90khz.from_raw(990_000),
            Pts90khz.from_raw(900_000),
        )


# ---------------------------------------------------------------------------
# DA-PY-3 settling tests — bytearray and memoryview coercion
#
# PyO3 0.22 abi3 extracts `&[u8]` only from `bytes`; the shared
# `coerce_bytes_like` helper in `util.rs` routes `bytearray`/`memoryview`
# through `bytes(arg)` so all byte-taking push_* methods accept
# buffer-protocol objects.  These tests verify the fix is wired end-to-end:
# the push must succeed AND the muxer must emit output bytes.
# ---------------------------------------------------------------------------


def _pull_all(m: Muxer) -> bytes:
    """Drain all pending TS packets from the muxer; return as bytes."""
    n_packets = m.pending_packets()
    buf = bytearray(max(n_packets, 1) * 188)
    n = m.pull(buf)
    return bytes(buf[:n])


def test_push_video_bytearray_accepted_and_emits_ts():
    """push_video must accept bytearray and produce output TS bytes."""
    m = Muxer(_simple_config())
    before = m.pending_packets()
    m.push_video(bytearray(_minimal_h264_nal_aud()), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > before, "bytearray push_video produced no output packets"
    ts = _pull_all(m)
    assert len(ts) > 0 and len(ts) % 188 == 0, "pulled TS bytes are not 188-aligned"


def test_push_video_memoryview_accepted_and_emits_ts():
    """push_video must accept memoryview and produce output TS bytes."""
    m = Muxer(_simple_config())
    before = m.pending_packets()
    m.push_video(memoryview(_minimal_h264_nal_aud()), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > before, "memoryview push_video produced no output packets"
    ts = _pull_all(m)
    assert len(ts) > 0 and len(ts) % 188 == 0, "pulled TS bytes are not 188-aligned"


def test_push_audio_bytearray_accepted_and_emits_ts():
    """push_audio must accept bytearray and produce output TS bytes."""
    m = Muxer(_simple_config())
    m.push_video(_minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    _pull_all(m)  # drain PAT/PMT/video first
    before = m.pending_packets()
    m.push_audio(bytearray(_minimal_aac_frame()), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > before, "bytearray push_audio produced no output packets"
    ts = _pull_all(m)
    assert len(ts) > 0 and len(ts) % 188 == 0, "pulled TS bytes are not 188-aligned"


def test_push_audio_memoryview_accepted_and_emits_ts():
    """push_audio must accept memoryview and produce output TS bytes."""
    m = Muxer(_simple_config())
    m.push_video(_minimal_h264_nal_aud(), pts=Pts90khz.from_raw(900_000))
    _pull_all(m)  # drain PAT/PMT/video first
    before = m.pending_packets()
    m.push_audio(memoryview(_minimal_aac_frame()), pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > before, "memoryview push_audio produced no output packets"
    ts = _pull_all(m)
    assert len(ts) > 0 and len(ts) % 188 == 0, "pulled TS bytes are not 188-aligned"


def _klv_reference_ts(payload: bytes) -> bytes:
    """Mux `payload` as a KLV push on a fresh muxer via the plain `bytes`
    path and return the drained TS output. Differential oracle for the
    coercion settling tests below: the muxer is deterministic, so a second
    fresh muxer fed the same payload through a coerced type must emit
    byte-identical TS."""
    ref = Muxer(_simple_config())
    ref.push_klv(payload, pts=Pts90khz.from_raw(900_000))
    ts = _pull_all(ref)
    # The mechanism-executed guard: an empty reference would make the
    # equality below vacuously true (empty == empty proves nothing).
    assert len(ts) > 0, "reference bytes-input KLV push emitted no TS bytes"
    assert len(ts) % 188 == 0, "reference TS bytes are not 188-aligned"
    return ts


def test_push_klv_bytearray_emits_identical_ts_to_bytes():
    """push_klv(bytearray) must produce byte-identical TS output to the
    same payload pushed as bytes (differential settling test; a bare
    `pending_packets() >= 0` check would be vacuously true)."""
    payload = _minimal_klv_ls() * 24  # >184 bytes: forces ≥1 full TS packet
    ref_ts = _klv_reference_ts(payload)
    m = Muxer(_simple_config())
    m.push_klv(bytearray(payload), pts=Pts90khz.from_raw(900_000))
    assert _pull_all(m) == ref_ts, "bytearray input produced different TS"


def test_push_klv_memoryview_emits_identical_ts_to_bytes():
    """push_klv(memoryview) must produce byte-identical TS output to the
    same payload pushed as bytes."""
    payload = _minimal_klv_ls() * 24
    ref_ts = _klv_reference_ts(payload)
    m = Muxer(_simple_config())
    m.push_klv(memoryview(payload), pts=Pts90khz.from_raw(900_000))
    assert _pull_all(m) == ref_ts, "memoryview input produced different TS"


def test_push_video_bytes_fast_path_unchanged():
    """bytes input must still work byte-for-byte (fast path must not regress)."""
    m = Muxer(_simple_config())
    nal = _minimal_h264_nal_aud()
    m.push_video(nal, pts=Pts90khz.from_raw(900_000))
    assert m.pending_packets() > 0
    ts = _pull_all(m)
    assert len(ts) > 0 and len(ts) % 188 == 0


# ---------------------------------------------------------------------------
# Task 10 — push_video_misp_to + extract_misp_timestamp round-trip
# ---------------------------------------------------------------------------


def _h264_idr_au() -> bytes:
    """Minimal Annex-B H.264 AU with an IDR slice (type 5).

    Start code + AUD (type 9) + IDR (type 5).  The IDR NAL is the first
    VCL NAL, so `push_video_misp_to` can splice the SEI before it.
    """
    # AUD: nal_unit_type=9, primary_pic_type=7 (any), RBSP trailing 0x80
    aud = b"\x00\x00\x00\x01\x09\xF0"
    # IDR: nal_unit_type=5 (IDR slice), minimal non-empty body
    idr = b"\x00\x00\x00\x01\x65\x88\x84\x00\x33\xff"
    return aud + idr


def test_push_video_misp_to_round_trips_misp_timestamp():
    """push_video_misp_to splices a MISP SEI into the AU; the muxer emits TS;
    the demuxer delivers a Video event; extract_misp_timestamp recovers the
    same timestamp."""
    from tstrans.codec import MispTimestamp, extract_misp_timestamp
    from tstrans.mpegts import Demuxer, DemuxerConfig, DemuxEvent

    ts_in = MispTimestamp.micros(0x0005_F5E1_0000_0001, 0x1F)
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_misp_to(
        handle, _h264_idr_au(), pts=Pts90khz.from_raw(900_000), misp=ts_in
    )
    assert m.pending_packets() > 0
    ts_bytes = _pull_all(m)
    assert len(ts_bytes) > 0

    d = Demuxer(DemuxerConfig())
    d.feed(ts_bytes)
    d.flush()
    video_events = [e for e in d if isinstance(e, DemuxEvent.Video)]
    assert video_events, "expected at least one Video event"

    ts_out = extract_misp_timestamp(video_events[0].raw, VideoCodec.H264)
    assert ts_out is not None, "extract returned None — MISP SEI was not found"
    assert ts_out == ts_in, f"round-trip failed: {ts_out!r} != {ts_in!r}"


def test_push_video_misp_to_nano_on_h264_raises():
    """Nano-precision is H.265-only; pushing it on an H.264 stream must
    raise MuxError(INPUT_MALFORMED) (MispTime variant)."""
    from tstrans.codec import MispTimestamp

    nano_ts = MispTimestamp.nanos(12345, 0x00)
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    with pytest.raises(MuxError) as ei:
        m.push_video_misp_to(
            handle, _h264_idr_au(), pts=Pts90khz.from_raw(900_000), misp=nano_ts
        )
    assert ei.value.kind is MuxErrorKind.MISP_TIME


def test_push_video_misp_to_with_dts():
    """push_video_misp_to accepts an optional dts kwarg and emits TS."""
    from tstrans.codec import MispTimestamp

    ts_in = MispTimestamp.micros(42, 0x00)
    m = Muxer(_simple_config())
    handle = m.video_handles()[0]
    m.push_video_misp_to(
        handle,
        _h264_idr_au(),
        pts=Pts90khz.from_raw(990_000),
        dts=Pts90khz.from_raw(900_000),
        misp=ts_in,
    )
    assert m.pending_packets() > 0
