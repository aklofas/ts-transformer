"""Tests for `tstrans.hls` (Plan A5b Wave C).

Covers:
- Publisher ABC: cannot instantiate; subclass-method enforcement.
- HlsPublisher: build (bind 127.0.0.1:0, TemporaryDirectory output_dir),
  push_ts, cut_segment, finish → assert .ts + .m3u8 on disk.
- MuxPublisher: with_config_hls → send_video / send_klv →
  finish_into_publisher → finish; KLV preserved in the .ts segment.
- HlsMode enum + HlsStats / PublisherStats / MuxPublisherStats dataclasses.
- Error cases: unaligned push, ops-after-finish, bad bind, bad URL.
- Error mapping wiring: every HlsErrorKind round-trips through Rust.

All filesystem output uses tempfile.TemporaryDirectory (never /tmp).
"""

from __future__ import annotations

import glob
import os
import tempfile
import urllib.request

import pytest

# The `hls` cargo feature is default-on and ships in published wheels. This
# skip only fires for a `--no-default-features` source build; a missing
# `tstrans.hls` in a default build means an under-built wheel — investigate.
pytest.importorskip(
    "tstrans.hls",
    reason="tstrans.hls missing = under-built wheel (or --no-default-features build); investigate.",
    exc_type=ImportError,
)

import tstrans
import tstrans.hls
from tstrans import _native
from tstrans.exceptions import HlsError, HlsErrorKind
from tstrans.hls import (
    HlsMode,
    HlsPublisher,
    HlsPublisherBuilder,
    HlsServerHandle,
    HlsStats,
    MuxPublisher,
    MuxPublisherStats,
    Publisher,
    PublisherStats,
)
from tstrans.mpegts import (
    KlvStreamType,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

# 376 bytes = 2 × 188-byte TS packets (aligned).
TS_TWO_PACKETS = b"\x47" + b"\x00" * 187 + b"\x47" + b"\x00" * 187
# Minimal Annex-B IDR NAL (start code + nal_unit_type=5 + a byte).
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
# A 24-byte KLV LS: ST 0601 UL (16 bytes) + a tiny body. We assert these
# raw bytes survive into the .ts PES payload (after the 5-byte AU-cell wrap
# the muxer prepends, the UL itself is unchanged).
KLV_ST0601 = (
    b"\x06\x0e\x2b\x34\x02\x0b\x01\x01"
    b"\x0e\x01\x03\x01\x01\x00\x00\x00"
    b"\x06\x01\x00\x02\x00\x00\x00\x01"
)


def _video_program(pid_video: int = 0x101) -> object:
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(pid_video, VideoCodec.H264)
        .build()
    )


def _video_klv_program() -> object:
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_module_re_exports() -> None:
    for name in (
        "Publisher",
        "PublisherStats",
        "HlsPublisher",
        "HlsPublisherBuilder",
        "MuxPublisher",
        "MuxPublisherStats",
        "HlsMode",
        "HlsStats",
        "HlsError",
        "HlsErrorKind",
    ):
        assert name in tstrans.hls.__all__
        assert getattr(tstrans.hls, name) is not None


# --------------------------------------------------------------------------- #
# Publisher ABC (T10)                                                         #
# --------------------------------------------------------------------------- #


def test_publisher_abc_cannot_instantiate() -> None:
    with pytest.raises(TypeError):
        Publisher()


def test_publisher_subclass_missing_methods_rejected() -> None:
    class Incomplete(Publisher):
        # Missing cut_segment / finish / stats — abc enforces all four.
        def push_ts(self, b: object) -> None:  # noqa: ANN001
            pass

    with pytest.raises(TypeError):
        Incomplete()


def test_publisher_complete_subclass_instantiates() -> None:
    class Complete(Publisher):
        def push_ts(self, b: object) -> None:  # noqa: ANN001
            pass

        def cut_segment(self) -> None:
            pass

        def finish(self) -> None:
            pass

        def stats(self) -> object:
            return PublisherStats(0, 0, None, None)

    inst = Complete()
    inst.push_ts(b"")
    assert isinstance(inst.stats(), PublisherStats)


def test_hls_publisher_is_virtual_subclass_of_publisher() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = (
            HlsPublisher.builder()
            .bind("127.0.0.1:0")
            .output_dir(d)
            .build()
        )
        try:
            assert isinstance(pub, Publisher)
        finally:
            pub.finish()


# --------------------------------------------------------------------------- #
# HlsMode + stats dataclasses (T13)                                           #
# --------------------------------------------------------------------------- #


def test_hls_mode_enum() -> None:
    assert HlsMode.LIVE != HlsMode.EVENT
    assert HlsMode.LIVE != HlsMode.VOD
    assert HlsMode.EVENT != HlsMode.VOD


def test_publisher_stats_fields() -> None:
    s = PublisherStats(3, 1234, 500, 1_000_000)
    assert s.segments_written == 3
    assert s.bytes_written == 1234
    assert s.current_segment_age_us == 500
    assert s.last_segment_duration_us == 1_000_000
    # Optional duration fields default to None.
    s2 = PublisherStats(0, 0)
    assert s2.current_segment_age_us is None
    assert s2.last_segment_duration_us is None


def test_hls_stats_fields() -> None:
    s = HlsStats(2, 752, 0, 0)
    assert s.segments_written == 2
    assert s.bytes_pushed_total == 752
    assert s.open_segment_bytes == 0
    assert s.forced_cuts == 0


# --------------------------------------------------------------------------- #
# HlsPublisher direct push (T12)                                              #
# --------------------------------------------------------------------------- #


def test_hls_publisher_push_cut_finish_writes_files() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = (
            HlsPublisher.builder()
            .bind("127.0.0.1:0")
            .output_dir(d)
            .segment_duration_ms(1000)
            .playlist_window(3)
            .mode(HlsMode.LIVE)
            .build()
        )
        # local_addr / local_port should reflect the OS-assigned port.
        addr = pub.local_addr()
        assert addr is not None and addr.startswith("127.0.0.1:")
        assert pub.local_port() > 0

        pub.push_ts(TS_TWO_PACKETS)
        pub.cut_segment()
        pub.push_ts(TS_TWO_PACKETS)
        pub.cut_segment()

        stats = pub.stats()
        assert isinstance(stats, PublisherStats)
        assert stats.segments_written == 2
        assert stats.bytes_written == 2 * len(TS_TWO_PACKETS)

        hstats = pub.hls_stats()
        assert isinstance(hstats, HlsStats)
        assert hstats.segments_written == 2

        playlist = pub.render_playlist(False)
        assert "#EXTM3U" in playlist

        pub.finish()

        # finish writes playlist.m3u8 + the .ts segments to disk.
        assert os.path.exists(os.path.join(d, "playlist.m3u8"))
        segs = sorted(glob.glob(os.path.join(d, "*.ts")))
        assert segs, "no .ts segments written"


def test_hls_publisher_ops_after_finish_raise_finished() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(d).build()
        pub.finish()
        with pytest.raises(HlsError) as ei:
            pub.push_ts(TS_TWO_PACKETS)
        assert ei.value.kind == HlsErrorKind.FINISHED
        # close() is idempotent — no raise even after finish.
        pub.close()


def test_hls_publisher_unaligned_push_rejected() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(d).build()
        try:
            with pytest.raises(HlsError) as ei:
                pub.push_ts(b"\x47" * 187)  # not a multiple of 188
            assert ei.value.kind == HlsErrorKind.UNALIGNED_PUSH_TS
        finally:
            pub.finish()


def test_hls_builder_bad_bind_raises_value_error() -> None:
    with pytest.raises(ValueError):
        HlsPublisher.builder().bind("not-an-addr").build()


def test_hls_builder_from_url_bad_scheme_raises_hls_error_url() -> None:
    with pytest.raises(HlsError) as ei:
        HlsPublisher.builder().from_url("rtsp://example.com:8000")
    assert ei.value.kind == HlsErrorKind.URL


# --------------------------------------------------------------------------- #
# MuxPublisher (T11)                                                          #
# --------------------------------------------------------------------------- #


def test_mux_publisher_video_then_finish_into_publisher() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(d).build()
        program = _video_program()
        mp = MuxPublisher.with_config_hls(pub, program)

        # The source HlsPublisher handle is now consumed.
        with pytest.raises(HlsError) as ei:
            pub.push_ts(TS_TWO_PACKETS)
        assert ei.value.kind == HlsErrorKind.FINISHED

        for i in range(3):
            mp.send_video(NAL_IDR, pts=Pts90khz.from_raw(i * 90_000), key_frame=True)

        mstats = mp.stats()
        assert isinstance(mstats, MuxPublisherStats)
        assert mstats.bytes_pushed > 0
        # Keyframes *begin* segments (send_video cuts BEFORE pushing a
        # keyframe): the stream-head keyframe opens segment 0 without a
        # spurious zero-duration cut, and the 2nd + 3rd keyframes each cut.
        # So 3 keyframes → 2 cuts (segment 2 stays open until finish).
        assert mstats.cut_calls >= 2

        pstats = mp.publisher_stats()
        assert isinstance(pstats, PublisherStats)

        recovered = mp.finish_into_publisher()
        assert isinstance(recovered, HlsPublisher)
        recovered.finish()

        assert os.path.exists(os.path.join(d, "playlist.m3u8"))
        segs = sorted(glob.glob(os.path.join(d, "*.ts")))
        assert segs

        # Shell is consumed — a second finish raises CLOSED
        # (`MuxPublisherError::Closed`; was FINISHED before 0.7.0).
        with pytest.raises(HlsError) as ei2:
            mp.finish_into_publisher()
        assert ei2.value.kind == HlsErrorKind.CLOSED


def test_mux_publisher_klv_preserved_in_ts_segment() -> None:
    with tempfile.TemporaryDirectory() as d:
        pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir(d).build()
        program = _video_klv_program()
        mp = MuxPublisher.with_config_hls(pub, program)

        mp.send_klv(KLV_ST0601, pts=Pts90khz.from_raw(0))
        mp.send_video(NAL_IDR, pts=Pts90khz.from_raw(90_000), key_frame=True)
        mp.cut_segment()
        recovered = mp.finish_into_publisher()
        recovered.finish()

        segs = sorted(glob.glob(os.path.join(d, "*.ts")))
        assert segs, "no segments produced"
        with open(segs[0], "rb") as f:
            seg_bytes = f.read()
        # Raw KLV UL passes through into the PES payload unchanged (the
        # 5-byte AU-cell header is prepended *before* the LS bytes, so the
        # KLV bytes appear verbatim as a substring).
        assert KLV_ST0601 in seg_bytes, "KLV bytes not found in segment"


# --------------------------------------------------------------------------- #
# Error-mapping wiring (T14)                                                  #
# --------------------------------------------------------------------------- #


def test_hls_error_kind_count() -> None:
    assert len(HlsErrorKind) == 10


@pytest.mark.parametrize(
    "kind_name",
    [
        "URL",
        "IO",
        "BIND_FAILED",
        "INVALID_CONFIG",
        "UNALIGNED_PUSH_TS",
        "FINISHED",
        "TLS_DISABLED",
        "TLS",
        "INTERNAL",
        "CLOSED",
    ],
)
def test_hls_error_round_trips_from_rust(kind_name: str) -> None:
    """Every HlsErrorKind variant maps through the one raise path in Rust."""
    with pytest.raises(HlsError) as ei:
        _native._raise_hls_error_for_test(kind_name, f"test {kind_name}")
    assert ei.value.kind == getattr(HlsErrorKind, kind_name)
    assert kind_name in ei.value.message


# --------------------------------------------------------------------------- #
# Media-derived EXTINF (T15)                                                  #
# --------------------------------------------------------------------------- #


def test_extinf_is_media_derived() -> None:
    """MuxPublisher derives #EXTINF from PTS span, not wall-clock time."""
    with tempfile.TemporaryDirectory() as d:
        pub = (
            HlsPublisher.builder()
            .bind("127.0.0.1:0")
            .output_dir(d)
            .segment_duration_ms(1000)
            .playlist_window(3)
            .mode(HlsMode.EVENT)
            .build()
        )
        program = _video_program()
        mp = MuxPublisher.with_config_hls(pub, program)

        # Keyframes *begin* segments (send_video cuts BEFORE a keyframe):
        # PTS 0: stream-head keyframe — opens segment 0 (no spurious cut),
        #   baselining its start PTS at 0.
        mp.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
        # PTS 90000: non-keyframe — appended to segment 0.
        mp.send_video(NAL_IDR, pts=Pts90khz.from_raw(90000), key_frame=False)
        # PTS 261000: keyframe — cuts segment 0 (span 0..261000 = 261000 ticks
        #   = 2.9 s at 90 kHz), then opens segment 1. 2.900 s is well past the
        #   1.0 s target, so it can only be the media-derived (PTS-span) EXTINF.
        mp.send_video(NAL_IDR, pts=Pts90khz.from_raw(261000), key_frame=True)

        hls = mp.finish_into_publisher()
        pl = hls.render_playlist(False)
        hls.finish()

        assert "#EXTINF:2.900," in pl, pl


def test_hls_publisher_cut_with_duration() -> None:
    """HlsPublisher.cut_segment_with_duration records the given µs as #EXTINF."""
    with tempfile.TemporaryDirectory() as d:
        pub = (
            HlsPublisher.builder()
            .bind("127.0.0.1:0")
            .output_dir(d)
            .segment_duration_ms(1000)
            .playlist_window(3)
            .mode(HlsMode.EVENT)
            .build()
        )
        import tstrans.hls as _hls_mod

        assert isinstance(pub, _hls_mod.Publisher)
        pub.push_ts(TS_TWO_PACKETS)
        pub.cut_segment_with_duration(3_200_000)  # 3.2 s in µs
        pl = pub.render_playlist(False)
        pub.finish()

        assert "#EXTINF:3.200," in pl, pl


# --------------------------------------------------------------------------- #
# finish_serving + HlsServerHandle (Task 14)                                  #
# --------------------------------------------------------------------------- #


def test_finish_serving_serves_vod(tmp_path: object) -> None:
    """finish_serving keeps the HTTP server up; the terminal VOD playlist is
    fetchable and carries #EXT-X-ENDLIST."""
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .mode(HlsMode.VOD)
        .build()
    )
    pub.push_ts(b"\x47" + b"\x00" * 187)
    pub.cut_segment()
    with pub.finish_serving() as handle:
        assert isinstance(handle, HlsServerHandle)
        port = handle.local_port()
        assert port > 0
        assert handle.local_addr().startswith("127.0.0.1:")
        url = f"http://127.0.0.1:{port}/playlist.m3u8"
        with urllib.request.urlopen(url) as resp:  # noqa: S310
            body = resp.read().decode()
        assert "#EXT-X-ENDLIST" in body, body


def test_hls_https_serving_with_tls(tmp_path: object) -> None:
    """HTTPS (hlss) end-to-end: enable_tls(cert, key) serves the playlist
    over TLS; a stdlib ssl context trusting the fixture cert fetches and
    fully verifies it (the fixture has an IP:127.0.0.1 SAN). Regression:
    enable_tls used to raise HlsError(TLS_DISABLED) because the wheels
    were built without the tls feature."""
    import pathlib
    import ssl

    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    cert, key = str(d / "cert.pem"), str(d / "key.pem")
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .mode(HlsMode.VOD)
        .enable_tls(cert, key)
        .build()
    )
    pub.push_ts(b"\x47" + b"\x00" * 187)
    pub.cut_segment()
    with pub.finish_serving() as handle:
        port = handle.local_port()
        ctx = ssl.create_default_context(cafile=cert)
        url = f"https://127.0.0.1:{port}/playlist.m3u8"
        with urllib.request.urlopen(url, context=ctx) as resp:  # noqa: S310
            body = resp.read().decode()
        assert "#EXT-X-ENDLIST" in body, body


def test_forced_cuts_stat_default_zero(tmp_path: object) -> None:
    """A fresh publisher reports forced_cuts == 0."""
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .build()
    )
    try:
        assert pub.hls_stats().forced_cuts == 0
    finally:
        pub.finish()


def test_max_segment_duration_ms_builder(tmp_path: object) -> None:
    """The max_segment_duration_ms builder setter builds OK (non-zero cap)."""
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .segment_duration_ms(4000)
        .max_segment_duration_ms(8000)
        .build()
    )
    try:
        assert isinstance(pub, HlsPublisher)
    finally:
        pub.finish()


def test_max_segment_duration_ms_zero_leaves_default(tmp_path: object) -> None:
    """Passing 0 leaves the library default (no reset-to-None); builds OK."""
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .max_segment_duration_ms(0)
        .build()
    )
    try:
        assert isinstance(pub, HlsPublisher)
    finally:
        pub.finish()


def test_server_handle_shutdown_is_idempotent(tmp_path: object) -> None:
    """Explicit shutdown() works and a following context-exit is a no-op."""
    pub = (
        HlsPublisher.builder()
        .bind("127.0.0.1:0")
        .output_dir(str(tmp_path))
        .mode(HlsMode.VOD)
        .build()
    )
    pub.push_ts(b"\x47" + b"\x00" * 187)
    pub.cut_segment()
    handle = pub.finish_serving()
    handle.shutdown()
    handle.close()  # alias; idempotent no-op after shutdown
