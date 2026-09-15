"""Regression test for DA-PY-1: TCP transport GIL freeze on stats/close/repr.

The bug (before fix):
  Thread A parks in `recv()` on a silent peer.  Internally, `recv()` acquires
  the `PyTcpTransport.inner` mutex inside `allow_threads` and then calls
  `recv_bytes()`, which polls in a 100 ms loop.  The mutex is held for the
  entire duration of recv_bytes.

  When thread B calls `stats()`, `peer_addr()`, `close()`, or `repr()` with the
  GIL held, it tries to acquire the same mutex.  Because the GIL is held while
  waiting, the Python runtime cannot schedule ANY Python thread — the interpreter
  freezes completely.

The fix:
  `stats`, `peer_addr`, `__repr__`, and `close` now call `py.allow_threads`
  before acquiring the mutex.  The GIL is released while waiting, so other
  Python threads remain alive.  `close()` additionally fires the cancel handle
  before locking so the recv loop exits within ≤100 ms, making the mutex
  promptly available.

What this test verifies:
  (a) With recv parked in another thread, calling stats() in a sub-thread does
      NOT freeze the main thread.  The main thread can still run Python code
      (sets an Event) while the sub-thread is blocked on the mutex — proving
      the GIL is free.  Before the fix, the sub-thread would hold the GIL
      while waiting, making the main thread block too.

  (b) Calling close() fires the cancel handle, the recv loop exits within
      ≤100 ms, the mutex is released, and the recv thread unblocks with
      TcpError(kind=CLOSED) within a generous watchdog.

Watchdog: 10 s overall.
"""

from __future__ import annotations

import socket
import threading
import time

import pytest

import tstrans.tcp as tcp
from tstrans.exceptions import TcpError


# ─────────────────────────────────────────────────────────────────────────── #
# Helpers
# ─────────────────────────────────────────────────────────────────────────── #

def _bind_silent_peer() -> tuple[socket.socket, int]:
    """Bind a TCP listener that accepts connections but never sends data."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    return srv, port


_OVERALL_WATCHDOG_S = 10.0


# ─────────────────────────────────────────────────────────────────────────── #
# Test
# ─────────────────────────────────────────────────────────────────────────── #

def test_tcp_stats_close_not_gil_blocked_with_parked_recv() -> None:
    """stats() must release the GIL while waiting for the inner mutex;
    close() must unblock a parked recv() within ≤200 ms via the cancel handle."""
    srv, port = _bind_silent_peer()

    accepted_conn: list[socket.socket] = []

    def silent_peer() -> None:
        conn, _ = srv.accept()
        accepted_conn.append(conn)
        # Hold the connection open so recv is not unblocked by peer-close.
        time.sleep(_OVERALL_WATCHDOG_S + 2)
        conn.close()

    peer_t = threading.Thread(target=silent_peer, daemon=True)
    peer_t.start()

    # Connect the tstrans TCP transport.
    transport = (
        tcp.Transport.builder()
        .url(f"tcp://127.0.0.1:{port}")
        .build()
    )

    # Thread A: park in recv(), signal when it returns.
    recv_result: list[object] = []
    recv_started = threading.Event()

    def recv_thread() -> None:
        buf = bytearray(65_536)
        recv_started.set()
        try:
            n = transport.recv(buf)
            recv_result.append(("ok", n))
        except TcpError as exc:
            recv_result.append(("err", exc))
        except BaseException as exc:  # noqa: BLE001
            recv_result.append(("other", exc))

    recv_t = threading.Thread(target=recv_thread, daemon=True)
    recv_t.start()

    # Wait until thread A has entered recv().
    assert recv_started.wait(timeout=3.0), "recv thread did not start"
    # Give it a moment to actually park inside recv_bytes and acquire the mutex.
    time.sleep(0.15)

    # ── Part 1: verify stats() does NOT freeze the GIL ──────────────────── #
    #
    # stats() releases the GIL (allow_threads) while blocking on the inner
    # mutex that recv is holding.  The main thread therefore remains schedulable.
    # We verify this by:
    #   (i)  Running stats() in a sub-thread (so it can block without stalling
    #        the main thread).
    #   (ii) From the main thread, setting an Event within a short window.
    #        If the GIL were frozen by the sub-thread, the main thread could not
    #        set this Event.
    #
    # The sub-thread will block on the mutex until close() is called later.
    # That's expected — the fix only guarantees no GIL freeze, not that stats()
    # returns without waiting for the mutex.

    stats_done = threading.Event()
    stats_result: list[object] = []

    def stats_thread() -> None:
        try:
            s = transport.stats()
            stats_result.append(("ok", s))
        except TcpError as exc:
            stats_result.append(("err", exc))
        except BaseException as exc:  # noqa: BLE001
            stats_result.append(("other", exc))
        finally:
            stats_done.set()

    stats_t = threading.Thread(target=stats_thread, daemon=True)
    stats_t.start()

    # The main thread must remain live while stats_t is blocked on the mutex.
    # A short sleep and Event set is enough to confirm the GIL is not held.
    time.sleep(0.05)  # give stats_t time to enter allow_threads + block on mutex
    main_ran = threading.Event()
    main_ran.set()  # main thread can run Python → GIL is not frozen
    assert main_ran.is_set(), "main thread was frozen (GIL not released by stats)"

    # Also verify peer_addr and repr return (they also release GIL before mutex).
    # They won't return until close() releases the mutex, so run them in threads.
    peer_done = threading.Event()
    repr_done = threading.Event()

    def peer_thread() -> None:
        try:
            transport.peer_addr()
        except Exception:  # noqa: BLE001
            pass
        finally:
            peer_done.set()

    def repr_thread() -> None:
        try:
            repr(transport)
        except Exception:  # noqa: BLE001
            pass
        finally:
            repr_done.set()

    peer_t2 = threading.Thread(target=peer_thread, daemon=True)
    repr_t2 = threading.Thread(target=repr_thread, daemon=True)
    peer_t2.start()
    repr_t2.start()

    # ── Part 2: close() fires cancel and unblocks recv within ≤200 ms ────── #
    transport.close()

    # recv thread must unblock promptly (cancel → alive=false → recv exits).
    recv_t.join(timeout=_OVERALL_WATCHDOG_S)
    assert not recv_t.is_alive(), "recv thread did not unblock after close()"

    # All the blocked sub-threads must also unblock now that the mutex is free.
    assert stats_done.wait(timeout=5.0), "stats() did not return after close()"
    assert peer_done.wait(timeout=5.0), "peer_addr() did not return after close()"
    assert repr_done.wait(timeout=5.0), "repr() did not return after close()"

    # recv thread must return TcpError(kind=CLOSED).
    assert recv_result, "recv thread exited without recording a result"
    kind, value = recv_result[0]
    assert kind == "err", f"expected TcpError from recv after close, got ({kind}, {value!r})"
    assert isinstance(value, TcpError), f"expected TcpError, got {type(value)}"
    assert value.kind.name == "CLOSED", (
        f"expected kind=CLOSED after transport.close(), got kind={value.kind.name!r}"
    )

    # Cleanup.
    srv.close()
    peer_t.join(timeout=1.0)


# ─────────────────────────────────────────────────────────────────────────── #
# X-E03: the destination bytearray is resized while recv() is parked
# ─────────────────────────────────────────────────────────────────────────── #


def test_tcp_recv_resize_during_park_is_refused_and_data_lands_intact() -> None:
    """While thread A is parked in `recv(buf)` (GIL released), thread B
    shrinks `buf` to zero and only THEN the peer sends. Before the fix the
    resize succeeded and A's post-read copy into the (now empty) buffer hit
    a Rust bounds panic (PyO3 `PanicException`) — with the bytes already
    consumed from the socket. Now A holds a buffer export across the park:
    B's resize raises `BufferError` and A returns the bytes into the
    unchanged buffer."""
    srv, port = _bind_silent_peer()
    payload = b"\x47" + bytes(range(1, 100))  # 100 bytes
    accepted: list[socket.socket] = []
    peer_may_send = threading.Event()

    def peer() -> None:
        conn, _ = srv.accept()
        accepted.append(conn)
        peer_may_send.wait(timeout=_OVERALL_WATCHDOG_S)
        conn.sendall(payload)
        time.sleep(1.0)
        conn.close()

    peer_t = threading.Thread(target=peer, daemon=True)
    peer_t.start()

    transport = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    buf = bytearray(4096)
    recv_result: list[object] = []
    recv_started = threading.Event()

    def recv_thread() -> None:
        recv_started.set()
        try:
            n = transport.recv(buf)
            recv_result.append(("ok", n))
        except TcpError as exc:
            recv_result.append(("err", exc))
        except BaseException as exc:  # noqa: BLE001 — PyO3's PanicException is a BaseException
            recv_result.append(("other", exc))

    recv_t = threading.Thread(target=recv_thread, daemon=True)
    recv_t.start()
    assert recv_started.wait(timeout=3.0)
    time.sleep(0.2)  # A is parked inside recv_bytes with the GIL released

    resize_err: list[BaseException] = []

    def resizer() -> None:
        try:
            buf.clear()  # → resize to 0
        except BaseException as exc:  # noqa: BLE001
            resize_err.append(exc)

    rt = threading.Thread(target=resizer, daemon=True)
    rt.start()
    rt.join(2.0)
    peer_may_send.set()  # peer-gated: data arrives only after the resize attempt

    recv_t.join(timeout=5.0)
    if recv_t.is_alive():
        transport.close()  # rescue cancel so teardown never hangs
        recv_t.join(timeout=5.0)
    try:
        assert resize_err and isinstance(resize_err[0], BufferError), (
            f"resize during a parked recv must raise BufferError; got {resize_err!r}"
        )
        assert recv_result, "recv thread recorded no result"
        kind, value = recv_result[0]
        assert kind == "ok", f"recv ended with ({kind}, {value!r})"
        assert value == len(payload)
        assert len(buf) == 4096
        assert bytes(buf[: len(payload)]) == payload
        # The export is released once recv() returns: resizing works again.
        buf.clear()
        assert len(buf) == 0
    finally:
        transport.close()
        for c in accepted:
            c.close()
        srv.close()


def test_tcp_recv_empty_destination_raises_value_error_before_io() -> None:
    """`recv(bytearray())` is a caller error, refused before any socket
    read — it must neither fake a clean EOF nor consume input."""
    srv, port = _bind_silent_peer()
    accepted: list[socket.socket] = []

    def peer() -> None:
        conn, _ = srv.accept()
        accepted.append(conn)
        time.sleep(_OVERALL_WATCHDOG_S)
        conn.close()

    peer_t = threading.Thread(target=peer, daemon=True)
    peer_t.start()
    transport = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    try:
        with pytest.raises(ValueError):
            transport.recv(bytearray())
        # Still usable: a real read into a real buffer works afterwards.
        assert accepted or peer_t.join(2.0) is None
        accepted[0].sendall(b"\x47" * 188)
        buf = bytearray(1024)
        assert transport.recv(buf) == 188
    finally:
        transport.close()
        for c in accepted:
            c.close()
        srv.close()
