"""Tests for `end_reason()` / `end_detail()` + `StreamEndReason` on
`tstrans.rtp.Receiver` / `DemuxReceiver` / `H264Receiver`.

Per the STANDING TEST RULE, these tests assert kinds/outcomes only — no
wall-clock duration asserts.

The deterministic scenario exercised here is `close()` -> `CANCELLED`:
every receiver class records `StreamEndReason.CANCELLED` when explicitly
closed (`RtpRecvTransport::close` / `H264Receiver::close`, both mirrored
by `Drop`). A genuine `CLEAN_TEARDOWN` requires a TCP-interleaved RTSP
peer to close its connection in an orderly way from a background
thread — no existing pytest fixture produces that scenario cheaply and
deterministically (the one full RTSP server/client loopback fixture,
`test_full_pipeline_rtsp_server_to_rtsp_client` in
test_rtp_integration.py, itself tears the receiver down via `.close()`,
which is the CANCELLED path, not a peer-initiated clean teardown), so
`close()` -> `CANCELLED` is the pinned minimum this file covers.
"""

from __future__ import annotations

import tstrans.rtp
from tstrans.rtp import StreamEndReason

from _builders.ports import free_udp_port as _free_udp_port


# --------------------------------------------------------------------------- #
# StreamEndReason enum shape                                                  #
# --------------------------------------------------------------------------- #


def test_stream_end_reason_values_are_pinned() -> None:
    """Numeric values are pinned cross-surface (C/Python/JVM); C
    additionally has `NONE = 0` — Python uses `None` for that case."""
    assert StreamEndReason.CLEAN_TEARDOWN == 1
    assert StreamEndReason.SESSION_EXPIRED == 2
    assert StreamEndReason.KEEPALIVE_FAILED == 3
    assert StreamEndReason.TRANSPORT_FAILED == 4
    assert StreamEndReason.PROTOCOL_ERROR == 5
    assert StreamEndReason.CANCELLED == 6


def test_stream_end_reason_is_int_enum() -> None:
    assert issubclass(StreamEndReason, int)
    assert "StreamEndReason" in tstrans.rtp.__all__


# --------------------------------------------------------------------------- #
# Receiver                                                                    #
# --------------------------------------------------------------------------- #


def test_receiver_end_reason_none_on_fresh_receiver() -> None:
    port = _free_udp_port()
    with tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}") as r:
        assert r.end_reason() is None
        assert r.end_detail() is None


def test_receiver_end_reason_cancelled_after_close() -> None:
    port = _free_udp_port()
    r = tstrans.rtp.Receiver(f"rtp://127.0.0.1:{port}")
    r.close()
    assert r.end_reason() == StreamEndReason.CANCELLED
    assert r.end_detail() is None
    # Idempotent — reading again after a second close doesn't change it.
    r.close()
    assert r.end_reason() == StreamEndReason.CANCELLED


# --------------------------------------------------------------------------- #
# DemuxReceiver                                                               #
# --------------------------------------------------------------------------- #


def test_demux_receiver_end_reason_none_on_fresh_receiver() -> None:
    port = _free_udp_port()
    with tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}") as rx:
        assert rx.end_reason() is None
        assert rx.end_detail() is None


def test_demux_receiver_end_reason_cancelled_after_close() -> None:
    port = _free_udp_port()
    rx = tstrans.rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}")
    rx.close()
    assert rx.end_reason() == StreamEndReason.CANCELLED
    assert rx.end_detail() is None


# --------------------------------------------------------------------------- #
# H264Receiver                                                                #
# --------------------------------------------------------------------------- #


def test_h264_receiver_end_reason_none_on_fresh_receiver() -> None:
    with tstrans.rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96") as rx:
        assert rx.end_reason() is None
        assert rx.end_detail() is None


def test_h264_receiver_end_reason_cancelled_after_close() -> None:
    rx = tstrans.rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96")
    rx.close()
    assert rx.end_reason() == StreamEndReason.CANCELLED
    assert rx.end_detail() is None
    # Idempotent — the snapshot survives a second close() call.
    rx.close()
    assert rx.end_reason() == StreamEndReason.CANCELLED
