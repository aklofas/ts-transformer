"""One lifecycle contract for every shell over every transport.

Each shell (`Sender`, `Receiver`, `MuxSender`, `DemuxReceiver`) is opened
from an `srt://`, `rtp://`, `udp://`, `tcp://` or `rist://` URL against a
live peer, and must honour the same rules whichever transport it landed on:

1. `close()` twice raises nothing.
2. After `close()`, a send or recv raises the transport's own exception
   class (exactly, not a base class) with kind `CLOSED`.
3. A receive parked on a thread ends within 5 s of `cancel_handle().cancel()`
   from another thread, raising that class with kind `CLOSED`.
4. While a receive is parked, `is_alive()` and `cancel_handle()` each
   return within 1 s. `socket_stats()` reads the counters held inside the
   shell, so it waits for the parked call by design; it answers within 1 s
   once the cancel has ended that call.
5. `srt` and `tcp` senders: once the peer is gone, further sends end in
   `BROKEN` or `CLOSED`. libsrt notices a closed peer only through its 5 s
   peer-idle timeout, so the window is 15 s.

Test ids read `{shell}-{scheme}-r{rule}`. A `MuxSender` and a
`DemuxReceiver` repeat the rules over the same peers as `Sender` and
`Receiver`. A `rist` case skips only when librist cannot bind an even
loopback port.
"""

from __future__ import annotations

import contextlib
import socket
import threading
import time
from typing import Callable, Iterator

import pytest

from tstrans import shells, tcp
from tstrans.exceptions import (
    RistError,
    RistErrorKind,
    RtpError,
    RtpErrorKind,
    SrtError,
    SrtErrorKind,
    TcpError,
    TcpErrorKind,
    UdpError,
    UdpErrorKind,
)
from tstrans.mpegts import Pts90khz

from _builders.hang_deadline import call_with_hang_deadline
from _builders.mux_programs import video_only_program
from _builders.ports import free_tcp_port, free_udp_port
from _builders.rist_ports import bind_even_port_or_skip

SCHEMES = ("srt", "rtp", "udp", "tcp", "rist")
ERRORS = {
    "srt": (SrtError, SrtErrorKind),
    "rtp": (RtpError, RtpErrorKind),
    "udp": (UdpError, UdpErrorKind),
    "tcp": (TcpError, TcpErrorKind),
    "rist": (RistError, RistErrorKind),
}
SENDERS = ("Sender", "MuxSender")
RECEIVERS = ("Receiver", "DemuxReceiver")

TS_PACKET = b"\x47" + b"\x00" * 187
TS_BUNDLE = TS_PACKET * 6
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"

_PEER_WAIT_S = 10.0


# --------------------------------------------------------------------------- #
# Shell operations                                                            #
# --------------------------------------------------------------------------- #


def _open_sender(shell: str, url: str) -> object:
    if shell == "Sender":
        return shells.Sender.open(url)
    return shells.MuxSender.open(url, video_only_program())


def _open_receiver(shell: str, url: str) -> object:
    return getattr(shells, shell).open(url)


def _send_once(shell: str, tx: object) -> None:
    if shell == "Sender":
        tx.send_bytes(TS_BUNDLE)
    else:
        tx.send_video(NAL_IDR, pts=Pts90khz.from_raw(0), key_frame=True)


def _recv_once(shell: str, rx: object) -> object:
    if shell == "Receiver":
        return rx.recv_bytes()
    return next(rx)


# --------------------------------------------------------------------------- #
# Live peers                                                                  #
# --------------------------------------------------------------------------- #


def _retry_open(open_fn: Callable[[], object], err: type[BaseException]) -> object:
    """Open a caller against a listener that may not be accepting yet."""
    end = time.monotonic() + _PEER_WAIT_S
    while True:
        try:
            return open_fn()
        except err:
            if time.monotonic() > end:
                raise
            time.sleep(0.05)


@contextlib.contextmanager
def _receiver_with_peer(shell: str, scheme: str) -> Iterator[object]:
    """Yield an open receiver shell with a live, silent peer attached."""
    cleanup: list[Callable[[], None]] = []
    try:
        if scheme == "srt":
            port = free_tcp_port()
            box: list[object] = []
            errs: list[BaseException] = []

            def accept() -> None:
                try:
                    box.append(_open_receiver(shell, f"srt://:{port}?mode=listener"))
                except BaseException as exc:  # noqa: BLE001
                    errs.append(exc)

            t = threading.Thread(target=accept, daemon=True)
            t.start()
            peer = _retry_open(
                lambda: shells.Sender.open(f"srt://127.0.0.1:{port}?mode=caller"), SrtError
            )
            cleanup.append(peer.close)
            t.join(_PEER_WAIT_S)
            assert not t.is_alive() and box, f"srt listener did not accept: {errs!r}"
            rx = box[0]
        elif scheme in ("rtp", "udp"):
            rx = _open_receiver(shell, f"{scheme}://127.0.0.1:{free_udp_port()}")
        elif scheme == "tcp":
            listener = tcp.Listener.builder().bind("127.0.0.1:0").build()
            cleanup.append(listener.close)
            held: list[object] = []

            def accept_one() -> None:
                with contextlib.suppress(BaseException):
                    held.append(listener.accept_blocking())

            threading.Thread(target=accept_one, daemon=True).start()
            rx = _open_receiver(shell, f"tcp://127.0.0.1:{listener.local_port()}")
            cleanup.append(lambda: [h.close() for h in held])
        else:
            rx, _ = bind_even_port_or_skip(lambda p: _open_receiver(shell, f"rist://@127.0.0.1:{p}"))
        cleanup.append(rx.close)
        yield rx
    finally:
        for fn in reversed(cleanup):
            with contextlib.suppress(BaseException):
                fn()


@contextlib.contextmanager
def _sender_with_peer(shell: str, scheme: str) -> Iterator[tuple[object, Callable[[], None]]]:
    """Yield `(sender, drop_peer)`: an open sender shell with a live peer
    that drains it, and a callable that makes the peer go away."""
    cleanup: list[Callable[[], None]] = []
    try:
        if scheme == "srt":
            port = free_tcp_port()
            box: list[object] = []

            def accept() -> None:
                with contextlib.suppress(BaseException):
                    box.append(shells.Receiver.open(f"srt://:{port}?mode=listener"))

            t = threading.Thread(target=accept, daemon=True)
            t.start()
            tx = _retry_open(
                lambda: _open_sender(shell, f"srt://127.0.0.1:{port}?mode=caller"), SrtError
            )
            t.join(_PEER_WAIT_S)
            assert box, "srt listener did not accept"
            peer = box[0]
            cleanup.append(peer.close)

            drop = peer.close

        elif scheme in ("rtp", "udp"):
            sink = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sink.bind(("127.0.0.1", 0))
            cleanup.append(sink.close)
            tx = _open_sender(shell, f"{scheme}://127.0.0.1:{sink.getsockname()[1]}")
            drop = sink.close
        elif scheme == "tcp":
            server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            server.bind(("127.0.0.1", 0))
            server.listen(1)
            conns: list[socket.socket] = []
            cleanup.append(server.close)

            def drain() -> None:
                conn, _ = server.accept()
                conns.append(conn)
                with contextlib.suppress(OSError):
                    while conn.recv(65536):
                        pass

            threading.Thread(target=drain, daemon=True).start()
            tx = _open_sender(shell, f"tcp://127.0.0.1:{server.getsockname()[1]}")

            def drop() -> None:
                end = time.monotonic() + _PEER_WAIT_S
                while not conns and time.monotonic() < end:
                    time.sleep(0.01)
                for c in conns:
                    c.close()
                server.close()

        else:
            peer, port = bind_even_port_or_skip(lambda p: shells.Receiver.open(f"rist://@127.0.0.1:{p}"))
            cleanup.append(peer.close)
            tx = _open_sender(shell, f"rist://127.0.0.1:{port}")
            drop = peer.close
        cleanup.append(tx.close)
        yield tx, drop
    finally:
        for fn in reversed(cleanup):
            with contextlib.suppress(BaseException):
                fn()


def _cases(shell_names: tuple[str, ...], rule: int, schemes: tuple[str, ...] = SCHEMES):
    return [
        pytest.param(shell, scheme, id=f"{shell}-{scheme}-r{rule}")
        for shell in shell_names
        for scheme in schemes
    ]


# --------------------------------------------------------------------------- #
# The rules                                                                   #
# --------------------------------------------------------------------------- #


def _with_peer(shell: str, scheme: str):
    if shell in SENDERS:
        return _sender_with_peer(shell, scheme)
    return _receiver_with_peer(shell, scheme)


@pytest.mark.parametrize(("shell", "scheme"), _cases(SENDERS + RECEIVERS, 1))
def test_close_twice_raises_nothing(shell: str, scheme: str) -> None:
    with _with_peer(shell, scheme) as got:
        obj = got[0] if shell in SENDERS else got
        obj.close()
        obj.close()
        assert obj.is_alive() is False


@pytest.mark.parametrize(("shell", "scheme"), _cases(SENDERS + RECEIVERS, 2))
def test_use_after_close_raises_the_transports_closed_error(shell: str, scheme: str) -> None:
    error_class, kind_enum = ERRORS[scheme]
    with _with_peer(shell, scheme) as got:
        if shell in SENDERS:
            obj = got[0]
            obj.close()
            with pytest.raises(error_class) as e:
                _send_once(shell, obj)
        else:
            obj = got
            obj.close()
            with pytest.raises(error_class) as e:
                _recv_once(shell, obj)
        assert type(e.value) is error_class
        assert e.value.kind == kind_enum.CLOSED


def _park_recv(shell: str, rx: object) -> tuple[threading.Thread, list[BaseException]]:
    outcome: list[BaseException] = []

    def parked() -> None:
        try:
            while True:
                try:
                    _recv_once(shell, rx)
                except RistError as exc:
                    # librist's receive ticks every 100 ms with BACKPRESSURE
                    # while no packet arrives; the recv is still waiting.
                    if exc.kind != RistErrorKind.BACKPRESSURE:
                        raise
                    continue
                outcome.append(AssertionError("a silent peer delivered data"))
                return
        except BaseException as exc:  # noqa: BLE001
            outcome.append(exc)

    t = threading.Thread(target=parked, daemon=True)
    t.start()
    time.sleep(0.3)
    return t, outcome


@pytest.mark.parametrize(("shell", "scheme"), _cases(RECEIVERS, 3))
def test_cancel_ends_a_parked_recv_with_closed(shell: str, scheme: str) -> None:
    error_class, kind_enum = ERRORS[scheme]
    with _receiver_with_peer(shell, scheme) as rx:
        t, outcome = _park_recv(shell, rx)
        rx.cancel_handle().cancel()
        t.join(5.0)
        assert not t.is_alive(), "the parked recv did not end within 5 s of cancel()"
        assert len(outcome) == 1
        err = outcome[0]
        assert type(err) is error_class, repr(err)
        assert err.kind == kind_enum.CLOSED


@pytest.mark.parametrize(("shell", "scheme"), _cases(RECEIVERS, 4))
def test_accessors_do_not_wait_behind_a_parked_recv(shell: str, scheme: str) -> None:
    with _receiver_with_peer(shell, scheme) as rx:
        t, outcome = _park_recv(shell, rx)
        try:
            results = {
                name: call_with_hang_deadline(getter, 1.0)
                for name, getter in (
                    ("is_alive", rx.is_alive),
                    ("cancel_handle", rx.cancel_handle),
                )
            }
        finally:
            rx.cancel_handle().cancel()
            t.join(5.0)
        for name, (blocked, got) in results.items():
            assert not blocked, f"{name}() waited behind the parked recv"
            assert "exc" not in got, f"{name}() raised {got.get('exc')!r}"
        assert not t.is_alive()
        blocked, got = call_with_hang_deadline(rx.socket_stats, 1.0)
        assert not blocked, "socket_stats() still waited after the cancel ended the recv"
        assert "exc" not in got, f"socket_stats() raised {got.get('exc')!r}"


@pytest.mark.parametrize(
    ("shell", "scheme"), _cases(SENDERS, 5, schemes=("srt", "tcp"))
)
def test_sends_end_broken_or_closed_once_the_peer_is_gone(shell: str, scheme: str) -> None:
    error_class, kind_enum = ERRORS[scheme]
    with _sender_with_peer(shell, scheme) as (tx, drop_peer):
        _send_once(shell, tx)
        drop_peer()

        def send_until_it_fails() -> BaseException | None:
            end = time.monotonic() + 15.0
            while time.monotonic() < end:
                try:
                    _send_once(shell, tx)
                except BaseException as exc:  # noqa: BLE001
                    return exc
                time.sleep(0.02)
            return None

        blocked, got = call_with_hang_deadline(send_until_it_fails, 25.0)
        assert not blocked, "a send hung after the peer went away"
        err = got.get("value")
        assert err is not None, "sends kept succeeding for 15 s after the peer went away"
        assert type(err) is error_class, repr(err)
        assert err.kind in (kind_enum.BROKEN, kind_enum.CLOSED)
