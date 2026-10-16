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
