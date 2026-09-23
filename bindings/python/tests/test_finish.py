"""`MuxSender.finish()` parity (Arc 2 R3 / DEBT-14): drain, report, close.

`close()` is cancel-first and abandons whatever the muxer still holds;
`finish()` is the lossless counterpart — it drains to the live transport,
raises the first drain error, then closes. These tests pin the three
observable halves of that contract on all three Python mux-sender classes:

* `finish()` returns `None` on a healthy sender and leaves it closed
  (`is_alive()` is `False` where the class exposes it),
* a second `finish()` is quiet (no raise), and
* a send after `finish()` raises the transport's `CLOSED` kind.

The loopback fixtures are the ones `test_cross_thread_close.py` already
uses: a listener-mode `srt.Receiver` that never reads (so libsrt's own send
buffer absorbs the payload and the drain has nothing to report), and a bound
UDP sink for RTP (so a connected sender never sees ECONNREFUSED). Nothing
here parks, so no test needs a watchdog.
"""

from __future__ import annotations

import socket
import threading
import time

import pytest

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port

NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"


def _srt_listener_peer(port: int):
    """Listener-mode `srt.Receiver` accepted on a side thread.

    Mirrors `test_cross_thread_close._srt_mux_sender_with_silent_peer`, but
    returns the accept box so the caller can build either mux-sender class
    against it.
    """
    import tstrans.srt as srt

    box: list[object] = []
    errs: list[BaseException] = []

    def accept_worker() -> None:
        try:
            box.append(srt.Receiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    return box, errs, t


def test_srt_mux_sender_finish_closes_and_is_idempotent() -> None:
    import tstrans.srt as srt
    from tstrans.exceptions import SrtError, SrtErrorKind
    from tstrans.mpegts import Pts90khz

    port = _free_tcp_port()
    box, errs, t = _srt_listener_peer(port)
    tx = srt.MuxSender.from_url(
        f"srt://127.0.0.1:{port}?mode=caller", _video_only_program()
    )
    t.join(5.0)
    if errs or not box:
        tx.close()
        pytest.fail(f"srt listener did not accept: {errs!r}")

    try:
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
        assert tx.finish() is None
        assert not tx.is_alive()
        assert tx.finish() is None  # second finish: quiet
        with pytest.raises(SrtError) as ei:
            tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(3000), key_frame=True)
        assert ei.value.kind == SrtErrorKind.CLOSED
    finally:
        tx.close()
        box[0].close()


def test_srt_mux_sender_finish_after_close_is_quiet() -> None:
    """`finish()` on an already-`close()`d sender is a no-op, not a raise.

    `close()` empties the handle slot, so the binding sees
    `HandleState::Closed` rather than a live shell — the same "already
    finished" answer a second `finish()` gets from the shell itself.
    """
    import tstrans.srt as srt

    port = _free_tcp_port()
    box, errs, t = _srt_listener_peer(port)
    tx = srt.MuxSender.from_url(
        f"srt://127.0.0.1:{port}?mode=caller", _video_only_program()
    )
    t.join(5.0)
    if errs or not box:
        tx.close()
        pytest.fail(f"srt listener did not accept: {errs!r}")

    try:
        tx.close()
        assert tx.finish() is None
    finally:
        box[0].close()


def test_srt_managed_mux_sender_finish_closes() -> None:
    import tstrans.srt as srt
    from tstrans.exceptions import SrtError, SrtErrorKind
    from tstrans.mpegts import Pts90khz

    port = _free_tcp_port()
    box, errs, t = _srt_listener_peer(port)
    tx = srt.ManagedMuxSender.from_url(
        f"srt://127.0.0.1:{port}?mode=caller", _video_only_program()
    )
    t.join(5.0)
    if errs or not box:
        tx.close()
        pytest.fail(f"srt listener did not accept: {errs!r}")

    try:
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
        assert tx.finish() is None
        assert not tx.is_alive()
        assert tx.finish() is None
        with pytest.raises(SrtError) as ei:
            tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(3000), key_frame=True)
        assert ei.value.kind == SrtErrorKind.CLOSED
    finally:
        tx.close()
        box[0].close()


def test_rtp_mux_sender_finish_closes() -> None:
    import tstrans.rtp as rtp
    from tstrans.exceptions import RtpError, RtpErrorKind
    from tstrans.mpegts import Pts90khz

    sink = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sink.bind(("127.0.0.1", 0))
    port = sink.getsockname()[1]
    try:
        tx = rtp.MuxSender(f"rtp://127.0.0.1:{port}", _video_only_program())
        try:
            tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
            assert tx.finish() is None
            assert tx.finish() is None
            with pytest.raises(RtpError) as ei:
                tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(3000), key_frame=True)
            assert ei.value.kind == RtpErrorKind.CLOSED
        finally:
            tx.close()
    finally:
        sink.close()
