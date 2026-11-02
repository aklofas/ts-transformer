"""A cancel is terminal for a shell's buffered output.

After `cancel_handle().cancel()` a receive, a sub-bundle send, a flush and a
finish all raise the transport's `CLOSED`, even with packets or bytes still
buffered. The H.264 receiver is the documented exception: it drains to end of
stream.
"""

from __future__ import annotations

import socket

import pytest

import tstrans
from tstrans import exceptions
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


def test_h264_receiver_drains_to_eos_after_cancel_not_closed() -> None:
    # The documented exemption: the H.264 receiver drains to end of stream.
    rx = tstrans.rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96")
    try:
        rx.cancel_handle().cancel()
        assert rx.recv_au() is None
    finally:
        rx.close()
