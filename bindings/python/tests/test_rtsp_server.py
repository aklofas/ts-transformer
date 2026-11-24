"""tstrans.rtp.RtspServer / MountHandle / RtspServerConfig.

Covers:
- RtspServerConfig dataclass validation (max_sessions / fanout_capacity /
  TLS-half-pair / drain_ms invariants).
- RtspServer.start lifecycle + context-manager + local_addr.
- add_unicast_mount / add_multicast_mount round-trip.
- MountHandle push family (video / klv / audio / subtitle) — single
  stream variants accept bytes / bytearray / memoryview.
- MountHandle stats() returns a frozen ServerStats / MountStats with
  expected fields.
- cancel_handle round-trip, and what the hard cancel refuses with no stop().
- Notice 5402 graceful shutdown path doesn't panic.
"""

from __future__ import annotations

import sys
import threading

import pytest

from tstrans.exceptions import RtspError, RtspErrorKind
from tstrans.mpegts import (
    AudioCodec,
    DataStreamHandle,
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
    WebVttInTsConfig,
)
from tstrans.rtp import (
    BasicAuth,
    DigestAuth,
    MountHandle,
    MountRule,
    MountStats,
    RtspServer,
    RtspServerCancelHandle,
    RtspServerConfig,
    ServerStats,
)


# ---------------------------------------------------------------------------
# RtspServerConfig dataclass validation.
# ---------------------------------------------------------------------------


def test_rtsp_server_config_defaults():
    cfg = RtspServerConfig()
    assert cfg.bind_addr == "0.0.0.0:8554"
    assert cfg.auth is None
    assert cfg.max_sessions == 100
    assert cfg.session_timeout_secs == 60
    assert cfg.fanout_capacity == 256
    assert cfg.graceful_shutdown_drain_ms == 2000
    assert cfg.tls_cert is None
    assert cfg.tls_key is None


def test_rtsp_server_config_rejects_zero_max_sessions():
    with pytest.raises(ValueError, match="max_sessions"):
        RtspServerConfig(max_sessions=0)


def test_rtsp_server_config_rejects_negative_max_sessions():
    with pytest.raises(ValueError, match="max_sessions"):
        RtspServerConfig(max_sessions=-5)


def test_rtsp_server_config_rejects_zero_session_timeout():
    with pytest.raises(ValueError, match="session_timeout_secs"):
        RtspServerConfig(session_timeout_secs=0)


def test_rtsp_server_config_rejects_zero_fanout_capacity():
    with pytest.raises(ValueError, match="fanout_capacity"):
        RtspServerConfig(fanout_capacity=0)


def test_rtsp_server_config_rejects_negative_drain_ms():
    with pytest.raises(ValueError, match="graceful_shutdown_drain_ms"):
        RtspServerConfig(graceful_shutdown_drain_ms=-1)


def test_rtsp_server_config_rejects_tls_cert_without_key():
    with pytest.raises(ValueError, match="tls_cert and tls_key"):
        RtspServerConfig(tls_cert="/tmp/cert.pem", tls_key=None)


def test_rtsp_server_config_rejects_tls_key_without_cert():
    with pytest.raises(ValueError, match="tls_cert and tls_key"):
        RtspServerConfig(tls_cert=None, tls_key="/tmp/key.pem")


def test_rtsp_server_config_accepts_both_tls_paths_set():
    cfg = RtspServerConfig(tls_cert="/tmp/cert.pem", tls_key="/tmp/key.pem")
    assert cfg.tls_cert == "/tmp/cert.pem"
    assert cfg.tls_key == "/tmp/key.pem"


def test_rtsp_server_config_accepts_basic_auth():
    # BasicAuth is the src/rtp/client.rs PyClass — same instance
    # type used by both RtspClientConfig.auth (client-side credentials) and
    # RtspServerConfig.auth (server-side challenge). Its shape
    # is (user, password, realm=None); the `realm` to advertise on
    # WWW-Authenticate is a server-side concern (see the realm tests below).
    auth = BasicAuth(user="admin", password="hunter2")
    cfg = RtspServerConfig(auth=auth)
    assert cfg.auth is auth


def test_rtsp_server_config_accepts_digest_auth():
    # Same client-side shape: (user, password, algorithm). `algorithm`
    # is the PyDigestAlgorithm enum (MD5 / SHA256), not a string.
    from tstrans.rtp import DigestAlgorithm
    auth = DigestAuth(user="admin", password="hunter2")
    cfg = RtspServerConfig(auth=auth)
    assert cfg.auth.algorithm == DigestAlgorithm.MD5


def test_digest_auth_accepts_sha256():
    from tstrans.rtp import DigestAlgorithm
    a = DigestAuth(user="x", password="x", algorithm=DigestAlgorithm.SHA256)
    assert a.algorithm == DigestAlgorithm.SHA256


# The digest algorithm is a typed enum, so unknown strings cannot reach the
# constructor (Python raises TypeError on bad enum extract before the
# `__new__` body runs).


def test_basic_auth_realm_optional_with_default_none():
    """The realm kwarg defaults to None (client-side use) and is
    exposed via a getter."""
    a = BasicAuth(user="x", password="x")
    assert a.user == "x"
    assert a.realm is None
    b = BasicAuth(user="x", password="x", realm="protected")
    assert b.realm == "protected"


def test_digest_auth_realm_optional_with_default_none():
    a = DigestAuth(user="x", password="x")
    assert a.realm is None
    b = DigestAuth(user="x", password="x", realm="protected")
    assert b.realm == "protected"


def test_server_start_with_basic_auth_requires_realm():
    """BasicAuth without realm raises ValueError
    when used as server-side config (the realm is what the server quotes
    in WWW-Authenticate)."""
    from tstrans.rtp import RtspServer

    no_realm = BasicAuth(user="admin", password="secret")
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", auth=no_realm)
    with pytest.raises(ValueError, match="realm"):
        RtspServer.start(cfg)


def test_server_start_with_digest_auth_requires_realm():
    from tstrans.rtp import RtspServer

    no_realm = DigestAuth(user="admin", password="secret")
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", auth=no_realm)
    with pytest.raises(ValueError, match="realm"):
        RtspServer.start(cfg)


def test_server_start_with_basic_auth_and_realm_succeeds():
    """With realm provided, the server starts and stops cleanly."""
    from tstrans.rtp import RtspServer

    auth = BasicAuth(user="admin", password="secret", realm="protected")
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", auth=auth)
    s = RtspServer.start(cfg)
    s.stop()


# ---------------------------------------------------------------------------
# Helpers — build a minimal MuxerProgramConfig with one H.264 video stream.
# ---------------------------------------------------------------------------


def _single_video_program():
    return (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .build()
    )


def _video_klv_program():
    return (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .add_klv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def _video_audio_program():
    return (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .add_audio(0x1012, AudioCodec.AAC)
        .build()
    )


def test_digest_auth_client_refuses_a_basic_only_server():
    """A DigestAuth client never answers a Basic challenge."""
    from tstrans.rtp import RtspClient, RtspClientConfig

    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        auth=BasicAuth(user="admin", password="secret", realm="protected"),
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        client_cfg = RtspClientConfig(
            url=f"rtsp://127.0.0.1:{port}/live",
            auth=DigestAuth(user="admin", password="secret"),
            keepalive=False,
        )
        with pytest.raises(RtspError, match="DigestOnly"):
            RtspClient.connect(client_cfg)


# ---------------------------------------------------------------------------
# Lifecycle — start / context manager / local_addr / stop.
# ---------------------------------------------------------------------------


def test_server_start_returns_server():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", max_sessions=2)
    server = RtspServer.start(cfg)
    try:
        assert isinstance(server, RtspServer)
        addr = server.local_addr()
        assert addr is not None
        assert addr[0] == "127.0.0.1"
    finally:
        server.stop()


def test_server_context_manager_stops_on_exit():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        assert server.local_addr() is not None
    # No assertion needed — `with` exiting cleanly is the assertion.


def test_server_stats_initially_zero():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        stats = server.stats()
        assert isinstance(stats, ServerStats)
        assert stats.active_sessions == 0
        assert stats.mounts == 0
        assert stats.total_rtp_packets_sent == 0
        assert stats.total_rtp_bytes_sent == 0


def test_server_cancel_handle_round_trip():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        h1 = server.cancel_handle()
        h2 = server.cancel_handle()
        assert isinstance(h1, RtspServerCancelHandle)
        assert not h1.is_cancelled()
        h1.cancel()
        assert h2.is_cancelled()


def _tls_fixture_paths():
    import pathlib

    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    return str(d / "cert.pem"), str(d / "key.pem")


def test_server_rtsps_bind_with_certs_starts():
    """An rtsps:// bind with valid cert + key paths starts and binds —
    the TLS feature is compiled in (regression: this used to raise
    RtspError(TLS) unconditionally)."""
    cert, key = _tls_fixture_paths()
    cfg = RtspServerConfig(
        bind_addr="rtsps://127.0.0.1:0",
        tls_cert=cert,
        tls_key=key,
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(cfg) as server:
        assert server.local_addr() is not None


def test_server_tls_directory_path_raises_tls_error():
    """A directory (stat-able but not a regular file) must be rejected at
    start() — the guard opens the file, it doesn't just stat it."""
    import pathlib

    d = str(pathlib.Path(__file__).parent / "fixtures" / "tls")
    cfg = RtspServerConfig(
        bind_addr="rtsps://127.0.0.1:0",
        tls_cert=d,
        tls_key=d,
        graceful_shutdown_drain_ms=50,
    )
    with pytest.raises(RtspError) as exc_info:
        RtspServer.start(cfg)
    assert exc_info.value.kind == RtspErrorKind.TLS


def test_server_tls_missing_cert_file_raises_tls_error():
    """Nonexistent cert/key paths surface as RtspError(TLS) at start()
    (the builder reads the files at build time)."""
    cfg = RtspServerConfig(
        bind_addr="rtsps://127.0.0.1:0",
        tls_cert="/nonexistent/cert.pem",
        tls_key="/nonexistent/key.pem",
        graceful_shutdown_drain_ms=50,
    )
    with pytest.raises(RtspError) as exc_info:
        RtspServer.start(cfg)
    assert exc_info.value.kind == RtspErrorKind.TLS


# ---------------------------------------------------------------------------
# add_unicast_mount + add_multicast_mount.
# ---------------------------------------------------------------------------


def test_add_unicast_mount_returns_handle():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        mount = server.add_unicast_mount("/live", program)
        assert isinstance(mount, MountHandle)
        assert mount.mount_path() == "/live"
        assert mount.mount_kind() == "unicast"
        assert server.stats().mounts == 1


def test_add_unicast_mount_rejects_empty_path():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        with pytest.raises(RtspError) as exc_info:
            server.add_unicast_mount("", program)
        assert exc_info.value.kind == RtspErrorKind.MOUNT


def test_add_unicast_mount_rejects_path_without_leading_slash():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        with pytest.raises(RtspError) as exc_info:
            server.add_unicast_mount("live", program)
        assert exc_info.value.kind == RtspErrorKind.MOUNT


def test_add_unicast_mount_rejects_duplicate_path():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        server.add_unicast_mount("/live", program)
        with pytest.raises(RtspError) as exc_info:
            server.add_unicast_mount("/live", _single_video_program())
        assert exc_info.value.kind == RtspErrorKind.MOUNT


def test_add_multicast_mount_returns_handle():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        mount = server.add_multicast_mount(
            "/mc", "239.0.0.1", 5004, ttl=1, program_config=program
        )
        assert mount.mount_path() == "/mc"
        assert mount.mount_kind() == "multicast"


def test_add_multicast_mount_with_iface():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        mount = server.add_multicast_mount(
            "/mc", "239.0.0.1", 5004, ttl=2, iface="127.0.0.1",
            program_config=program,
        )
        assert mount.mount_path() == "/mc"


def test_add_multicast_mount_rejects_unicast_group():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        program = _single_video_program()
        with pytest.raises(RtspError) as exc_info:
            server.add_multicast_mount(
                "/mc", "10.0.0.1", 5004, program_config=program
            )
        assert exc_info.value.kind == RtspErrorKind.MOUNT


# ---------------------------------------------------------------------------
# MountHandle push family.
# ---------------------------------------------------------------------------


# Annex-B IDR NAL: `00 00 00 01 65 BB` — type 5 (IDR) + filler payload.
# Reliably produces a drained TS chunk on first push.
_IDR_NAL = bytes([0x00, 0x00, 0x00, 0x01, 0x65, 0xBB])


def test_push_video_succeeds_with_no_peers():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        # Pre-PLAY: no peers; push should still succeed (muxer consumes,
        # broadcast::send returns Err which the Rust side suppresses).
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)


def test_push_video_accepts_bytearray():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video(bytearray(_IDR_NAL), pts=Pts90khz(0), key_frame=True)


def test_push_video_accepts_memoryview():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video(
            memoryview(_IDR_NAL), pts=Pts90khz(0), key_frame=True
        )


def test_push_video_updates_mount_stats():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        initial = mount.stats()
        assert initial.bytes_pushed == 0
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
        after = mount.stats()
        assert after.bytes_pushed > 0
        assert after.packets_pushed >= 1


def test_push_klv_succeeds():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_klv_program())
        # Minimal KLV: 16-byte UL + 1-byte length(0).
        klv = b"\x06\x0e\x2b\x34\x02\x0b\x01\x01\x0e\x01\x03\x01\x01\x00\x00\x00\x00"
        mount.push_klv(klv, pts=Pts90khz(0))


def test_push_audio_succeeds():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_audio_program())
        # Smallest plausible ADTS header: syncword + payload. AAC parser
        # will likely reject, but the call must marshal correctly through
        # GIL release.
        adts = bytes([0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x3F, 0xFC]) + bytes(20)
        try:
            mount.push_audio(adts, pts=Pts90khz(0))
        except RtspError as e:
            # Codec parse may reject; that's a MOUNT-mapped error and
            # confirms the path works end-to-end.
            assert e.kind == RtspErrorKind.MOUNT


# ---------------------------------------------------------------------------
# Handle getters.
# ---------------------------------------------------------------------------


def test_video_handle_returns_for_configured_program():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        h = mount.video_handle()
        assert h is not None


def test_klv_handle_returns_none_for_video_only_program():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        assert mount.klv_handle() is None
        assert mount.audio_handle() is None
        assert mount.subtitle_handle() is None


def test_push_video_with_dts_on_one_video_stream():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video_with_dts(
            _IDR_NAL, pts=Pts90khz(18000), dts=Pts90khz(0), key_frame=True
        )
        assert mount.stats().bytes_pushed > 0


def test_push_video_with_dts_with_two_video_streams_raises_mount():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    two = (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .add_video(0x1012, VideoCodec.H264)
        .build()
    )
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", two)
        with pytest.raises(RtspError) as ei:
            mount.push_video_with_dts(_IDR_NAL, pts=Pts90khz(3000), dts=Pts90khz(0))
        assert ei.value.kind == RtspErrorKind.MOUNT


def test_push_video_to_with_explicit_handle():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        h = mount.video_handle()
        assert h is not None
        mount.push_video_to(h, _IDR_NAL, pts=Pts90khz(0), key_frame=True)


# ---------------------------------------------------------------------------
# ServerStats + MountStats dataclass shape.
# ---------------------------------------------------------------------------


def test_mount_stats_has_expected_fields():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        s = mount.stats()
        assert isinstance(s, MountStats)
        for attr in ("bytes_pushed", "packets_pushed", "peer_count", "frames_dropped_total"):
            assert hasattr(s, attr)
        assert s.peer_count == 0
        assert s.frames_dropped_total == 0


def test_server_stats_has_expected_fields():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        s = server.stats()
        for attr in (
            "active_sessions",
            "mounts",
            "total_rtp_packets_sent",
            "total_rtp_bytes_sent",
        ):
            assert hasattr(s, attr)


# ---------------------------------------------------------------------------
# stop() + Notice 5402 path.
# ---------------------------------------------------------------------------


def test_stop_is_idempotent():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    server = RtspServer.start(cfg)
    server.stop()
    server.stop()  # No-op; must not raise.


def test_stop_after_mount_does_not_panic():
    # Notice 5402 path runs over each active session; with zero sessions
    # the loop body is skipped but the cancel cascade still fires. The
    # graceful_shutdown_drain_ms=50 keeps the test fast.
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    server = RtspServer.start(cfg)
    server.add_unicast_mount("/live", _single_video_program())
    server.stop()


# ---------------------------------------------------------------------------
# The hard cancel: cancel_handle().cancel() with no stop().
# ---------------------------------------------------------------------------


def test_push_after_the_hard_cancel_is_closed():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
        before = mount.stats().bytes_pushed
        server.cancel_handle().cancel()
        with pytest.raises(RtspError) as ei:
            mount.push_video(_IDR_NAL, pts=Pts90khz(3000), key_frame=True)
        assert ei.value.kind == RtspErrorKind.CLOSED
        # Refused before the muxer is touched.
        assert mount.stats().bytes_pushed == before


def test_registration_after_the_hard_cancel_is_closed():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        server.cancel_handle().cancel()
        for call in (
            lambda: server.add_unicast_mount("/other", _single_video_program()),
            lambda: server.remove_mount("/live"),
        ):
            with pytest.raises(RtspError) as ei:
                call()
            assert ei.value.kind == RtspErrorKind.CLOSED


def test_rule_edits_after_the_hard_cancel_are_closed_but_len_answers():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        rules = server.mount_rules()
        rules.set("/a", MountRule())
        server.cancel_handle().cancel()
        assert rules.is_closed()
        with pytest.raises(RtspError) as ei:
            rules.set("/b", MountRule())
        assert ei.value.kind == RtspErrorKind.CLOSED
        assert len(rules) == 1


def test_stop_after_the_hard_cancel_returns():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    server = RtspServer.start(cfg)
    try:
        server.add_unicast_mount("/live", _single_video_program())
        server.cancel_handle().cancel()
    finally:
        server.stop()
    server.stop()


# ---------------------------------------------------------------------------
# flush + reset_stats.
# ---------------------------------------------------------------------------


def test_flush_is_noop_on_empty_mount():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.flush()  # must not raise


def test_reset_stats_zeros_counters():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
        assert mount.stats().bytes_pushed > 0
        mount.reset_stats()
        assert mount.stats().bytes_pushed == 0
        assert mount.stats().packets_pushed == 0


# ---------------------------------------------------------------------------
# push_data / push_data_to / data_handle on MountHandle.
# ---------------------------------------------------------------------------


def _video_data_program():
    """Video + one private data stream (user-private stream_type 0xF0)."""
    return (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .add_data(0x1F0, 0xF0, carries_pts=True)
        .build()
    )


_DATA_RECORD = b"\x01\x02\x03\x04private-mount-data"


def test_data_handle_returns_none_for_video_only_program():
    """data_handle() returns None when no data stream is configured."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        assert mount.data_handle() is None


def test_data_handle_returns_handle_for_data_program():
    """data_handle() returns a DataStreamHandle when a data stream is
    configured in the program."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_data_program())
        h = mount.data_handle()
        assert h is not None
        assert isinstance(h, DataStreamHandle)
        # data_handles() (list form) mirrors the other *_handles() accessors.
        handles = mount.data_handles()
        assert len(handles) == 1
        assert all(isinstance(x, DataStreamHandle) for x in handles)


def test_push_data_succeeds_with_no_peers():
    """push_data to a mount with no connected peers succeeds (broadcast
    no-receivers path; muxer consumes the payload, fanout is suppressed)."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_data_program())
        mount.push_data(_DATA_RECORD, pts=Pts90khz(0))


def test_push_data_to_with_explicit_handle():
    """push_data_to with an explicit DataStreamHandle succeeds."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_data_program())
        h = mount.data_handle()
        assert h is not None
        mount.push_data_to(h, _DATA_RECORD, pts=Pts90khz(0))


def test_push_data_updates_mount_stats():
    """push_data contributes to bytes_pushed (same accounting as push_video)."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_data_program())
        # Push a key-frame so the muxer can emit its first TS packet set
        # (muxer holds back until it can emit a PCR-bearing adaptation).
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
        mount.push_data(_DATA_RECORD, pts=Pts90khz(3000))
        assert mount.stats().bytes_pushed > 0


def test_push_data_accepts_bytearray():
    """push_data accepts bytearray (bytes-like coercion path)."""
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _video_data_program())
        mount.push_data(bytearray(_DATA_RECORD), pts=Pts90khz(0))


# ---------------------------------------------------------------------------
# Handle getters answer without the mount's muxer.
# ---------------------------------------------------------------------------


def _every_kind_program():
    return (
        MuxerProgramConfigBuilder(1, 0x1000)
        .add_video(0x1011, VideoCodec.H264)
        .add_video(0x1012, VideoCodec.H264)
        .add_klv(0x1013, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .add_audio(0x1014, AudioCodec.AAC)
        .add_subtitle(0x1015, WebVttInTsConfig())
        .add_data(0x1016, 0xF0, carries_pts=True)
        .build()
    )


def _handle_getters(mount):
    """Raw values of all ten handle getters, keyed by getter name."""
    out = {}
    for kind in ("video", "klv", "audio", "subtitle", "data"):
        one = getattr(mount, f"{kind}_handle")()
        out[f"{kind}_handle"] = None if one is None else one.raw
        out[f"{kind}_handles"] = [h.raw for h in getattr(mount, f"{kind}_handles")()]
    return out


def test_handle_getters_match_a_muxer_of_the_same_program():
    program = _every_kind_program()
    muxer = Muxer(MuxerConfigBuilder().add_program(program).build())
    expected = {}
    for kind in ("video", "klv", "audio", "subtitle", "data"):
        raws = [h.raw for h in getattr(muxer, f"{kind}_handles")()]
        expected[f"{kind}_handle"] = raws[0] if raws else None
        expected[f"{kind}_handles"] = raws
    assert len(expected["video_handles"]) == 2
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", program)
        assert _handle_getters(mount) == expected
        mount.push_video_to(
            mount.video_handles()[1], _IDR_NAL, pts=Pts90khz(0), key_frame=True
        )
        assert _handle_getters(mount) == expected
        multicast = server.add_multicast_mount(
            "/mc", "239.0.0.1", 5004, program_config=program
        )
        assert _handle_getters(multicast) == expected
        server.remove_mount("/live")
        assert _handle_getters(mount) == expected
    assert _handle_getters(mount) == expected


@pytest.mark.timeout(60)
def test_handle_getters_do_not_wait_for_a_push_in_progress():
    # Each round starts one push on a second thread and, as soon as that
    # push has released the GIL, calls all ten handle getters, then reads
    # `bytes_pushed`, which a push updates at the end of its work while it
    # still holds the mount's muxer lock. A getter that waits for that lock
    # can only return once the push's count is in, so an unchanged count
    # proves the getters did not wait for the push.
    #
    # With the switch interval pinned, the pushing thread gives up the GIL
    # only inside the push, so when `in_push.wait()` returns the push has
    # begun. What this cannot show alone: a round on a loaded host where
    # this thread is held up for longer than a whole push (about 1 to 3 ms
    # here) also reads a changed count, hence the majority over the rounds.
    rounds = 40
    frame = b"\x00\x00\x01\x0c\xff" * (300 * 1024)
    go = threading.Event()
    in_push = threading.Event()
    done = threading.Event()
    stop = False
    errors = []
    unchanged = 0
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        expected = _handle_getters(mount)

        def pusher():
            i = 0
            while True:
                go.wait()
                go.clear()
                if stop:
                    return
                in_push.set()
                try:
                    mount.push_video(frame, pts=Pts90khz(i * 3000), key_frame=True)
                except Exception as e:  # noqa: BLE001 - reported below
                    errors.append(e)
                i += 1
                done.set()

        t = threading.Thread(target=pusher, daemon=True)
        t.start()
        prev = sys.getswitchinterval()
        sys.setswitchinterval(3600.0)
        try:
            for _ in range(rounds):
                before = mount.stats().bytes_pushed
                in_push.clear()
                done.clear()
                go.set()
                assert in_push.wait(10.0)
                got = _handle_getters(mount)
                after = mount.stats().bytes_pushed
                assert got == expected
                if after == before:
                    unchanged += 1
                assert done.wait(10.0)
        finally:
            sys.setswitchinterval(prev)
            stop = True
            go.set()
            t.join(10.0)
    assert not errors, errors
    assert mount.stats().bytes_pushed > 0
    assert unchanged > rounds // 2, (
        f"the handle getters returned before the push finished in only "
        f"{unchanged} of {rounds} rounds: they wait for the muxer lock"
    )


def test_server_local_addr_is_a_tuple():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        addr = server.local_addr()
        assert isinstance(addr, tuple)
        host, port = addr
        assert host == "127.0.0.1"
        assert isinstance(port, int) and port > 0


# ---------------------------------------------------------------------------
# UDP media and the Digest answer without `qop`: what the config's options
# make the server answer on the wire.
# ---------------------------------------------------------------------------


def _read_head(sock) -> str:
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        assert chunk, "the server closed before answering"
        buf += chunk
    return buf.decode(errors="replace")


def _status_of(head: str) -> int:
    return int(head.split()[1])


def _wrap_tls(sock):
    import ssl

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx.wrap_socket(sock)


def _reader_setup(port: int, transport: str, *, tls: bool = False) -> str:
    """One reader SETUP of `/live` asking for `transport`, on a fresh
    connection; returns the response head."""
    import socket

    scheme = "rtsps" if tls else "rtsp"
    raw = socket.create_connection(("127.0.0.1", port), timeout=5.0)
    with _wrap_tls(raw) if tls else raw as sock:
        sock.sendall(
            (
                f"SETUP {scheme}://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\n"
                f"CSeq: 1\r\nTransport: {transport}\r\n\r\n"
            ).encode()
        )
        return _read_head(sock)


def _udp_transport() -> str:
    import socket

    # Only the port number goes into the request; nothing is sent to it.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(("127.0.0.1", 0))
        p = udp.getsockname()[1]
    return f"RTP/AVP;unicast;client_port={p}-{p + 1}"


_INTERLEAVED = "RTP/AVP/TCP;unicast;interleaved=0-1"


def _transport_of(head: str) -> str:
    from conftest import rtsp_header

    return rtsp_header(head, "Transport")


def test_config_udp_media_and_digest_options_default():
    cfg = RtspServerConfig()
    assert cfg.allow_udp_media is None
    assert cfg.accept_digest_without_qop is False
    with pytest.raises(TypeError, match="allow_udp_media"):
        RtspServerConfig(allow_udp_media=1)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="accept_digest_without_qop"):
        RtspServerConfig(accept_digest_without_qop=None)  # type: ignore[arg-type]


def test_a_server_without_udp_media_answers_461_to_a_udp_setup():
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0", allow_udp_media=False, graceful_shutdown_drain_ms=50
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        refused = _reader_setup(port, _udp_transport())
        assert _status_of(refused) == 461, refused
        assert "server_port" not in refused
        assert _status_of(_reader_setup(port, _INTERLEAVED)) == 200


def test_a_plain_server_allows_udp_media_by_default():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        head = _reader_setup(server.local_addr()[1], _udp_transport())
        assert _status_of(head) == 200, head
        assert "server_port" in _transport_of(head)


def test_an_rtsps_server_refuses_udp_media_unless_allowed():
    cert, key = _tls_fixture_paths()
    for allow, want in ((None, 461), (True, 200)):
        cfg = RtspServerConfig(
            bind_addr="rtsps://127.0.0.1:0",
            tls_cert=cert,
            tls_key=key,
            allow_udp_media=allow,
            graceful_shutdown_drain_ms=50,
        )
        with RtspServer.start(cfg) as server:
            server.add_unicast_mount("/live", _single_video_program())
            port = server.local_addr()[1]
            head = _reader_setup(port, _udp_transport(), tls=True)
            assert _status_of(head) == want, (allow, head)
            assert _status_of(_reader_setup(port, _INTERLEAVED, tls=True)) == 200


def _rfc2069_describe(accept_digest_without_qop: bool) -> tuple[str, str]:
    """A DESCRIBE without a credential, then the same DESCRIBE answered in
    the RFC 2069 form (no `qop`, `nc` or `cnonce`), as GStreamer 1.24 answers
    a Digest MD5 challenge. Returns the challenge and the answered head."""
    import hashlib
    import socket

    from conftest import rtsp_header

    realm = "lab"
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        auth=DigestAuth(user="admin", password="secret", realm=realm),
        accept_digest_without_qop=accept_digest_without_qop,
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        uri = f"rtsp://127.0.0.1:{port}/live"
        with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
            sock.sendall(f"DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\n\r\n".encode())
            first = _read_head(sock)
            assert _status_of(first) == 401, first
            challenge = rtsp_header(first, "WWW-Authenticate")
            nonce = challenge.split('nonce="', 1)[1].split('"', 1)[0]

            def md5(s: str) -> str:
                return hashlib.md5(s.encode()).hexdigest()

            ha1 = md5(f"admin:{realm}:secret")
            ha2 = md5(f"DESCRIBE:{uri}")
            response = md5(f"{ha1}:{nonce}:{ha2}")
            sock.sendall(
                (
                    f"DESCRIBE {uri} RTSP/1.0\r\nCSeq: 2\r\n"
                    f'Authorization: Digest username="admin", realm="{realm}", '
                    f'nonce="{nonce}", uri="{uri}", response="{response}"\r\n\r\n'
                ).encode()
            )
            return challenge, _read_head(sock)


def test_an_answer_without_qop_is_accepted_only_with_the_option():
    challenge, answered = _rfc2069_describe(True)
    assert 'qop="auth"' in challenge
    assert _status_of(answered) == 200, answered
    challenge, answered = _rfc2069_describe(False)
    assert 'qop="auth"' in challenge
    assert _status_of(answered) == 401, answered


# ---------------------------------------------------------------------------
# What start() refuses, and a mount handle after its server or its mount
# ended.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bind_addr",
    [
        "admin:secret@127.0.0.1:0",
        "rtsp://admin:secret@127.0.0.1:0",
        "rtsp://admin@127.0.0.1:0",
        "rtsps://admin:secret@127.0.0.1:0",
    ],
)
def test_a_bind_address_with_userinfo_does_not_start(bind_addr):
    kw = {}
    if bind_addr.startswith("rtsps://"):
        kw["tls_cert"], kw["tls_key"] = _tls_fixture_paths()
    cfg = RtspServerConfig(bind_addr=bind_addr, graceful_shutdown_drain_ms=50, **kw)
    with pytest.raises(RtspError) as ei:
        RtspServer.start(cfg)
    assert ei.value.kind == RtspErrorKind.CONFIG_INVALID
    text = str(ei.value)
    assert "invalid server config" in text
    assert "bind URL" in text
    assert "admin" not in text and "secret" not in text


@pytest.mark.parametrize(
    "bind_addr, what",
    [
        ("127.0.0.1:0/live", "path"),
        ("rtsp://127.0.0.1:0/live", "path"),
        ("rtsps://127.0.0.1:0/live", "path"),
        ("127.0.0.1:0?transport=tcp", "query"),
        ("rtsp://127.0.0.1:0?transport=udp", "query"),
        ("rtsps://127.0.0.1:0?transport=udp", "query"),
        ("rtsp://127.0.0.1:0?", "query"),
    ],
)
def test_a_bind_address_with_a_path_or_a_query_does_not_start(bind_addr, what):
    kw = {}
    if bind_addr.startswith("rtsps://"):
        kw["tls_cert"], kw["tls_key"] = _tls_fixture_paths()
    cfg = RtspServerConfig(bind_addr=bind_addr, graceful_shutdown_drain_ms=50, **kw)
    with pytest.raises(RtspError) as ei:
        RtspServer.start(cfg)
    assert ei.value.kind == RtspErrorKind.CONFIG_INVALID
    text = str(ei.value)
    assert "bind URL" in text
    assert what in text


def test_a_bind_address_with_an_unknown_query_key_is_a_url_error():
    # The parse refuses it before the server's own check of the bind URL.
    cfg = RtspServerConfig(
        bind_addr="rtsp://127.0.0.1:0?mount=live", graceful_shutdown_drain_ms=50
    )
    with pytest.raises(RtspError) as ei:
        RtspServer.start(cfg)
    assert ei.value.kind == RtspErrorKind.PROTOCOL


def test_a_bind_address_with_a_bare_slash_starts():
    cfg = RtspServerConfig(bind_addr="rtsp://127.0.0.1:0/", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        assert server.local_addr()[1] != 0


def test_a_basic_auth_user_name_with_a_colon_does_not_start():
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        auth=BasicAuth(user="a:b", password="secret", realm="r"),
        graceful_shutdown_drain_ms=50,
    )
    with pytest.raises(RtspError) as ei:
        RtspServer.start(cfg)
    assert ei.value.kind == RtspErrorKind.CONFIG_INVALID
    assert "secret" not in str(ei.value)


def test_flush_after_the_server_ended_is_closed():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    server = RtspServer.start(cfg)
    mount = server.add_unicast_mount("/live", _single_video_program())
    mount.flush()
    server.stop()
    with pytest.raises(RtspError) as ei:
        mount.flush()
    assert ei.value.kind == RtspErrorKind.CLOSED
    mount.reset_stats()


def test_flush_after_the_hard_cancel_is_closed():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        server.cancel_handle().cancel()
        with pytest.raises(RtspError) as ei:
            mount.flush()
        assert ei.value.kind == RtspErrorKind.CLOSED


def test_a_removed_mounts_handle_is_closed_and_its_getters_answer():
    cfg = RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50)
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
        pushed = mount.stats().bytes_pushed
        server.remove_mount("/live")
        for call in (
            lambda: mount.push_video(_IDR_NAL, pts=Pts90khz(3000), key_frame=True),
            mount.flush,
        ):
            with pytest.raises(RtspError) as ei:
                call()
            assert ei.value.kind == RtspErrorKind.CLOSED
        assert mount.mount_path() == "/live"
        assert mount.stats().bytes_pushed == pushed
        assert mount.video_handle() is not None
        # The path is free again, and the new mount's handle pushes.
        again = server.add_unicast_mount("/live", _single_video_program())
        again.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)


def _describe(port: int, authorization: str | None = None) -> str:
    import socket

    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        auth = f"Authorization: {authorization}\r\n" if authorization else ""
        sock.sendall(
            f"DESCRIBE rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 1\r\n{auth}\r\n".encode()
        )
        return _read_head(sock)


def _basic(user: str, password: str) -> str:
    import base64

    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


def test_set_challenge_takes_effect_with_a_config_credential():
    from conftest import rtsp_header

    from tstrans.rtp import AuthScheme

    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        auth=DigestAuth(user="admin", password="secret", realm="realm"),
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        assert rtsp_header(_describe(port), "WWW-Authenticate").startswith("Digest")
        server.mount_rules().set_challenge(AuthScheme.BASIC, "lab")
        challenge = rtsp_header(_describe(port), "WWW-Authenticate")
        assert challenge.startswith("Basic") and 'realm="lab"' in challenge
        # The config's credential is checked in the new scheme.
        assert _status_of(_describe(port, _basic("admin", "secret"))) == 200
        assert _status_of(_describe(port, _basic("admin", "wrong"))) == 401


def test_a_config_credential_with_an_empty_password_admits_only_it():
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0",
        auth=BasicAuth(user="admin", password="", realm="r"),
        graceful_shutdown_drain_ms=50,
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        assert _status_of(_describe(port)) == 401
        assert _status_of(_describe(port, _basic("admin", "x"))) == 401
        assert _status_of(_describe(port, _basic("admin", ""))) == 200


# ---------------------------------------------------------------------------
# A multicast mount where UDP media is off; the reader cap; this library's
# client on rtsps://.
# ---------------------------------------------------------------------------


def test_a_multicast_mount_is_refused_where_udp_media_is_off():
    cert, key = _tls_fixture_paths()
    tls = {"tls_cert": cert, "tls_key": key}
    for bind, extra, refused in (
        ("127.0.0.1:0", {"allow_udp_media": False}, True),
        ("rtsps://127.0.0.1:0", tls, True),
        ("rtsps://127.0.0.1:0", {**tls, "allow_udp_media": True}, False),
    ):
        cfg = RtspServerConfig(bind_addr=bind, graceful_shutdown_drain_ms=50, **extra)
        with RtspServer.start(cfg) as server:
            add = lambda: server.add_multicast_mount(  # noqa: E731
                "/mc", "239.0.0.1", 5004, program_config=_single_video_program()
            )
            if refused:
                with pytest.raises(RtspError) as ei:
                    add()
                assert ei.value.kind == RtspErrorKind.MOUNT, (bind, extra)
                assert server.stats().mounts == 0
            else:
                assert add().mount_kind() == "multicast"


def test_config_max_readers():
    assert RtspServerConfig().max_readers is None
    assert RtspServerConfig(max_readers=0).max_readers == 0
    with pytest.raises(ValueError, match="max_readers"):
        RtspServerConfig(max_readers=-1)
    with pytest.raises(TypeError, match="max_readers"):
        RtspServerConfig(max_readers=True)  # type: ignore[arg-type]
    # Too large for the server, refused at start() like max_sessions.
    for field in ("max_sessions", "max_readers"):
        cfg = RtspServerConfig(bind_addr="127.0.0.1:0", **{field: 2**70})
        with pytest.raises(OverflowError):
            RtspServer.start(cfg)


def _session_id(head: str) -> str:
    from conftest import rtsp_header

    return rtsp_header(head, "Session").split(";", 1)[0]


def test_a_reader_over_the_cap_is_refused_and_one_under_it_plays():
    import socket

    from tstrans.rtp import RtspClient, RtspClientConfig

    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0", max_readers=1, graceful_shutdown_drain_ms=50
    )
    with RtspServer.start(cfg) as server:
        mount = server.add_unicast_mount("/live", _single_video_program())
        port = server.local_addr()[1]
        uri = f"rtsp://127.0.0.1:{port}/live"
        with socket.create_connection(("127.0.0.1", port), timeout=5.0) as reader:
            reader.sendall(
                f"SETUP {uri}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n"
                f"Transport: {_INTERLEAVED}\r\n\r\n".encode()
            )
            setup = _read_head(reader)
            assert _status_of(setup) == 200, setup
            reader.sendall(
                f"PLAY {uri} RTSP/1.0\r\nCSeq: 2\r\n"
                f"Session: {_session_id(setup)}\r\n\r\n".encode()
            )
            buf = b""
            while b"\r\n\r\n" not in buf:
                chunk = reader.recv(4096)
                assert chunk, "the server closed before answering PLAY"
                buf += chunk
            head, _, rest = buf.partition(b"\r\n\r\n")
            assert _status_of(head.decode()) == 200, head
            mount.push_video(_IDR_NAL, pts=Pts90khz(0), key_frame=True)
            # The reader under the cap is sent the media, interleaved.
            while b"$" not in rest:
                chunk = reader.recv(4096)
                assert chunk, "the server closed instead of sending media"
                rest += chunk

            # A second reader is over the cap: 453, then the connection ends.
            with socket.create_connection(("127.0.0.1", port), timeout=5.0) as over:
                over.sendall(
                    f"SETUP {uri}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n"
                    f"Transport: {_INTERLEAVED}\r\n\r\n".encode()
                )
                refused = _read_head(over)
                assert _status_of(refused) == 453, refused
                tail = b""
                while True:
                    try:
                        chunk = over.recv(4096)
                    except ConnectionResetError:
                        break
                    if not chunk:
                        break
                    tail += chunk
                assert b"RTSP/1.0" not in tail

            # This library's client over the cap reads the 453 as PROTOCOL.
            client_cfg = RtspClientConfig(url=uri, keepalive=False)
            with pytest.raises(RtspError) as ei:
                RtspClient.connect(client_cfg)
            assert ei.value.kind == RtspErrorKind.PROTOCOL
            assert "453" in str(ei.value)


def test_max_readers_zero_refuses_every_reader():
    cfg = RtspServerConfig(
        bind_addr="127.0.0.1:0", max_readers=0, graceful_shutdown_drain_ms=50
    )
    with RtspServer.start(cfg) as server:
        server.add_unicast_mount("/live", _single_video_program())
        head = _reader_setup(server.local_addr()[1], _INTERLEAVED)
        assert _status_of(head) == 453, head


def _scripted_rtsps_peer(cert: str, key: str):
    """A one-connection `rtsps://` peer that answers DESCRIBE with an MP2T
    SDP and every SETUP with 461, and records each SETUP's Transport.
    Returns (port, recorded transports, thread)."""
    import socket
    import ssl

    from conftest import rtsp_header

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    listener = socket.create_server(("127.0.0.1", 0))
    listener.settimeout(10.0)
    port = listener.getsockname()[1]
    transports: list[str] = []
    sdp = (
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n"
        "m=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\na=control:trackID=0\r\n"
    )

    def serve() -> None:
        with listener:
            conn, _ = listener.accept()
        with ctx.wrap_socket(conn, server_side=True) as tls:
            tls.settimeout(10.0)
            buf = b""
            while True:
                while b"\r\n\r\n" not in buf:
                    chunk = tls.recv(4096)
                    if not chunk:
                        return
                    buf += chunk
                head, _, buf = buf.partition(b"\r\n\r\n")
                req = head.decode(errors="replace") + "\r\n"
                method = req.split(" ", 1)[0]
                cseq = rtsp_header(req, "CSeq")
                if method == "DESCRIBE":
                    reply = (
                        f"RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n"
                        f"Content-Type: application/sdp\r\n"
                        f"Content-Length: {len(sdp)}\r\n\r\n{sdp}"
                    )
                elif method == "SETUP":
                    transports.append(rtsp_header(req, "Transport"))
                    reply = f"RTSP/1.0 461 Unsupported Transport\r\nCSeq: {cseq}\r\n\r\n"
                else:
                    reply = f"RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n"
                tls.sendall(reply.encode())

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return port, transports, t


def test_an_rtsps_pull_asks_for_interleaved_by_default():
    import pathlib

    from tstrans.rtp import RtspClient, RtspClientConfig, TransportPref

    cert, key = _tls_fixture_paths()
    pem = pathlib.Path(cert).read_bytes()
    for extra, want in (
        ({}, ["RTP/AVP/TCP"]),
        ({"transport_pref": TransportPref.UDP}, ["RTP/AVP;"]),
    ):
        port, transports, t = _scripted_rtsps_peer(cert, key)
        cfg = RtspClientConfig(
            url=f"rtsps://127.0.0.1:{port}/live",
            tls_root_certs_pem=pem,
            keepalive=False,
            **extra,
        )
        with pytest.raises(RtspError) as ei:
            RtspClient.connect(cfg)
        assert "461" in str(ei.value)
        t.join(10.0)
        assert not t.is_alive()
        # One SETUP, and no second one over another transport.
        assert len(transports) == 1, transports
        assert transports[0].startswith(want[0]), transports
        if not extra:
            assert "interleaved" in transports[0]
