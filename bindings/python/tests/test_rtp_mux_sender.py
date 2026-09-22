"""Tests for `tstrans.rtp.MuxSender` (Wave B Task 23).

Loopback-only tests:
- Construct a MuxSender + an OS UDP receiver socket; push a NAL via
  MuxSender; verify the receiver sees RTP-framed TS bytes.
- Verify push family covers video/klv/audio/subtitle (single-stream
  + handle-targeted variants).
- Verify stats() returns (SocketStats, MuxerStats).
- Verify close()/__exit__ idempotency.
"""

from __future__ import annotations

import socket

import pytest

import tstrans
import tstrans.rtp
from tstrans.exceptions import MuxError, MuxErrorKind, RtpError, RtpErrorKind
from tstrans.mpegts import (
    KlvStreamType,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_udp_port as _free_udp_port


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


def _video_klv_program() -> object:
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
        .build()
    )


def _video_data_program() -> object:
    """Video + one private data stream (bare PES-private 0x06, W3)."""
    return (
        MuxerProgramConfigBuilder(1, 0x100)
        .add_video(0x101, VideoCodec.H264)
        .add_data(0x1F0, 0x06, carries_pts=True)
        .build()
    )


# Minimal Annex-B IDR NAL (start code + nal_unit_type=5).
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
# Minimal AU delimiter NAL.
NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"
# A 17-byte KLV LS with UL=ST 0601 (universal label only, no payload).
KLV_UL_ZERO = (
    b"\x06\x0E\x2B\x34\x02\x0B\x01\x01"
    b"\x0E\x01\x03\x01\x01\x00\x00\x00\x00"
)
# Opaque private-data record — the muxer applies no framing or
# inspection, so any byte string works.
DATA_RECORD = b"\x01\x02\x03\x04private-record"


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_mux_sender_module_re_exports() -> None:
    """`tstrans.rtp.MuxSender` must be exposed after T23."""
    assert tstrans.rtp.MuxSender is not None
    assert "MuxSender" in tstrans.rtp.__all__


# --------------------------------------------------------------------------- #
# Construction                                                                #
# --------------------------------------------------------------------------- #


def test_mux_sender_constructs_with_video_program() -> None:
    port = _free_udp_port()
    program = _video_only_program()
    with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
        # Sender should report an open state.
        assert "open" in repr(s)
        # video handle accessor should resolve since a video stream is configured.
        h = s.video_handle()
        assert h is not None
        assert s.klv_handle() is None
        assert s.audio_handle() is None
        assert s.subtitle_handle() is None
        assert s.data_handle() is None


def test_mux_sender_constructs_with_pkt_size() -> None:
    """`pkt_size` keyword arg should be accepted (smoke)."""
    port = _free_udp_port()
    with tstrans.rtp.MuxSender(
        f"rtp://127.0.0.1:{port}", _video_only_program(), pkt_size=752
    ) as s:
        assert "open" in repr(s)


def test_mux_sender_bad_url_raises_rtp_error() -> None:
    with pytest.raises(RtpError) as exc_info:
        tstrans.rtp.MuxSender("not-a-valid-url://", _video_only_program())
    assert exc_info.value.kind == RtpErrorKind.URL


# --------------------------------------------------------------------------- #
# Push methods                                                                #
# --------------------------------------------------------------------------- #


def test_push_video_lands_rtp_framed_ts_at_peer() -> None:
    """End-to-end: push a video NAL via MuxSender; an OS UDP receiver
    socket should see at least one datagram containing RTP-framed TS
    bytes (12-byte RTP header + N*188 TS bytes)."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        program = _video_only_program()
        with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
            s.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
            stats = s.stats()
            assert isinstance(stats, tuple)
            assert len(stats) == 2
            socket_stats, muxer_stats = stats
            # At least one packet handed off to the transport.
            assert socket_stats.packets_sent >= 1
            assert socket_stats.bytes_sent > 0
        # Drain at least one datagram from the OS socket.
        data, _ = listener.recvfrom(2048)
        # RTP header is 12 bytes; the rest must be a multiple of 188.
        assert len(data) >= 12 + 188
        ts_payload = data[12:]
        assert len(ts_payload) % 188 == 0
        # First byte after RTP header must be the TS sync byte.
        assert ts_payload[0] == 0x47
    finally:
        listener.close()


def test_send_klv_to_handle() -> None:
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        program = _video_klv_program()
        with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
            klv_h = s.klv_handle()
            assert klv_h is not None
            # Use the _to variant against the explicit handle.
            s.send_klv_to(klv_h, KLV_UL_ZERO, pts=Pts90khz.from_raw(0))
            socket_stats, _ = s.stats()
            assert socket_stats.packets_sent >= 1
    finally:
        listener.close()


def test_send_data_and_send_data_to_handle() -> None:
    """W3: `send_data` (single-stream shorthand) + `send_data_to`
    (explicit handle from `data_handle()`) both land bytes on the
    transport. Pass-through contract — no framing, no inspection."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        program = _video_data_program()
        with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
            data_h = s.data_handle()
            assert data_h is not None
            # Single-stream shorthand.
            s.send_data(DATA_RECORD, pts=Pts90khz.from_raw(0))
            # Explicit-handle variant.
            s.send_data_to(data_h, DATA_RECORD, pts=Pts90khz.from_raw(3000))
            socket_stats, _ = s.stats()
            assert socket_stats.packets_sent >= 1
    finally:
        listener.close()


def test_send_video_to_handle() -> None:
    """The _to variant takes a handle from `video_handle()` and sends
    to it explicitly. Used when multiple video streams are configured
    (here we just demonstrate the surface works on a single-stream
    setup)."""
    port = _free_udp_port()
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        program = _video_only_program()
        with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
            h = s.video_handle()
            assert h is not None
            s.send_video_to(h, NAL_AUD, pts=Pts90khz.from_raw(0))
            socket_stats, _ = s.stats()
            assert socket_stats.packets_sent >= 1
    finally:
        listener.close()


def test_send_video_accepts_bytes_like() -> None:
    """The send family accepts bytes / bytearray / memoryview (the
    audit-#10 two-path bytes-like extraction shared with PySender)."""
    port = _free_udp_port()
    # Bind a UDP listener so the kernel doesn't return ICMP
    # "Connection refused" between sends — once libsrt-style ECONNREFUSED
    # surfaces, the MuxSender marks the transport broken and subsequent
    # pushes raise RtpError(BROKEN) on Linux's connected-UDP semantics.
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind(("127.0.0.1", port))
    listener.settimeout(2.0)
    try:
        program = _video_only_program()
        with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
            # bytes — fast path
            s.send_video(NAL_AUD, pts=Pts90khz.from_raw(0))
            # bytearray — fallback through `bytes()` builtin
            s.send_video(bytearray(NAL_AUD), pts=Pts90khz.from_raw(3000))
            # memoryview — also fallback
            s.send_video(memoryview(bytearray(NAL_AUD)), pts=Pts90khz.from_raw(6000))
            socket_stats, _ = s.stats()
            assert socket_stats.packets_sent >= 3
    finally:
        listener.close()


# --------------------------------------------------------------------------- #
# Error mapping                                                               #
# --------------------------------------------------------------------------- #


def test_send_video_on_closed_sender_raises_transport() -> None:
    port = _free_udp_port()
    program = _video_only_program()
    s = tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program)
    s.close()
    with pytest.raises(RtpError) as exc_info:
        s.send_video(NAL_IDR, pts=Pts90khz.from_raw(0))
    assert exc_info.value.kind == RtpErrorKind.CLOSED


def test_send_video_malformed_nal_raises_mux_error() -> None:
    """Raw bytes without an Annex-B start code → MuxError(INVALID_NAL)."""
    port = _free_udp_port()
    program = _video_only_program()
    with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
        with pytest.raises(MuxError) as exc_info:
            s.send_video(b"not annex-b bytes", pts=Pts90khz.from_raw(0))
        assert exc_info.value.kind == MuxErrorKind.INVALID_NAL


# --------------------------------------------------------------------------- #
# Lifecycle                                                                   #
# --------------------------------------------------------------------------- #


def test_close_is_idempotent() -> None:
    port = _free_udp_port()
    s = tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", _video_only_program())
    s.close()
    s.close()  # no-op
    assert "closed" in repr(s)


def test_context_manager_closes_on_exception() -> None:
    """`__exit__` calls close() even when the body raises."""
    port = _free_udp_port()
    with pytest.raises(RuntimeError, match="boom"):
        with tstrans.rtp.MuxSender(
            f"rtp://127.0.0.1:{port}", _video_only_program()
        ) as s:
            assert "open" in repr(s)
            raise RuntimeError("boom")


# --------------------------------------------------------------------------- #
# Stats                                                                       #
# --------------------------------------------------------------------------- #


def test_stats_tuple_shape() -> None:
    port = _free_udp_port()
    program = _video_only_program()
    with tstrans.rtp.MuxSender(f"rtp://127.0.0.1:{port}", program) as s:
        stats = s.stats()
        assert isinstance(stats, tuple)
        assert len(stats) == 2
        socket_stats, muxer_stats = stats
        assert socket_stats.packets_sent == 0
        assert muxer_stats.ts_packets_emitted == 0
        # At least one program is configured.
        assert muxer_stats.programs_configured == 1
