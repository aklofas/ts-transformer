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
# `rtp.Sender`'s default `pkt_size=1316` is the whole DATAGRAM, so the TS
# payload cap is 1316 - 12 (RTP header) = 1304 B: six packets, not seven.
RTP_BUNDLE = TS_PACKET * 6

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

    def worker() -> None:
        _spin_sender(lambda: sender.send_bytes(TS_BUNDLE), stop)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)  # the worker is now cycling through send_bytes
    try:
        c, errs = _close_on_thread(sender)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "srt.Sender")
        assert not w.is_alive(), "send loop did not end after close()"
        # The worker loop only checks `stop` between iterations, so whether
        # it attempts one more send before noticing `stop` is a scheduling
        # race, not a guarantee `send_bytes()` makes — a plain srt.Sender
        # with an idle peer never parks, so on a loaded CI runner the
        # worker can legitimately see `stop` first and exit clean, leaving
        # `outcome` empty (PR #232 run 35182377345). `close()` above
        # already returned, which deterministically guarantees the slot is
        # empty; call `send_bytes()` directly to prove a post-close send is
        # rejected without depending on the worker's own timing (the same
        # shape the rtp/udp non-parking sender tests below use).
        with pytest.raises(SrtError) as ei:
            sender.send_bytes(TS_BUNDLE)
        assert ei.value.kind == SrtErrorKind.CLOSED, ei.value.kind
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
    pts = [0]

    def send_one() -> None:
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(pts[0]), key_frame=True)
        pts[0] += 3000

    def worker() -> None:
        _spin_sender(send_one, stop)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "srt.MuxSender")
        assert not w.is_alive(), "send loop did not end after close()"
        # See test_srt_sender_close_from_other_thread_during_send: whether
        # the worker attempts one more send before noticing `stop` is a
        # scheduling race, not a guarantee. `close()` already returned, so
        # the slot is deterministically empty; call `send_video()` directly
        # to prove a post-close send is rejected without depending on the
        # worker's own timing.
        with pytest.raises(SrtError) as ei:
            send_one()
        assert ei.value.kind == SrtErrorKind.CLOSED, ei.value.kind
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


def test_srt_close_from_other_thread_is_observable_on_a_prior_cancel_handle() -> None:
    """`close()` cancels first; a handle obtained BEFORE the close reports
    `is_cancelled()` afterwards (shared state — the watchdog pattern)."""
    sender, receiver = _srt_pair(_free_tcp_port())
    try:
        handle = sender.cancel_handle()
        assert not handle.is_cancelled()
        c, errs = _close_on_thread(sender)
        _assert_close_ok(c, errs, "srt.Sender")
        assert handle.is_cancelled()
    finally:
        sender.close()
        receiver.close()


def test_rtp_cancel_handle_has_shared_is_cancelled() -> None:
    import tstrans.rtp as rtp

    sink, port = _udp_sink()
    tx = rtp.Sender(f"rtp://127.0.0.1:{port}")
    try:
        h1 = tx.cancel_handle()
        h2 = tx.cancel_handle()
        assert not h1.is_cancelled() and not h2.is_cancelled()
        tx.close()
        assert h1.is_cancelled() and h2.is_cancelled()
    finally:
        tx.close()
        sink.close()


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


# --------------------------------------------------------------------------- #
# srt.ManagedMuxSender                                                        #
# --------------------------------------------------------------------------- #


def _managed_mux_sender_after_peer_drop(port: int):
    """Caller `ManagedMuxSender` (outage policy, 0.5 s conntimeo) whose plain
    `srt.Receiver` peer accepted, took a few frames, then closed."""
    import tstrans.srt as srt
    from tstrans.mpegts import Pts90khz

    accept_t, box = _plain_srt_receiver_on_thread(port)
    tx = srt.ManagedMuxSender.from_url(
        f"srt://127.0.0.1:{port}?mode=caller&conntimeo=500",
        _video_only_program(),
        policy=_outage_policy(),
    )
    accept_t.join(5.0)
    if not box:
        tx.close()
        pytest.fail("plain srt listener did not accept the managed caller")
    for i in range(3):
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i == 0))
    box[0].close()
    return tx


def _park_send_video(tx) -> tuple[threading.Thread, threading.Event, dict[str, object]]:
    from tstrans.mpegts import Pts90khz

    stop = threading.Event()
    outcome: dict[str, object] = {}
    progress = [0]

    def worker() -> None:
        def send_one() -> None:
            tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)
            progress[0] += 1
            time.sleep(0.02)

        outcome.update(_spin_sender(send_one, stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    _wait_until_parked(progress, "srt.ManagedMuxSender")
    return w, stop, outcome


def test_srt_managed_mux_sender_close_from_other_thread_during_blocking_backoff() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind

    port = _free_tcp_port()
    tx = _managed_mux_sender_after_peer_drop(port)
    w, stop, outcome = _park_send_video(tx)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        if w.is_alive():
            _t, rescue_box = _plain_srt_receiver_on_thread(port)
            w.join(5.0)
            for r in rescue_box:
                r.close()
        _assert_close_ok(c, errs, "srt.ManagedMuxSender")
        assert not w.is_alive(), "close() did not end the send parked in the reconnect loop"
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"parked send ended with {exc!r}"
        assert exc.kind == SrtErrorKind.CLOSED, exc.kind
        assert not tx.is_alive()
    finally:
        stop.set()
        tx.close()


def test_srt_managed_mux_sender_cancel_handle_wakes_blocking_backoff() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    from tstrans.srt import CancelHandle

    port = _free_tcp_port()
    tx = _managed_mux_sender_after_peer_drop(port)
    try:
        handle = tx.cancel_handle()
        assert isinstance(handle, CancelHandle)
        w, stop, outcome = _park_send_video(tx)
        handle.cancel()
        w.join(5.0)
        stop.set()
        if w.is_alive():
            _t, rescue_box = _plain_srt_receiver_on_thread(port)
            w.join(5.0)
            for r in rescue_box:
                r.close()
            pytest.fail("cancel() did not wake the send parked in the reconnect loop")
        exc = outcome.get("exc")
        assert isinstance(exc, SrtError), f"parked send ended with {exc!r}"
        assert exc.kind == SrtErrorKind.CLOSED, exc.kind
        assert handle.is_cancelled()
    finally:
        tx.close()


# --------------------------------------------------------------------------- #
# srt.Listener / srt.Socket                                                   #
# --------------------------------------------------------------------------- #


def test_srt_listener_close_from_other_thread_while_accept_parked() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    import tstrans.srt as srt

    port = _free_tcp_port()
    lst = srt.Builder(f"srt://127.0.0.1:{port}?mode=listener").listen()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            lst.accept()
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, errs = _close_on_thread(lst)
        w.join(5.0)
        if w.is_alive():
            # Rescue: a caller connect unparks the accept.
            rescue = srt.Builder(f"srt://127.0.0.1:{port}").connect()
            w.join(5.0)
            rescue.close()
        _assert_close_ok(c, errs, "srt.Listener")
        assert not w.is_alive(), "close() did not end the parked accept()"
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, SrtError), f"parked accept ended with {err!r}"
        assert err.kind == SrtErrorKind.CLOSED, err.kind
        assert not lst.is_alive()
    finally:
        lst.close()


def test_srt_socket_close_and_getters_from_other_thread_do_not_raise() -> None:
    """Keepalive, not a regression test: `Socket` has no call that parks with
    its borrow held, so the `&mut self` shape never raised here. It pins the
    uniform `&self` contract after the conversion (getters and `close()`
    from a second thread, then the consuming `into_*` raises CLOSED)."""
    from tstrans.exceptions import SrtError, SrtErrorKind
    import tstrans.srt as srt

    port = _free_tcp_port()
    lst = srt.Builder(f"srt://127.0.0.1:{port}?mode=listener").listen()
    box: list[srt.Socket] = []

    def accept_worker() -> None:
        box.append(lst.accept(timeout_ms=5000))

    t = threading.Thread(target=accept_worker, daemon=True)
    t.start()
    caller = srt.Builder(f"srt://127.0.0.1:{port}").connect()
    t.join(6.0)
    assert box, "listener did not accept"
    sock = box[0]
    errs: list[BaseException] = []

    def other_thread() -> None:
        try:
            sock.local_addr()
            sock.peer_addr()
            sock.stream_id()
            assert sock.is_alive()
            sock.close()
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    o = threading.Thread(target=other_thread, daemon=True)
    o.start()
    o.join(5.0)
    assert not o.is_alive(), "close thread hung"
    assert not errs, f"second-thread use raised {errs!r}"
    assert not sock.is_alive()
    with pytest.raises(SrtError) as ei:
        sock.into_sender()
    assert ei.value.kind == SrtErrorKind.CLOSED
    caller.close()
    lst.close()


# --------------------------------------------------------------------------- #
# rtp.Sender / rtp.Receiver                                                   #
# --------------------------------------------------------------------------- #


def _udp_sink() -> tuple[socket.socket, int]:
    """A bound UDP socket that absorbs datagrams (keeps the loopback peer
    reachable so a connected sender never sees ECONNREFUSED)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", 0))
    return s, s.getsockname()[1]


def test_rtp_sender_close_from_other_thread_during_send() -> None:
    from tstrans.exceptions import RtpError, RtpErrorKind
    import tstrans.rtp as rtp

    sink, port = _udp_sink()
    tx = rtp.Sender(f"rtp://127.0.0.1:{port}")
    stop = threading.Event()
    outcome: dict[str, object] = {}

    def worker() -> None:
        outcome.update(_spin_sender(lambda: tx.send(RTP_BUNDLE), stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "rtp.Sender")
        assert not w.is_alive(), "send loop did not end after close()"
        # The worker loop only checks `stop` between iterations, so whether
        # it happens to attempt one more send before noticing `stop` is a
        # scheduling race, not a guarantee `send()` makes — on a loaded CI
        # runner the worker can legitimately see `stop` first and exit
        # clean, leaving `outcome` empty. `close()` above already
        # returned, which deterministically guarantees the slot is empty;
        # call `send()` directly to prove a post-close send is rejected
        # without depending on the worker's own timing.
        with pytest.raises(RtpError) as ei:
            tx.send(RTP_BUNDLE)
        assert ei.value.kind == RtpErrorKind.CLOSED, ei.value.kind
        assert "closed" in repr(tx)
    finally:
        stop.set()
        tx.close()
        sink.close()


def test_rtp_receiver_close_from_other_thread_while_recv_parked() -> None:
    from tstrans.exceptions import RtpError, RtpErrorKind
    import tstrans.rtp as rtp

    port = _free_udp_port()
    rx = rtp.Receiver(f"rtp://127.0.0.1:{port}")
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            rx.recv()
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, errs = _close_on_thread(rx)
        w.join(5.0)
        if w.is_alive():
            # Rescue: a datagram unparks the recv (12-byte RTP header + TS).
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.sendto(
                b"\x80\x21\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01" + TS_PACKET,
                ("127.0.0.1", port),
            )
            s.close()
            w.join(5.0)
        _assert_close_ok(c, errs, "rtp.Receiver")
        assert not w.is_alive(), "close() did not end the parked recv()"
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, RtpError), f"parked recv ended with {err!r}"
        assert err.kind == RtpErrorKind.CLOSED, err.kind
        assert rx.end_reason() is not None  # Cancelled — recorded by close()
    finally:
        rx.close()


# --------------------------------------------------------------------------- #
# rtp.MuxSender                                                               #
# --------------------------------------------------------------------------- #


def test_rtp_mux_sender_close_from_other_thread_during_send_video() -> None:
    from tstrans.exceptions import RtpError, RtpErrorKind
    from tstrans.mpegts import Pts90khz
    import tstrans.rtp as rtp

    sink, port = _udp_sink()
    tx = rtp.MuxSender(f"rtp://127.0.0.1:{port}", _video_only_program())
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
        _assert_close_ok(c, errs, "rtp.MuxSender")
        assert not w.is_alive(), "send loop did not end after close()"
        # See the identical comment in test_rtp_sender_close_from_other_
        # thread_during_send: whether the worker attempts one more send
        # before noticing `stop` is a scheduling race, not a guarantee.
        # `close()` already returned, so the slot is deterministically
        # empty; call `send_video()` directly to prove a post-close send
        # is rejected without depending on the worker's own timing.
        with pytest.raises(RtpError) as ei:
            send_one()
        assert ei.value.kind == RtpErrorKind.CLOSED, ei.value.kind
        assert "closed" in repr(tx)
    finally:
        stop.set()
        tx.close()
        sink.close()


# --------------------------------------------------------------------------- #
# rtp.H264Receiver                                                            #
# --------------------------------------------------------------------------- #


def test_rtp_h264_receiver_close_from_other_thread_while_recv_au_parked() -> None:
    import tstrans.rtp as rtp

    rx = rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96")
    # Read the bound address up front (it is a construction-time snapshot,
    # so reading it while the worker is parked would also work); the udp /
    # rtp / srt siblings capture their ports the same way.
    host_port = rx.local_addr()
    assert host_port is not None
    host, port = host_port.rsplit(":", 1)
    outcome: dict[str, object] = {}

    def worker() -> None:
        try:
            outcome["ret"] = rx.recv_au()
        except BaseException as exc:  # noqa: BLE001
            outcome["exc"] = exc

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, errs = _close_on_thread(rx)
        w.join(5.0)
        if w.is_alive():
            # Rescue: a single-NAL IDR packet completes an AU and unparks.
            # Uses the address captured above — never the object under test.
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.sendto(
                b"\x80\xe0\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01" + b"\x65\xAB\xCD",
                (host, int(port)),
            )
            s.close()
            w.join(5.0)
        _assert_close_ok(c, errs, "rtp.H264Receiver")
        assert not w.is_alive(), "close() did not end the parked recv_au()"
        assert "exc" not in outcome, f"parked recv_au raised {outcome['exc']!r}"
        # Cancel/close is EOS on this shell: recv_au() returns None.
        assert outcome.get("ret") is None
        assert rx.end_reason() is not None  # Cancelled, snapshotted by close()
        assert "closed" in repr(rx)
    finally:
        rx.close()


# --------------------------------------------------------------------------- #
# udp.Transport / udp.RecvTransport                                           #
# --------------------------------------------------------------------------- #


def test_udp_transport_close_from_other_thread_during_send() -> None:
    from tstrans import udp
    from tstrans.exceptions import UdpError, UdpErrorKind

    sink, port = _udp_sink()
    tx = udp.Transport.builder().url(f"udp://127.0.0.1:{port}").build()
    stop = threading.Event()
    outcome: dict[str, object] = {}

    def worker() -> None:
        outcome.update(_spin_sender(lambda: tx.send(TS_BUNDLE), stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.1)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "udp.Transport")
        assert not w.is_alive(), "send loop did not end after close()"
        # See the identical comment in test_rtp_sender_close_from_other_
        # thread_during_send: whether the worker attempts one more send
        # before noticing `stop` is a scheduling race, not a guarantee —
        # `udp.Transport` has no cancel handle at all (send() never
        # parks), so there is even less reason to expect the worker to
        # land another send before `stop` becomes visible to it. `close()`
        # already returned, so the slot is deterministically empty; call
        # `send()` directly to prove a post-close send is rejected without
        # depending on the worker's own timing.
        with pytest.raises(UdpError) as ei:
            tx.send(TS_BUNDLE)
        assert ei.value.kind == UdpErrorKind.CLOSED, ei.value.kind
        assert "closed" in repr(tx)
    finally:
        stop.set()
        tx.close()
        sink.close()


def test_udp_recv_transport_close_from_other_thread_while_recv_parked() -> None:
    from tstrans import udp
    from tstrans.exceptions import UdpError, UdpErrorKind

    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    port = rx.local_addr_port()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            rx.recv(timeout_ms=None)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, errs = _close_on_thread(rx)
        w.join(5.0)
        if w.is_alive():
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.sendto(TS_PACKET, ("127.0.0.1", port))  # rescue
            s.close()
            w.join(5.0)
        _assert_close_ok(c, errs, "udp.RecvTransport")
        assert not w.is_alive(), "close() did not end the parked recv()"
        # A rescued recv returns data, not an error: `captured` then stays
        # empty and the assertion below is what fails — no wall-clock bound.
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, UdpError), f"parked recv ended with {err!r}"
        assert err.kind == UdpErrorKind.CLOSED, err.kind
    finally:
        rx.close()


def test_udp_recv_transport_timeout_ms_still_raises_backpressure() -> None:
    """The polling rewrite must keep the documented per-call deadline
    contract: `recv(timeout_ms=N)` with no data still ENDS, and ends with
    `UdpError(BACKPRESSURE)` "recv timed out" — not `CLOSED`, not a hang
    (the kind was `IO` before 0.7.0). The call
    runs on a worker with a generous join so a regression that never
    honours the deadline fails the test (after a rescue `close()`) instead
    of wedging the process; how long the 50 ms deadline actually takes is
    deliberately not asserted (wall-clock bounds are a flake class)."""
    from tstrans import udp
    from tstrans.exceptions import UdpError, UdpErrorKind

    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            rx.recv(timeout_ms=50)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    w.join(30.0)
    try:
        if w.is_alive():
            rx.close()  # rescue: the stop flag ends the parked recv
            w.join(5.0)
            pytest.fail("recv(timeout_ms=50) did not return within 30 s")
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, UdpError), f"recv ended with {err!r}"
        assert err.kind == UdpErrorKind.BACKPRESSURE, err.kind
        assert "timed out" in str(err)
    finally:
        rx.close()


# --------------------------------------------------------------------------- #
# rist.Transport / rist.RecvTransport                                         #
# --------------------------------------------------------------------------- #


def _rist_recv_or_skip():
    """Open a RIST receiver on a free even loopback port (librist Simple
    profile needs even ports); skip like test_rist_basic.py when none binds."""
    from tstrans import rist
    from tstrans.exceptions import RistError

    for port in range(34110, 34150, 2):
        try:
            rx = rist.RecvTransport.builder().bind_url(f"rist://@127.0.0.1:{port}").build()
            return rx, port
        except RistError:
            continue
    pytest.skip("could not bind any even RIST port (librist unavailable)")


def test_rist_transport_close_from_other_thread_during_send() -> None:
    from tstrans import rist
    from tstrans.exceptions import RistError, RistErrorKind

    rx, port = _rist_recv_or_skip()
    try:
        tx = rist.Transport.builder().url(f"rist://127.0.0.1:{port}").build()
    except RistError as e:
        rx.close()
        pytest.skip(f"sender build failed ({e.kind.name}): {e}")
    stop = threading.Event()
    outcome: dict[str, object] = {}

    def worker() -> None:
        outcome.update(_spin_sender(lambda: tx.send(TS_BUNDLE), stop))

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.2)
    try:
        c, errs = _close_on_thread(tx)
        stop.set()
        w.join(5.0)
        _assert_close_ok(c, errs, "rist.Transport")
        assert not w.is_alive(), "send loop did not end after close()"
        exc = outcome.get("exc")
        assert isinstance(exc, RistError), f"send loop ended with {exc!r}"
        assert exc.kind == RistErrorKind.CLOSED, exc.kind
        assert "closed" in repr(tx)
    finally:
        stop.set()
        tx.close()
        rx.close()


def test_rist_recv_transport_close_from_other_thread_while_recv_parked() -> None:
    from tstrans import rist
    from tstrans.exceptions import RistError, RistErrorKind

    rx, port = _rist_recv_or_skip()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            rx.recv(timeout_ms=None)
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    w.start()
    time.sleep(0.3)
    try:
        c, errs = _close_on_thread(rx)
        w.join(5.0)
        if w.is_alive():
            # Rescue: a sender session delivers one packet and unparks the recv.
            try:
                tx = rist.Transport.builder().url(f"rist://127.0.0.1:{port}").build()
                for _ in range(20):
                    tx.send(TS_BUNDLE)
                    time.sleep(0.05)
                tx.close()
            except RistError:
                pass
            w.join(5.0)
        _assert_close_ok(c, errs, "rist.RecvTransport")
        assert not w.is_alive(), "close() did not end the parked recv()"
        # A rescued recv returns data, not an error: `captured` then stays
        # empty and the assertion below is what fails — no wall-clock bound.
        assert len(captured) == 1, f"expected one error; got {captured!r}"
        err = captured[0]
        assert isinstance(err, RistError), f"parked recv ended with {err!r}"
        assert err.kind == RistErrorKind.CLOSED, err.kind
    finally:
        rx.close()


# --------------------------------------------------------------------------- #
# Construction-constant getters must not wait behind a parked call            #
# --------------------------------------------------------------------------- #


def _assert_getter_does_not_wait_behind_park(
    what: str,
    park: Callable[[], object],
    getter: Callable[[], object],
    end_park: Callable[[], None],
) -> object:
    """Park `park()` on a worker, then call `getter()` on a second thread
    and require it to answer while the park is still in progress.

    A getter that goes through the slot the park holds does not return
    until the park ends — and the caller that wants the port is usually
    the one that has to connect / send to end it, so that is a deadlock,
    not slowness. The only bound is therefore a generous hang deadline;
    `end_park()` (the object's `close()`) always runs before the failure
    so no daemon thread is left inside native code."""
    outcome: dict[str, object] = {}

    def park_worker() -> None:
        try:
            park()
        except BaseException:  # noqa: BLE001
            pass

    def getter_worker() -> None:
        try:
            outcome["value"] = getter()
        except BaseException as exc:  # noqa: BLE001
            outcome["exc"] = exc

    p = threading.Thread(target=park_worker, daemon=True)
    p.start()
    time.sleep(0.3)  # parked with the GIL released
    g = threading.Thread(target=getter_worker, daemon=True)
    g.start()
    g.join(30.0)
    blocked = g.is_alive()
    end_park()  # rescue: ends the park (and frees a slot-bound getter)
    p.join(5.0)
    g.join(5.0)
    assert not blocked, f"{what} waited behind the parked call instead of answering"
    assert "exc" not in outcome, f"{what} raised {outcome['exc']!r} while the call was parked"
    return outcome.get("value")


def test_srt_listener_local_addr_does_not_wait_behind_parked_accept() -> None:
    from tstrans.exceptions import SrtError, SrtErrorKind
    import tstrans.srt as srt

    lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    before = lst.local_addr()
    got = _assert_getter_does_not_wait_behind_park(
        "srt.Listener.local_addr()", lst.accept, lst.local_addr, lst.close
    )
    assert got == before
    with pytest.raises(SrtError) as ei:
        lst.local_addr()
    assert ei.value.kind == SrtErrorKind.CLOSED


def test_tcp_listener_local_port_does_not_wait_behind_parked_accept() -> None:
    from tstrans import tcp
    from tstrans.exceptions import TcpError, TcpErrorKind

    lst = tcp.Listener.builder().bind("127.0.0.1:0").build()
    before = lst.local_port()
    got = _assert_getter_does_not_wait_behind_park(
        "tcp.Listener.local_port()", lst.accept_blocking, lst.local_port, lst.close
    )
    assert got == before
    with pytest.raises(TcpError) as ei:
        lst.local_port()
    assert ei.value.kind == TcpErrorKind.CLOSED


def test_udp_recv_transport_local_addr_port_does_not_wait_behind_parked_recv() -> None:
    from tstrans import udp
    from tstrans.exceptions import UdpError, UdpErrorKind

    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    before = rx.local_addr_port()
    got = _assert_getter_does_not_wait_behind_park(
        "udp.RecvTransport.local_addr_port()",
        lambda: rx.recv(timeout_ms=None),
        rx.local_addr_port,
        rx.close,
    )
    assert got == before
    with pytest.raises(UdpError) as ei:
        rx.local_addr_port()
    assert ei.value.kind == UdpErrorKind.CLOSED


def test_rtp_h264_receiver_local_addr_does_not_wait_behind_parked_recv_au() -> None:
    from tstrans.exceptions import RtpError
    import tstrans.rtp as rtp

    rx = rtp.H264Receiver.listen("rtp://127.0.0.1:0?pt=96")
    before = rx.local_addr()
    assert before is not None
    got = _assert_getter_does_not_wait_behind_park(
        "rtp.H264Receiver.local_addr()", rx.recv_au, rx.local_addr, rx.close
    )
    assert got == before
    with pytest.raises(RtpError):  # closed-handle contract, never None
        rx.local_addr()


def test_rist_recv_transport_repr_does_not_wait_behind_parked_recv() -> None:
    rx, _port = _rist_recv_or_skip()
    before = repr(rx)
    assert "rist://" in before
    got = _assert_getter_does_not_wait_behind_park(
        "repr(rist.RecvTransport)", lambda: rx.recv(timeout_ms=None), lambda: repr(rx), rx.close
    )
    assert got == before
    assert repr(rx) == "RecvTransport(closed)"
