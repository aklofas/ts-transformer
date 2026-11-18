"""`tstrans.shells.OpenCancel`: the host's cancel for an open that has not
returned a shell yet.

A listener-mode `srt://` open blocks in its first accept before any
`cancel_handle()` exists. An `OpenCancel` made beforehand and passed as
`cancel=` ends that accept from another thread; an object already
cancelled refuses every open before it parses its URL; and an open that
finishes after the cancel raises `CLOSED` whatever it did, so nothing is
handed out. An open that returned before the cancel is untouched by it.
"""

from __future__ import annotations

import socket
import threading
import time
from collections.abc import Callable
from typing import Any

import pytest

import tstrans.srt as srt
from tstrans import exceptions, shells
from tstrans.shells import OpenCancel

from _builders.mux_programs import video_only_program
from _builders.native_hooks import shells_hook
from _builders.ports import free_udp_port

# Far longer than any healthy wake takes: a test fails, it does not hang.
DEADLINE_S = 10.0
TS_PACKET = b"\x47" + bytes(187)


def _wait_for(what: str, cond: Callable[[], bool], limit: float = DEADLINE_S) -> None:
    end = time.monotonic() + limit
    while not cond():
        if time.monotonic() > end:
            pytest.fail(f"timed out after {limit}s waiting for {what}")
        time.sleep(0.01)


def _udp_port_bound(port: int) -> bool:
    """`True` while something holds `port` (a plain bind of it fails)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.bind(("127.0.0.1", port))
    except OSError:
        return True
    finally:
        s.close()
    return False


class _Open:
    """Run one open on a daemon thread and keep what it returned or raised."""

    def __init__(self, call: Callable[[], Any]) -> None:
        self.result: Any = None
        self.error: BaseException | None = None
        self.done = threading.Event()

        def run() -> None:
            try:
                self.result = call()
            except BaseException as exc:  # noqa: BLE001
                self.error = exc
            finally:
                self.done.set()

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def join(self, what: str) -> None:
        self.thread.join(DEADLINE_S)
        assert not self.thread.is_alive(), f"{what}: the open is still parked"

    def close_result(self) -> None:
        if self.result is not None:
            self.result.close()


def _rescue(port: int, opens: list[_Open]) -> None:
    """Connect a peer to every open still parked, so no thread is left in a
    native accept when the cancel under test failed to wake it."""
    for o in opens:
        if o.done.is_set():
            continue
        try:
            tx = shells.Sender.open(f"srt://127.0.0.1:{port}?conntimeo=3000")
            tx.close()
        except exceptions.SrtError:
            pass
        o.thread.join(DEADLINE_S)
        o.close_result()


def _assert_closed(exc: BaseException | None, cls: type = exceptions.SrtError) -> None:
    assert isinstance(exc, cls), f"expected {cls.__name__}(CLOSED), got {exc!r}"
    assert exc.kind.name == "CLOSED", f"expected CLOSED, got {exc!r}"


# The four opens that block in a listener-mode first accept.
LISTENER_OPENS: dict[str, Callable[[str, OpenCancel], Any]] = {
    "Receiver.open": lambda url, oc: shells.Receiver.open(url, cancel=oc),
    "DemuxReceiver.open": lambda url, oc: shells.DemuxReceiver.open(url, cancel=oc),
    "ManagedReceiver.from_url": lambda url, oc: srt.ManagedReceiver.from_url(url, cancel=oc),
    "ManagedDemuxReceiver.from_url": lambda url, oc: srt.ManagedDemuxReceiver.from_url(
        url, cancel=oc
    ),
}


def _park(open_fn: Callable[[str, OpenCancel], Any], oc: OpenCancel) -> tuple[int, _Open]:
    """Start a listener-mode open with `oc` and return once it is parked:
    its port is bound and the open has not returned.

    A free port can be taken before the open binds it, by another process
    (the box is shared) or by this wait's own probe, which binds the port
    to see whether it is taken; that bind failure is retried on a new port,
    since it is not what any test here is about."""
    for _ in range(5):
        port = free_udp_port()
        o = _Open(lambda: open_fn(f"srt://127.0.0.1:{port}?mode=listener", oc))
        _wait_for("the open to bind its port", lambda: _udp_port_bound(port) or o.done.is_set())
        if o.done.is_set() and isinstance(o.error, exceptions.SrtError) and str(o.error).startswith("bind: "):
            continue
        break
    assert not o.done.is_set(), f"the open returned instead of parking: {o.error!r}"
    return port, o


@pytest.mark.parametrize("name", list(LISTENER_OPENS))
def test_cancel_ends_a_parked_first_accept(name: str) -> None:
    oc = OpenCancel()
    port, o = _park(LISTENER_OPENS[name], oc)
    try:
        oc.cancel()
        o.join(name)
        _assert_closed(o.error)
        assert o.result is None
        assert oc.is_cancelled()
        _wait_for("the port to be released", lambda: not _udp_port_bound(port))
    finally:
        _rescue(port, [o])


def test_one_cancel_ends_two_opens_parked_with_the_same_object() -> None:
    oc = OpenCancel()
    port_a, a = _park(LISTENER_OPENS["Receiver.open"], oc)
    port_b, b = _park(LISTENER_OPENS["ManagedDemuxReceiver.from_url"], oc)
    try:
        oc.cancel()
        a.join("Receiver.open")
        b.join("ManagedDemuxReceiver.from_url")
        _assert_closed(a.error)
        _assert_closed(b.error)
    finally:
        _rescue(port_a, [a])
        _rescue(port_b, [b])


def test_repr_and_is_cancelled() -> None:
    oc = OpenCancel()
    assert not oc.is_cancelled()
    assert repr(oc) == "OpenCancel(cancelled=false)"
    oc.cancel()
    oc.cancel()  # idempotent
    assert oc.is_cancelled()
    assert repr(oc) == "OpenCancel(cancelled=true)"


# ---------------------------------------------------------------------------
# An object cancelled before the open: refused, nothing opened.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name", list(LISTENER_OPENS))
def test_a_cancelled_object_refuses_a_listener_open_and_binds_nothing(name: str) -> None:
    oc = OpenCancel()
    oc.cancel()
    port = free_udp_port()
    with pytest.raises(exceptions.SrtError) as raised:
        LISTENER_OPENS[name](f"srt://127.0.0.1:{port}?mode=listener", oc)
    _assert_closed(raised.value)
    assert not _udp_port_bound(port), "the refused open left its port bound"


# Every open that dials. `url` is a caller-mode URL to a listener the test owns.
CALLER_OPENS: dict[str, Callable[[str, OpenCancel], Any]] = {
    "Sender.open": lambda url, oc: shells.Sender.open(url, cancel=oc),
    "MuxSender.open": lambda url, oc: shells.MuxSender.open(url, video_only_program(), cancel=oc),
    "ManagedSender.from_url": lambda url, oc: srt.ManagedSender.from_url(url, cancel=oc),
    "ManagedMuxSender.from_url": lambda url, oc: srt.ManagedMuxSender.from_url(
        url, video_only_program(), cancel=oc
    ),
    "ManagedDemuxReceiver.from_url": lambda url, oc: srt.ManagedDemuxReceiver.from_url(
        url, cancel=oc
    ),
}


@pytest.mark.parametrize("name", list(CALLER_OPENS))
def test_a_cancelled_object_refuses_a_caller_open_without_dialling(name: str) -> None:
    """Nothing reaches the dial target: once the refused open has returned,
    the test sends a marker datagram to the same port, and the marker is
    the first datagram there. A dial would have sent its handshake before
    the open returned, so it would be queued ahead of the marker."""
    oc = OpenCancel()
    oc.cancel()
    target = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target.bind(("127.0.0.1", 0))
    target.settimeout(DEADLINE_S)
    port = target.getsockname()[1]
    try:
        with pytest.raises(exceptions.SrtError) as raised:
            CALLER_OPENS[name](f"srt://127.0.0.1:{port}?mode=caller&conntimeo=1000", oc)
        _assert_closed(raised.value)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.sendto(b"marker", ("127.0.0.1", port))
        first, _ = target.recvfrom(2048)
        assert first == b"marker", "the refused open dialled: its handshake arrived first"
    finally:
        target.close()


# A URL of each scheme an open would accept; the cancelled object refuses it
# before the transport is touched, with that transport's exception class.
SCHEMES = [
    ("srt", "srt://127.0.0.1:{port}?mode=listener", exceptions.SrtError),
    ("rtp", "rtp://127.0.0.1:{port}", exceptions.RtpError),
    ("udp", "udp://127.0.0.1:{port}", exceptions.UdpError),
    ("tcp", "tcp://127.0.0.1:{port}", exceptions.TcpError),
    ("tcps", "tcps://127.0.0.1:{port}", exceptions.TcpError),
    ("rist", "rist://@127.0.0.1:{port}", exceptions.RistError),
]


@pytest.mark.parametrize(("scheme", "url", "cls"), SCHEMES, ids=[s[0] for s in SCHEMES])
def test_a_cancelled_object_raises_the_schemes_closed(scheme: str, url: str, cls: type) -> None:
    oc = OpenCancel()
    oc.cancel()
    port = free_udp_port()
    for open_fn in (
        lambda u: shells.Receiver.open(u, cancel=oc),
        lambda u: shells.DemuxReceiver.open(u, cancel=oc),
        lambda u: shells.Sender.open(u, cancel=oc),
        lambda u: shells.MuxSender.open(u, video_only_program(), cancel=oc),
    ):
        with pytest.raises(cls) as raised:
            open_fn(url.format(port=port))
        _assert_closed(raised.value, cls)


def test_a_bad_url_with_a_cancelled_object_is_closed() -> None:
    """The object is checked before the URL is parsed."""
    oc = OpenCancel()
    oc.cancel()
    bad = "srt://127.0.0.1:notaport?mode=listener&no_such_key=1"
    for call in (
        lambda: shells.Receiver.open(bad, cancel=oc),
        lambda: shells.Sender.open(bad, cancel=oc),
        lambda: srt.ManagedReceiver.from_url(bad, cancel=oc),
        lambda: srt.ManagedSender.from_url(bad, cancel=oc),
        lambda: srt.ManagedDemuxReceiver.from_url(bad, cancel=oc),
        lambda: srt.ManagedMuxSender.from_url(bad, video_only_program(), cancel=oc),
        # The managed shells are SRT only, so even another scheme is SRT's CLOSED.
        lambda: srt.ManagedSender.from_url("udp://127.0.0.1:1", cancel=oc),
    ):
        with pytest.raises(exceptions.SrtError) as raised:
            call()
        _assert_closed(raised.value)


@pytest.mark.parametrize(
    "url", ["http://127.0.0.1:1", "udp://127.0.0.1:1", "not a url"], ids=["http", "udp", "garbage"]
)
@pytest.mark.parametrize(
    "call",
    [
        lambda url, oc: srt.ManagedSender.from_url(url, cancel=oc),
        lambda url, oc: srt.ManagedReceiver.from_url(url, cancel=oc),
        lambda url, oc: srt.ManagedMuxSender.from_url(url, video_only_program(), cancel=oc),
        lambda url, oc: srt.ManagedDemuxReceiver.from_url(url, cancel=oc),
    ],
    ids=["ManagedSender", "ManagedReceiver", "ManagedMuxSender", "ManagedDemuxReceiver"],
)
def test_a_managed_open_with_a_cancelled_object_is_srt_closed_whatever_the_url(
    call: Callable[[str, OpenCancel], Any], url: str
) -> None:
    """The managed shells are SRT only: no URL turns the refusal into a
    `ValueError` or another transport's class."""
    oc = OpenCancel()
    oc.cancel()
    with pytest.raises(exceptions.SrtError) as raised:
        call(url, oc)
    _assert_closed(raised.value)


def test_an_unsupported_scheme_is_a_value_error_even_with_a_cancelled_object() -> None:
    """A shell open with no transport for the scheme has no `CLOSED` to
    raise: the one exception to the object being checked first."""
    oc = OpenCancel()
    oc.cancel()
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        shells.Sender.open("http://127.0.0.1:1", cancel=oc)
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        shells.Receiver.open("http://127.0.0.1:1", cancel=oc)
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        shells.MuxSender.open("http://127.0.0.1:1", video_only_program(), cancel=oc)
    with pytest.raises(ValueError, match="unsupported URL scheme"):
        shells.DemuxReceiver.open("http://127.0.0.1:1", cancel=oc)


# ---------------------------------------------------------------------------
# The outcome rule: an open that finishes after the cancel reports CLOSED.
# ---------------------------------------------------------------------------


# An SRT control packet (top bit set) of type 5, SHUTDOWN.
_SRT_SHUTDOWN = b"\x80\x05"


class _HeldRelay:
    """A UDP relay between a caller and an SRT listener that holds the
    caller's first datagram until `release()`: the caller's dial is then
    provably in progress, and it completes only once released."""

    def __init__(self, target_port: int) -> None:
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.settimeout(0.05)
        self.port = self.sock.getsockname()[1]
        self.target = ("127.0.0.1", target_port)
        self.dial_seen = threading.Event()
        self.shutdown_seen = threading.Event()
        self._go = threading.Event()
        self._stop = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def release(self) -> None:
        self._go.set()

    def close(self) -> None:
        self._stop.set()
        self._go.set()
        self.thread.join(DEADLINE_S)
        self.sock.close()

    def _run(self) -> None:
        caller = None
        held: bytes | None = None
        while not self._stop.is_set():
            try:
                data, addr = self.sock.recvfrom(65536)
            except socket.timeout:
                if held is not None and self._go.is_set():
                    self.sock.sendto(held, self.target)
                    held = None
                continue
            except OSError:
                return
            if addr == self.target:
                if caller is not None:
                    self.sock.sendto(data, caller)
                continue
            caller = addr
            if data[:2] == _SRT_SHUTDOWN:
                self.shutdown_seen.set()
            if not self._go.is_set():
                held = data
                self.dial_seen.set()
                continue
            if held is not None:
                self.sock.sendto(held, self.target)
                held = None
            self.sock.sendto(data, self.target)


@pytest.mark.parametrize(
    "name",
    ["Sender.open", "ManagedSender.from_url", "ManagedDemuxReceiver.from_url"],
)
def test_a_dial_that_fails_after_the_cancel_reports_closed(name: str) -> None:
    """A cancel during a caller-mode dial does not interrupt it; the dial is
    then refused by the listener, and the open reports the cancel instead
    of the refusal. The relay holds the dial in progress until the cancel
    has run, and the connect timeout is far longer than the test, so the
    dial can only end by the refusal, after the cancel."""
    lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    lst.rules().set_default(srt.ListenRule.reject(srt.RejectReason.FORBIDDEN))
    relay = _HeldRelay(lst.local_addr()[1])
    oc = OpenCancel()
    url = f"srt://127.0.0.1:{relay.port}?mode=caller&conntimeo=60000"
    o = _Open(lambda: CALLER_OPENS[name](url, oc))
    try:
        assert relay.dial_seen.wait(DEADLINE_S), "the caller never dialled"
        assert not o.done.is_set()
        oc.cancel()
        relay.release()
        o.join(name)
        _assert_closed(o.error)
        assert o.result is None
    finally:
        relay.close()
        lst.close()
        o.thread.join(DEADLINE_S)
        o.close_result()


def test_a_dial_that_succeeds_after_the_cancel_is_closed_again() -> None:
    """The cancel lands while the dial is held in progress; the dial then
    connects, but the open raises CLOSED, closes the connection and hands
    nothing out."""
    lst = srt.Builder("srt://127.0.0.1:0?mode=listener").listen()
    relay = _HeldRelay(lst.local_addr()[1])
    oc = OpenCancel()
    o = _Open(lambda: shells.Sender.open(f"srt://127.0.0.1:{relay.port}?conntimeo=5000", cancel=oc))
    try:
        assert relay.dial_seen.wait(DEADLINE_S), "the caller never dialled"
        assert not o.done.is_set()
        oc.cancel()
        relay.release()
        o.join("Sender.open")
        _assert_closed(o.error)
        assert o.result is None
        # The dial did connect, and the open closed the connection again:
        # libsrt sends a SHUTDOWN only on a connected socket.
        assert relay.shutdown_seen.wait(DEADLINE_S), "no SHUTDOWN: the dial never connected"
    finally:
        o.thread.join(DEADLINE_S)
        o.close_result()
        relay.close()
        lst.close()


# Longer than a loopback connect takes even on a loaded box; the test
# fails, rather than passing, if the accept ends after the wake.
_HELD_WAKE_MS = 5000


@pytest.mark.parametrize("name", ["ManagedReceiver.from_url", "ManagedDemuxReceiver.from_url"])
def test_an_accept_that_succeeds_after_the_cancel_is_closed_again(name: str) -> None:
    """The object's `cancel()` sets the latch, then spends `_HELD_WAKE_MS`
    in an earlier open's wake before it reaches the managed receiver's
    accept. A peer connects meanwhile and the accept succeeds, but the open
    raises CLOSED and closes the connection again: the peer sees it end."""
    # Its one open in progress was registered first, so cancel() wakes it first.
    oc = shells_hook("_open_cancel_with_slow_open_for_test")(_HELD_WAKE_MS)
    port, o = _park(LISTENER_OPENS[name], oc)
    canceller = threading.Thread(target=oc.cancel, daemon=True)
    peer = None
    try:
        canceller.start()
        _wait_for("the latch", oc.is_cancelled)
        assert not o.done.is_set(), "the accept ended before a peer connected"

        def dial_and_recv() -> Any:
            dial = srt.Builder(f"srt://127.0.0.1:{port}").caller().connect_timeout_ms(3000)
            rx = dial.connect().into_receiver()
            try:
                return rx.recv_bytes()
            finally:
                rx.close()

        peer = _Open(dial_and_recv)
        o.join(name)
        # Still inside the held wake: only the peer can have ended the accept.
        assert canceller.is_alive(), "cancel() reached the accept before the open returned"
        _assert_closed(o.error)
        assert o.result is None
        peer.join("the peer")
        assert isinstance(peer.error, exceptions.SrtError), (
            f"the peer's connection did not end: {peer.error!r}"
        )
    finally:
        canceller.join(_HELD_WAKE_MS / 1000 + DEADLINE_S)
        _rescue(port, [o])
        o.close_result()
        if peer is not None:
            peer.thread.join(DEADLINE_S)
    assert not canceller.is_alive(), "cancel() did not return after the held wake"


def test_a_cancel_after_the_open_returned_leaves_the_shells_working() -> None:
    oc = OpenCancel()
    port, o = _park(LISTENER_OPENS["Receiver.open"], oc)
    tx = None
    try:
        tx = shells.Sender.open(f"srt://127.0.0.1:{port}", cancel=oc)
        o.join("Receiver.open")
        assert o.error is None, o.error
        rx = o.result
        oc.cancel()
        assert not rx.cancel_handle().is_cancelled()
        assert not tx.cancel_handle().is_cancelled()
        tx.send_bytes(TS_PACKET * 7)
        tx.flush()
        got = rx.recv_bytes()
        assert got[:1] == b"\x47" and len(got) == 188
    finally:
        _rescue(port, [o])
        if tx is not None:
            tx.close()
        o.close_result()


def _connect_with_retry(open_sender: Callable[[], Any]) -> Any:
    """Connect a caller, retrying while the managed listener re-binds."""
    end = time.monotonic() + DEADLINE_S
    while True:
        try:
            return open_sender()
        except exceptions.SrtError:
            if time.monotonic() > end:
                raise
            time.sleep(0.05)


def test_a_cancel_after_a_managed_receiver_opened_leaves_its_reconnects_going() -> None:
    oc = OpenCancel()
    port, o = _park(LISTENER_OPENS["ManagedReceiver.from_url"], oc)
    caller = f"srt://127.0.0.1:{port}"
    tx = None
    got: list[bytes] = []
    pump_err: list[BaseException] = []
    pump = None
    try:
        tx = srt.ManagedSender.from_url(caller)
        o.join("ManagedReceiver.from_url")
        assert o.error is None, o.error
        rx = o.result
        oc.cancel()
        assert not rx.cancel_handle().is_cancelled()

        def run() -> None:
            try:
                while True:
                    got.append(rx.recv_bytes())
            except BaseException as exc:  # noqa: BLE001
                pump_err.append(exc)

        pump = threading.Thread(target=run, daemon=True)
        pump.start()
        tx.send_bytes(TS_PACKET * 7)
        tx.flush()
        _wait_for("data before the reconnect", lambda: len(got) > 0 or bool(pump_err))
        assert not pump_err, pump_err

        # The peer leaves; the receiver re-accepts on the slot the object no
        # longer reaches, and a new peer's data arrives.
        tx.close()
        tx = None
        _wait_for("a reconnect attempt", lambda: rx.reconnect_attempts() >= 1 or bool(pump_err))
        before = len(got)
        tx = _connect_with_retry(lambda: srt.ManagedSender.from_url(caller))

        def more() -> bool:
            if len(got) > before or pump_err:
                return True
            tx.send_bytes(TS_PACKET * 7)
            tx.flush()
            return False

        _wait_for("data after the reconnect", more)
        assert not pump_err, pump_err
        rx.close()
        pump.join(DEADLINE_S)
        assert not pump.is_alive()
    finally:
        _rescue(port, [o])
        if tx is not None:
            tx.close()
        o.close_result()


# ---------------------------------------------------------------------------
# Without the argument nothing changes.
# ---------------------------------------------------------------------------


def test_cancel_none_is_the_same_as_no_argument() -> None:
    port = free_udp_port()
    rx = shells.Receiver.open(f"udp://127.0.0.1:{port}", cancel=None)
    tx = shells.Sender.open(f"udp://127.0.0.1:{port}", cancel=None)
    try:
        tx.send_bytes(TS_PACKET * 7)
        tx.flush()
        assert rx.recv_bytes()[:1] == b"\x47"
    finally:
        tx.close()
        rx.close()


def test_the_argument_is_keyword_only() -> None:
    with pytest.raises(TypeError):
        shells.Sender.open("udp://127.0.0.1:1", OpenCancel())  # type: ignore[misc]


def test_open_cancel_is_exported() -> None:
    assert "OpenCancel" in shells.__all__
    assert OpenCancel.__module__ == "tstrans.shells"
    with pytest.raises(TypeError):

        class _Sub(OpenCancel):  # type: ignore[misc]
            pass
