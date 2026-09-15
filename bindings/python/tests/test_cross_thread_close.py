"""`close()` from another thread while a call is parked — sender-side
parity for the receiver-side contract PRs #205/#209 established.

Every class below used to close through `&mut self`: a cross-thread
`close()` during a parked (or in-flight, GIL-released) call tripped
PyO3's borrow check and raised `RuntimeError: Already borrowed` instead
of ending the parked call. Each test parks (or keeps in flight) one call
on a worker thread, runs `close()` on a second thread with a bounded
join, and asserts (a) `close()` raised nothing and returned within 2 s,
(b) the parked call ended with the documented kind.

The tests keep a rescue path so a daemon thread parked in native I/O is
unparked before the test fails rather than at interpreter exit.
"""

from __future__ import annotations

import socket
import threading
import time
from typing import Callable

import pytest

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port
from _builders.ports import free_udp_port as _free_udp_port

TS_PACKET = b"\x47" + b"\x00" * 187
TS_BUNDLE = TS_PACKET * 7          # one full 1316-byte framing bundle
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"

_CLOSE_BUDGET_S = 2.0


def _close_on_thread(obj: object) -> tuple[threading.Thread, list[BaseException]]:
    """Run `obj.close()` on its own daemon thread; join with the WP budget."""
    errs: list[BaseException] = []

    def closer() -> None:
        try:
            obj.close()
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=closer, daemon=True)
    t.start()
    t.join(_CLOSE_BUDGET_S)
    return t, errs


def _assert_close_ok(t: threading.Thread, errs: list[BaseException], what: str) -> None:
    assert not errs, f"{what}.close() raised {errs!r}"
    assert not t.is_alive(), f"{what}.close() did not return within {_CLOSE_BUDGET_S} s"


def _spin_sender(send_one: Callable[[], None], stop: threading.Event) -> dict[str, object]:
    """Worker body for the non-parking senders: call `send_one` until it
    raises or `stop` is set; record how it ended."""
    outcome: dict[str, object] = {}
    try:
        while not stop.is_set():
            send_one()
    except BaseException as exc:  # noqa: BLE001
        outcome["exc"] = exc
    return outcome


# --------------------------------------------------------------------------- #
# srt.Sender                                                                  #
# --------------------------------------------------------------------------- #


def _srt_pair(port: int):
    """Listener-mode `srt.Receiver` on a thread + caller-mode `srt.Sender`
    (the `_make_loopback_pair` shape from test_srt_transport.py)."""
    import tstrans.srt as srt

    box: list[srt.Receiver] = []
    errs: list[BaseException] = []

    def accept_worker() -> None:
        try:
            box.append(srt.Receiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    sender = srt.Sender.from_url(f"srt://127.0.0.1:{port}?mode=caller")
    t.join(5.0)
    if errs or not box:
        sender.close()
        pytest.fail(f"srt listener did not accept: {errs!r}")
    return sender, box[0]


def test_srt_sender_close_from_other_thread_during_send() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind

    sender, receiver = _srt_pair(_free_tcp_port())
    stop = threading.Event()
    outcome: dict[str, object] = {}

    def worker() -> None:
        outcome.update(_spin_sender(lambda: sender.send_bytes(TS_BUNDLE), stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)  # the worker is now cycling through send_bytes
    try:
        c, errs = _close_on_thread(sender)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "srt.Sender")
        assert not w.is_alive(), "send loop did not end after close()"
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"send loop ended with {exc!r}"
        assert exc.kind in (SrtErrorKind.CLOSED, SrtErrorKind.BROKEN), exc.kind
        assert not sender.is_alive()
    finally:
        stop.set()
        sender.close()
        receiver.close()


# --------------------------------------------------------------------------- #
# srt.MuxSender                                                               #
# --------------------------------------------------------------------------- #


def _srt_mux_sender_with_silent_peer(port: int):
    """Listener-mode `srt.Receiver` (never reads) + caller `srt.MuxSender`."""
    import tstrans.srt as srt

    box: list[srt.Receiver] = []
    errs: list[BaseException] = []

    def accept_worker() -> None:
        try:
            box.append(srt.Receiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    tx = srt.MuxSender.from_url(f"srt://127.0.0.1:{port}?mode=caller", _video_only_program())
    t.join(5.0)
    if errs or not box:
        tx.close()
        pytest.fail(f"srt listener did not accept: {errs!r}")
    return tx, box[0]


def test_srt_mux_sender_close_from_other_thread_during_send_video() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    from tstrans.mpegts import Pts90khz

    tx, peer = _srt_mux_sender_with_silent_peer(_free_tcp_port())
    stop = threading.Event()
    outcome: dict[str, object] = {}
    pts = [0]

    def send_one() -> None:
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(pts[0]), key_frame=True)
        pts[0] += 3000

    def worker() -> None:
        outcome.update(_spin_sender(send_one, stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "srt.MuxSender")
        assert not w.is_alive(), "send loop did not end after close()"
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"send loop ended with {exc!r}"
        assert exc.kind in (SrtErrorKind.CLOSED, SrtErrorKind.BROKEN), exc.kind
        assert not tx.is_alive()
    finally:
        stop.set()
        tx.close()
        peer.close()


def test_srt_mux_sender_cancel_handle_wakes_send_from_other_thread() -> None:
    """`MuxSender.cancel_handle()` did not exist: the primary SRT sending
    object had no cross-thread interrupt path at all (CORR-02)."""
    from tstrans.exceptions import SrtError, SrtErrorKind
    from tstrans.mpegts import Pts90khz
    from tstrans.srt import CancelHandle

    tx, peer = _srt_mux_sender_with_silent_peer(_free_tcp_port())
    try:
        handle = tx.cancel_handle()
        assert isinstance(handle, CancelHandle)
        assert not handle.is_cancelled()
        stop = threading.Event()
        outcome: dict[str, object] = {}

        def worker() -> None:
            outcome.update(
                _spin_sender(
                    lambda: tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True),
                    stop,
                )
            )

        w = threading.Thread(target=worker, daemon=True)
        w.start()
        time.sleep(0.1)
        handle.cancel()
        w.join(5.0)
        stop.set()
        assert handle.is_cancelled()
        assert not w.is_alive(), "cancel() did not end the send loop"
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"send loop ended with {exc!r}"
        assert exc.kind in (SrtErrorKind.CLOSED, SrtErrorKind.BROKEN), exc.kind
    finally:
        tx.close()
        peer.close()


# --------------------------------------------------------------------------- #
# srt.ManagedSender / srt.ManagedReceiver                                     #
# --------------------------------------------------------------------------- #

# `conntimeo=500` bounds the reconnect factory's dead-port connect to 0.5 s
# (libsrt's default is 3 s and a caller connect to a closed loopback port
# only fails at that deadline), so a close() that lands while the factory
# is running returns well inside the 2 s budget. `max_attempts=None` +
# a 5 s constant backoff keeps the worker parked for as long as the test
# needs — nothing but the close under test ends it.
def _outage_policy():
    from tstrans.srt import BackoffStrategy, ReconnectPolicy

    return ReconnectPolicy(max_attempts=None, backoff=BackoffStrategy.constant(ms=5000))


def _wait_until_parked(progress: list[int], what: str, budget_s: float = 20.0) -> None:
    """Block until `progress` has stopped advancing for a full second — the
    worker's send loop is then parked inside one native call.

    A fixed short sleep is not enough: libsrt only marks a caller socket
    Broken after its 5 s peer-idle timeout, and `?peeridletimeo=` is on
    tst-srt's rejected-URL-keys list, so for ~5 s after the peer drop the
    sender happily keeps sending into the void. Closing in that window
    would land between two sends and prove nothing.
    """
    deadline = time.monotonic() + budget_s
    last, stable_since = progress[0], time.monotonic()
    while time.monotonic() < deadline:
        time.sleep(0.1)
        if progress[0] != last:
            last, stable_since = progress[0], time.monotonic()
        elif time.monotonic() - stable_since > 1.0:
            return
    pytest.fail(f"{what}: the send loop never parked within {budget_s} s")


def _plain_srt_receiver_on_thread(port: int):
    """Accept one caller with a plain `srt.Receiver` on a daemon thread; the
    returned box holds the receiver once the handshake completes."""
    import tstrans.srt as srt

    box: list[srt.Receiver] = []

    def accept_worker() -> None:
        try:
            box.append(srt.Receiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException:  # noqa: BLE001
            pass

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    return t, box


def test_srt_managed_sender_close_from_other_thread_during_blocking_backoff() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    import tstrans.srt as srt

    port = _free_tcp_port()
    accept_t, box = _plain_srt_receiver_on_thread(port)
    tx = srt.ManagedSender.from_url(
        f"srt://127.0.0.1:{port}?mode=caller&conntimeo=500", policy=_outage_policy()
    )
    accept_t.join(5.0)
    if not box:
        tx.close()
        pytest.fail("plain srt listener did not accept the managed caller")
    peer = box[0]
    for _ in range(3):
        tx.send_bytes(TS_BUNDLE)
    # Peer drop: the next send that notices the break re-enters the
    # reconnect loop (factory → 5 s backoff → factory …) and parks there.
    peer.close()
    stop = threading.Event()
    outcome: dict[str, object] = {}
    progress = [0]

    def worker() -> None:
        def send_one() -> None:
            tx.send_bytes(TS_BUNDLE)
            progress[0] += 1
            time.sleep(0.02)

        outcome.update(_spin_sender(send_one, stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    _wait_until_parked(progress, "srt.ManagedSender")
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        if w.is_alive():
            # Rescue: a fresh listener lets the factory succeed so the
            # worker leaves the reconnect loop and sees `stop`.
            _rescue_t, _rescue_box = _plain_srt_receiver_on_thread(port)
            w.join(5.0)
            for r in _rescue_box:
                r.close()
        _assert_close_ok(c, errs, "srt.ManagedSender")
        assert not w.is_alive(), "close() did not end the send parked in the reconnect loop"
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"parked send ended with {exc!r}"
        assert exc.kind == SrtErrorKind.CLOSED, exc.kind
        assert not tx.is_alive()
    finally:
        stop.set()
        tx.close()


def test_srt_managed_receiver_close_from_other_thread_while_recv_parked() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    import tstrans.srt as srt

    port = _free_tcp_port()
    box: list[srt.ManagedReceiver] = []
    errs: list[BaseException] = []

    def accept_worker() -> None:
        try:
            box.append(srt.ManagedReceiver.from_url(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    time.sleep(0.1)
    # Silent peer: connects, never sends.
    peer = srt.Sender.from_url(f"srt://127.0.0.1:{port}?mode=caller")
    t.join(5.0)
    if errs or not box:
        peer.close()
        pytest.fail(f"ManagedReceiver did not accept: {errs!r}")
    rx = box[0]
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            rx.recv_bytes(max_len=1316)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, close_errs = _close_on_thread(rx)
        w.join(5.0)
        if w.is_alive():
            peer.send_bytes(TS_BUNDLE)  # rescue: unpark the recv
            w.join(5.0)
        _assert_close_ok(c, close_errs, "srt.ManagedReceiver")
        assert not w.is_alive(), "close() did not end the parked recv_bytes()"
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, SrtError), f"parked recv ended with {err!r}"
        assert err.kind == SrtErrorKind.CLOSED, err.kind
        assert not rx.is_alive()
    finally:
        peer.close()
        rx.close()
