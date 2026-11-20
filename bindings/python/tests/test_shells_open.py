"""tstrans.shells: one class per shell, opened from any transport's URL."""

from __future__ import annotations

import threading

import pytest

from tstrans import exceptions
from tstrans.mpegts import Pts90khz
from tstrans.shells import CancelHandle, DemuxReceiver, EndReason, MuxSender, Receiver, Sender

from _builders.mux_programs import video_only_program
from _builders.nal import minimal_h264_au
from _builders.ports import free_udp_port


@pytest.mark.parametrize("scheme", ["udp", "rtp"])
def test_mux_sender_to_demux_receiver_over_a_datagram_transport(scheme: str) -> None:
    port = free_udp_port()
    rx = DemuxReceiver.open(f"{scheme}://127.0.0.1:{port}")
    tx = MuxSender.open(f"{scheme}://127.0.0.1:{port}", video_only_program())
    try:
        # A shell's TS syncer needs a handful of packets to lock, and a
        # video sample is emitted when the NEXT access unit starts, so send
        # several units rather than one.
        for i in range(10):
            tx.send_video(minimal_h264_au(), pts=Pts90khz.from_raw(i * 3000), key_frame=True)
        tx.finish()
        seen = []
        for ev in rx:
            seen.append(type(ev).__name__)
            if "Video" in seen[-1]:
                break
        assert any("Video" in s for s in seen), seen
        assert isinstance(rx.cancel_handle(), CancelHandle)
    finally:
        rx.close()
        tx.close()


def test_an_unknown_scheme_is_a_value_error() -> None:
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        Sender.open("http://127.0.0.1:1")


def test_a_multibyte_char_before_a_scheme_is_a_value_error() -> None:
    with pytest.raises(ValueError, match="unsupported URL scheme") as raised:
        Sender.open("\u201csrt://u:p@h\u201d")
    message = str(raised.value)
    assert "<redacted>" in message
    assert ":p@" not in message


def test_closed_errors_are_raised_in_the_transport_domain() -> None:
    port = free_udp_port()
    tx = Sender.open(f"udp://127.0.0.1:{port}")
    tx.close()
    with pytest.raises(exceptions.UdpError) as e:
        tx.send_bytes(b"\x47" + bytes(187))
    assert e.value.kind == exceptions.UdpErrorKind.CLOSED


def test_srt_stats_is_srt_only() -> None:
    port = free_udp_port()
    tx = Sender.open(f"udp://127.0.0.1:{port}")
    with pytest.raises(TypeError, match="srt_stats"):
        tx.srt_stats()
    tx.close()


def test_end_reason_values_are_pinned() -> None:
    assert [(m.name, m.value) for m in EndReason] == [
        ("CLEAN_TEARDOWN", 1), ("SESSION_EXPIRED", 2), ("KEEPALIVE_FAILED", 3),
        ("TRANSPORT_FAILED", 4), ("PROTOCOL_ERROR", 5), ("CANCELLED", 6),
        ("END_OF_STREAM", 7), ("RECONNECT_EXHAUSTED", 8),
    ]


def _null_packets(n: int) -> bytes:
    return (bytes([0x47, 0x1F, 0xFF, 0x10]) + b"\xff" * 184) * n


def test_finish_after_a_cancel_raises_closed() -> None:
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as rx:
        rx.bind(("127.0.0.1", 0))
        tx = Sender.open(f"udp://127.0.0.1:{rx.getsockname()[1]}")
        tx.send_bytes(_null_packets(3))
        tx.cancel_handle().cancel()
        for _ in range(2):
            with pytest.raises(exceptions.UdpError) as ei:
                tx.finish()
            assert ei.value.kind == exceptions.UdpErrorKind.CLOSED
        tx.close()


def test_close_after_finish_does_nothing() -> None:
    """`finish()` sends the partial bundle `close()` would drop; the shell
    is closed after it, a second `finish()` and a `close()` do nothing,
    and a `finish()` after `close()` is quiet too."""
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as rx:
        rx.bind(("127.0.0.1", 0))
        rx.settimeout(5.0)
        tx = Sender.open(f"udp://127.0.0.1:{rx.getsockname()[1]}")
        tx.send_bytes(_null_packets(3))
        assert tx.finish() is None
        assert rx.recv(65536) == _null_packets(3)
        assert tx.is_alive() is False
        assert tx.finish() is None
        assert tx.close() is None
        with pytest.raises(exceptions.UdpError) as ei:
            tx.send_bytes(_null_packets(1))
        assert ei.value.kind == exceptions.UdpErrorKind.CLOSED
        rx.settimeout(0.2)
        with pytest.raises(socket.timeout):
            rx.recv(65536)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as rx:
        rx.bind(("127.0.0.1", 0))
        tx = Sender.open(f"udp://127.0.0.1:{rx.getsockname()[1]}")
        tx.close()
        assert tx.finish() is None
