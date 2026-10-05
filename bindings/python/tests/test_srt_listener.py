"""Tests for `tstrans.srt.Listener`.

Listener has both blocking accept(timeout_ms=...) and an iterator
(__iter__/__next__) shape. Tests cover:
- explicit accept-with-timeout (TIMEOUT path),
- local_addr port readback after kernel-pick,
- cancel_handle().cancel() from another thread unblocks accept,
- iterator yields Sockets until cancel,
- context-manager close on __exit__.
"""

from __future__ import annotations

import threading
import time
from typing import List, Optional

import pytest

import tstrans
import tstrans.srt
from tstrans.exceptions import SrtError, SrtErrorKind

from _builders.ports import free_tcp_port as _free_tcp_port


def _connect_caller(port: int) -> tstrans.srt.Socket:
    """Drive a Builder.connect() in caller mode against the loopback port."""
    return tstrans.srt.Builder(f"srt://127.0.0.1:{port}").connect()


# --------------------------------------------------------------------------- #
# Module surface                                                              #
# --------------------------------------------------------------------------- #


def test_listener_is_exported() -> None:
    assert tstrans.srt.Listener is not None
    assert tstrans.srt.Socket is not None
    assert "Listener" in tstrans.srt.__all__
    assert "Socket" in tstrans.srt.__all__


# --------------------------------------------------------------------------- #
# accept(timeout_ms=...) — TIMEOUT path                                       #
# --------------------------------------------------------------------------- #


def test_accept_with_timeout_raises_timeout() -> None:
    """No peer connects within 100 ms → SrtError(TIMEOUT)."""
    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    with b.listen() as lst:
        t_start = time.monotonic()
        with pytest.raises(SrtError) as exc_info:
            lst.accept(timeout_ms=100)
        elapsed = time.monotonic() - t_start
        assert exc_info.value.kind == SrtErrorKind.TIMEOUT
        # Should return promptly (≤ 5 s slack for slow CI).
        assert elapsed < 5.0


# --------------------------------------------------------------------------- #
# local_addr() port readback                                                  #
# --------------------------------------------------------------------------- #


def test_local_addr_returns_kernel_picked_port() -> None:
    """Binding with port=0 lets the kernel pick; local_addr must
    surface the chosen non-zero port."""
    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    with b.listen() as lst:
        host, port = lst.local_addr()
        assert port != 0
        assert isinstance(port, int)
        assert host == "0.0.0.0"


# --------------------------------------------------------------------------- #
# accept() returns a connected Socket on peer connect                         #
# --------------------------------------------------------------------------- #


def test_accept_returns_socket_on_peer_connect() -> None:
    """Pair a Listener.accept() with a caller-side connect; assert
    both Sockets are alive after the handshake."""
    port = _free_tcp_port()
    listener_b = tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
    lst = listener_b.listen()

    accepted_box: list[Optional[tstrans.srt.Socket]] = [None]
    err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            accepted_box[0] = lst.accept(timeout_ms=5000)
        except BaseException as exc:  # noqa: BLE001
            err.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    caller = _connect_caller(port)
    t.join(timeout=5.0)

    try:
        if err:
            raise err[0]
        assert accepted_box[0] is not None
        assert accepted_box[0].is_alive()
        assert caller.is_alive()
    finally:
        caller.close()
        if accepted_box[0] is not None:
            accepted_box[0].close()
        lst.close()


# --------------------------------------------------------------------------- #
# cancel_handle().cancel() unblocks accept()                                  #
# --------------------------------------------------------------------------- #


def test_cancel_handle_unblocks_accept() -> None:
    """Park a thread in `accept()` (no timeout). From another thread,
    call `cancel_handle().cancel()`. The parked accept must wake within
    a few seconds with SrtError(CLOSED)."""
    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    lst = b.listen()
    handle = lst.cancel_handle()

    err_box: list[Optional[BaseException]] = [None]
    started = threading.Event()
    done = threading.Event()

    def accept_worker() -> None:
        started.set()
        try:
            lst.accept()
        except BaseException as exc:  # noqa: BLE001
            err_box[0] = exc
        finally:
            done.set()

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    assert started.wait(timeout=2.0)
    # Give the accept syscall time to park.
    time.sleep(0.2)
    handle.cancel()
    assert done.wait(timeout=5.0)
    assert err_box[0] is not None
    assert isinstance(err_box[0], SrtError)
    # SRT close path raises CLOSED (not StopIteration here — that mapping
    # is iterator-only).
    assert err_box[0].kind == SrtErrorKind.CLOSED
    assert handle.is_cancelled()
    lst.close()


# --------------------------------------------------------------------------- #
# Iterator: for sock in listener                                              #
# --------------------------------------------------------------------------- #


def test_iterator_yields_sockets_until_cancel() -> None:
    """Iterator yields accepted Sockets until cancel triggers
    StopIteration. We connect one caller, observe one Socket, then
    cancel and confirm the for-loop terminates."""
    port = _free_tcp_port()
    b = tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
    lst = b.listen()
    handle = lst.cancel_handle()

    collected: List[tstrans.srt.Socket] = []
    loop_done = threading.Event()
    loop_err: list[BaseException] = []

    def loop_worker() -> None:
        try:
            for sock in lst:
                collected.append(sock)
                if len(collected) == 1:
                    # After the first accept, schedule a cancel from
                    # another thread (not this one — we're inside the
                    # iterator).
                    threading.Timer(0.2, handle.cancel).start()
        except BaseException as exc:  # noqa: BLE001
            loop_err.append(exc)
        finally:
            loop_done.set()

    t = threading.Thread(target=loop_worker, daemon=True)
    t.start()
    time.sleep(0.1)

    caller = _connect_caller(port)
    # Wait for the loop to terminate (it should hit StopIteration after cancel).
    assert loop_done.wait(timeout=5.0)
    try:
        if loop_err:
            raise loop_err[0]
        assert len(collected) >= 1
    finally:
        caller.close()
        for s in collected:
            s.close()
        lst.close()


# --------------------------------------------------------------------------- #
# Context manager closes on __exit__                                          #
# --------------------------------------------------------------------------- #


def test_context_manager_closes_on_exit() -> None:
    """`with Builder(url).listen() as lst:` must close the listener on
    exit. After exit, is_alive() is False."""
    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    captured: list[tstrans.srt.Listener] = []
    with b.listen() as lst:
        captured.append(lst)
        assert lst.is_alive()
    # After the with-block, the listener is closed.
    assert not captured[0].is_alive()


# --------------------------------------------------------------------------- #
# Socket promotion: into_sender / into_receiver consume the handle           #
# --------------------------------------------------------------------------- #


def test_socket_into_sender_consumes_handle() -> None:
    """`Socket.into_sender()` consumes the inner handle; subsequent
    `into_receiver()` raises SrtError(CLOSED)."""
    port = _free_tcp_port()
    listener_b = tstrans.srt.Builder(f"srt://0.0.0.0:{port}?mode=listener")
    lst = listener_b.listen()

    accepted_box: list[Optional[tstrans.srt.Socket]] = [None]

    def accept_worker() -> None:
        accepted_box[0] = lst.accept(timeout_ms=5000)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    caller = _connect_caller(port)
    t.join(timeout=5.0)

    try:
        sock = caller
        sender = sock.into_sender()
        assert sender is not None
        # Re-using the consumed Socket raises CLOSED.
        with pytest.raises(SrtError) as exc_info:
            sock.into_receiver()
        assert exc_info.value.kind == SrtErrorKind.CLOSED
        sender.close()
    finally:
        if accepted_box[0] is not None:
            accepted_box[0].close()
        lst.close()


def test_socket_into_demux_receiver_consumes_socket() -> None:
    """`Socket.into_demux_receiver()` consumes the socket (a real
    implementation, not a NotImplementedError stub). After consumption,
    the original socket handle reports closed.

    The mux-side promotion is covered by
    `test_srt_mux_demux.py::test_socket_into_mux_sender_promotion`.
    """
    from tstrans.mpegts import (
        MuxerProgramConfigBuilder,
        VideoCodec,
    )

    b = tstrans.srt.Builder("srt://0.0.0.0:0?mode=listener")
    with b.listen() as lst:
        port = lst.local_addr()[1]
        accepted_box: list[Optional[tstrans.srt.Socket]] = [None]

        def accept_worker() -> None:
            accepted_box[0] = lst.accept(timeout_ms=5000)

        t = threading.Thread(target=accept_worker, daemon=True)
        t.start()
        time.sleep(0.1)
        caller = _connect_caller(port)
        t.join(timeout=5.0)
        try:
            # Demote the listener-accepted socket into a DemuxReceiver.
            assert accepted_box[0] is not None
            rx = accepted_box[0].into_demux_receiver()
            # The original Socket handle is now consumed.
            assert not accepted_box[0].is_alive()
            assert "open" in repr(rx)
            rx.close()
            # Mux-side promotion: take a fresh socket pair.
            program = (
                MuxerProgramConfigBuilder(1, 0x100)
                .add_video(0x101, VideoCodec.H264)
                .build()
            )
            # The caller-side socket from above is still alive; promote it.
            tx = caller.into_mux_sender(program)
            assert not caller.is_alive()
            assert "open" in repr(tx)
            tx.close()
        finally:
            # Idempotent close of already-consumed handles.
            caller.close()
            if accepted_box[0] is not None:
                accepted_box[0].close()
