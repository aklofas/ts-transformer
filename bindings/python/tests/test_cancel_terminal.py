"""A cancel is terminal for a shell's buffered output.

After `cancel_handle().cancel()` a receive, a sub-bundle send, a flush and a
finish all raise the transport's `CLOSED`, even with packets or bytes still
buffered. The H.264 receiver hands out the access units it already holds
first, then raises `CLOSED`.
"""

from __future__ import annotations

import socket

import pytest

import tstrans
from tstrans import exceptions
from tstrans.exceptions import RtpError, RtpErrorKind
from tstrans.shells import MuxSender, Receiver, Sender

from _builders.mux_programs import video_only_program
from _builders.ports import free_udp_port


def _null_packet() -> bytes:
    return b"\x47\x1f\xff\x10" + b"\xff" * 184


def test_sender_cancel_then_partial_send_raises_closed() -> None:
    tx = Sender.open(f"udp://127.0.0.1:{free_udp_port()}")
    try:
        tx.cancel_handle().cancel()
        with pytest.raises(exceptions.UdpError) as e:
            tx.send_bytes(_null_packet())
        assert e.value.kind == exceptions.UdpErrorKind.CLOSED
        with pytest.raises(exceptions.UdpError) as e:
            tx.flush()
        assert e.value.kind == exceptions.UdpErrorKind.CLOSED
    finally:
        tx.close()


# `udp://` takes no recv_timeout, so the first receive is bounded by the mark.
# `method="thread"`: a receive parked in native code never runs the signal
# method's handler, so a lost datagram would hang until the job timeout.
@pytest.mark.timeout(10, method="thread")
def test_receiver_cancel_then_recv_raises_closed_not_buffered_bytes() -> None:
    port = free_udp_port()
    rx = Receiver.open(f"udp://127.0.0.1:{port}")
    try:
        # 16 packets in ONE datagram: the syncer locks after four aligned
        # packets, so one recv returns a packet and the rest stay queued.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.sendto(_null_packet() * 16, ("127.0.0.1", port))
        assert len(rx.recv_bytes()) == 188
        rx.cancel_handle().cancel()
        with pytest.raises(exceptions.UdpError) as e:
            rx.recv_bytes()
        assert e.value.kind == exceptions.UdpErrorKind.CLOSED
    finally:
        rx.close()


def test_mux_sender_cancel_then_finish_raises_closed() -> None:
    tx = MuxSender.open(f"udp://127.0.0.1:{free_udp_port()}", video_only_program())
    try:
        tx.cancel_handle().cancel()
        with pytest.raises(exceptions.UdpError) as e:
            tx.finish()
        assert e.value.kind == exceptions.UdpErrorKind.CLOSED
    finally:
        tx.close()


def _h264_pkt(seq: int, ts: int, nal: bytes, marker: bool = False) -> bytes:
    """One single-NAL-unit RTP packet, PT 96, SSRC 9."""
    return (
        bytes([0x80, (0x80 if marker else 0) | 96])
        + seq.to_bytes(2, "big")
        + ts.to_bytes(4, "big")
        + (9).to_bytes(4, "big")
        + nal
    )


def _h264_rx_and_addr(url: str) -> tuple[tstrans.rtp.H264Receiver, tuple[str, int]]:
    rx = tstrans.rtp.H264Receiver.listen(url)
    addr = rx.local_addr()
    assert addr is not None
    return rx, addr


def test_h264_receiver_ends_closed_after_cancel() -> None:
    rx = tstrans.rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96")
    try:
        rx.cancel_handle().cancel()
        for _ in range(2):
            with pytest.raises(RtpError) as e:
                rx.recv_au()
            assert e.value.kind == RtpErrorKind.CLOSED
    finally:
        rx.close()


@pytest.mark.timeout(10, method="thread")
def test_h264_receiver_hands_out_flushed_au_then_ends_closed_after_cancel() -> None:
    rx, addr = _h264_rx_and_addr("rtp://127.0.0.1:0?pt=96")
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.sendto(_h264_pkt(1, 0, b"\x65\x01"), addr)
            s.sendto(_h264_pkt(2, 3000, b"\x41\x02"), addr)
        # The second packet's timestamp completes the first AU and leaves
        # its own open, so this AU proves both packets were read.
        first = rx.recv_au(timeout_ms=5000)
        assert first is not None and first.rtp_timestamp == 0
        rx.cancel_handle().cancel()
        flushed = rx.recv_au()
        assert flushed is not None and flushed.rtp_timestamp == 3000
        assert bytes(flushed.annexb) == b"\x00\x00\x00\x01\x41\x02"
        with pytest.raises(RtpError) as e:
            rx.recv_au()
        assert e.value.kind == RtpErrorKind.CLOSED
    finally:
        rx.close()


@pytest.mark.timeout(10, method="thread")
def test_h264_receiver_iterator_raises_closed_after_cancel() -> None:
    # The URL deadline turns a lost datagram into a failure instead of a hang.
    rx, addr = _h264_rx_and_addr("rtp://127.0.0.1:0?pt=96&recv_timeout=5000")
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.sendto(_h264_pkt(1, 0, b"\x65\x01", marker=True), addr)
        collected: list[object] = []
        with pytest.raises(RtpError) as e:
            for au in rx:
                collected.append(au)
                rx.cancel_handle().cancel()
        assert e.value.kind == RtpErrorKind.CLOSED
        assert len(collected) == 1
    finally:
        rx.close()
