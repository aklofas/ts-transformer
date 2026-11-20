"""The receiving shells, `rist.RecvTransport` and `udp.RecvTransport`
report the address they bound.

A shell answers from what its open recorded: `udp://` and `rist://@`
binds, `rtp://` binds, and an `srt://` listener once its caller
connected. A shell whose opener records no address answers `None`. The
answer never waits behind a receive in progress and raises `CLOSED` once
the shell is closed.
"""

from __future__ import annotations

import socket
import threading
import time

import pytest

from tstrans import rist, shells, udp
from tstrans.exceptions import (
    RistError,
    RistErrorKind,
    RtpError,
    RtpErrorKind,
    UdpError,
    UdpErrorKind,
)

from _builders.hang_deadline import call_with_hang_deadline
from _builders.mux_programs import video_only_program
from _builders.ports import free_udp_port

WAIT_S = 10.0
SHELLS = [shells.Receiver, shells.DemuxReceiver]


@pytest.mark.parametrize("cls", SHELLS)
def test_shell_local_port_is_the_bound_port(cls) -> None:
    """Port 0 in the URL: the shell reports the kernel's port, and a
    datagram sent to it is what the shell reads."""
    with cls.open("udp://127.0.0.1:0") as rx:
        port = rx.local_port()
        assert isinstance(port, int) and port != 0
        assert rx.local_addr() == ("127.0.0.1", port)
        if cls is shells.Receiver:
            null = bytes([0x47, 0x1F, 0xFF, 0x10]) + b"\xff" * 184
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as tx:
                tx.sendto(null * 7, ("127.0.0.1", port))
            assert rx.recv_bytes() == null


def test_shell_local_addr_over_rtp_and_srt() -> None:
    with shells.DemuxReceiver.open("rtp://127.0.0.1:0") as rx:
        port = rx.local_port()
        assert isinstance(port, int) and port != 0
        assert rx.local_addr() == ("127.0.0.1", port)

    # An srt:// listener's address exists once its caller has connected.
    port = free_udp_port()
    box: list[object] = []

    def accept() -> None:
        try:
            box.append(shells.DemuxReceiver.open(f"srt://127.0.0.1:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            box.append(exc)

    t = threading.Thread(target=accept, daemon=True)
    t.start()
    sender = None
    deadline = time.monotonic() + WAIT_S
    while sender is None:
        try:
            sender = shells.MuxSender.open(f"srt://127.0.0.1:{port}?mode=caller", video_only_program())
        except Exception:  # noqa: BLE001 - the listener may not be up yet
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)
    try:
        t.join(WAIT_S)
        assert not t.is_alive()
        rx = box[0]
        assert isinstance(rx, shells.DemuxReceiver), rx
        try:
            assert rx.local_port() == port
            assert rx.local_addr() == ("127.0.0.1", port)
        finally:
            rx.close()
    finally:
        sender.close()


def test_a_shell_whose_open_records_no_address_answers_none() -> None:
    """A receiver built from a TCP connection has no bound address of its own."""
    from tstrans import tcp

    with tcp.Listener.open("tcp://127.0.0.1:0?listen=1") as lst:
        conn = socket.create_connection(("127.0.0.1", lst.local_port()))
        try:
            rx = lst.accept_blocking().into_receiver()
            try:
                assert rx.local_addr() is None
                assert rx.local_port() is None
            finally:
                rx.close()
        finally:
            conn.close()


@pytest.mark.parametrize("cls", SHELLS)
@pytest.mark.parametrize("url,err,kind", [
    ("udp://127.0.0.1:0", UdpError, UdpErrorKind.CLOSED),
    ("rtp://127.0.0.1:0", RtpError, RtpErrorKind.CLOSED),
])
def test_shell_local_addr_after_close_raises_closed(cls, url, err, kind) -> None:
    rx = cls.open(url)
    assert rx.local_port() != 0
    rx.close()
    with pytest.raises(err) as ei:
        rx.local_addr()
    assert ei.value.kind == kind
    with pytest.raises(err) as ei:
        rx.local_port()
    assert ei.value.kind == kind


@pytest.mark.parametrize("cls", SHELLS)
def test_local_port_answers_while_a_receive_is_parked(cls) -> None:
    """Over `rtp://` a malformed datagram is counted only inside a receive
    call, so the count proves the receive holds the shell when
    `local_port()` answers."""
    rx = cls.open("rtp://127.0.0.1:0")
    port = rx.local_port()

    def park() -> None:
        try:
            if cls is shells.Receiver:
                rx.recv_bytes()
            else:
                next(rx)
        except RtpError:
            pass

    t = threading.Thread(target=park, daemon=True)
    t.start()
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as tx:
            tx.sendto(b"\x40" + b"\x00" * 199, ("127.0.0.1", port))
        deadline = time.monotonic() + WAIT_S
        while rx.rtp_stats().malformed_packets < 1:
            assert time.monotonic() < deadline, "the receive never ran"
            time.sleep(0.005)
        blocked, outcome = call_with_hang_deadline(rx.local_port, 5.0)
        still_parked = t.is_alive()
    finally:
        rx.close()
        t.join(WAIT_S)
    assert not blocked, "local_port() waited behind the parked receive"
    assert outcome["value"] == port
    assert still_parked


@pytest.mark.parametrize("cls", SHELLS)
def test_local_port_racing_close_never_fails(cls) -> None:
    for _ in range(20):
        rx = cls.open("udp://127.0.0.1:0")
        port = rx.local_port()
        answers: list[object] = []
        errs: list[BaseException] = []

        def read() -> None:
            while True:
                try:
                    answers.append(rx.local_port())
                except UdpError as exc:
                    if exc.kind == UdpErrorKind.CLOSED:
                        return
                    errs.append(exc)
                    return
                except BaseException as exc:  # noqa: BLE001
                    errs.append(exc)
                    return

        t = threading.Thread(target=read, daemon=True)
        t.start()
        while not answers and t.is_alive():
            time.sleep(0.001)
        rx.close()
        t.join(WAIT_S)
        assert not t.is_alive(), "the reader did not see the close"
        assert not errs, errs
        assert answers and all(a == port for a in answers)


def test_rist_main_profile_port_zero_reports_its_port() -> None:
    """Port 0 works in the Main profile (the default): the kernel's port
    is reported, by the transport and by a shell over the same URL."""
    rx = rist.RecvTransport.builder().bind_url("rist://@127.0.0.1:0").build()
    try:
        port = rx.local_port()
        assert isinstance(port, int) and port != 0
        assert rx.local_addr() == ("127.0.0.1", port)
    finally:
        rx.close()
    with pytest.raises(RistError) as ei:
        rx.local_port()
    assert ei.value.kind == RistErrorKind.CLOSED
    with shells.Receiver.open("rist://@127.0.0.1:0") as srx:
        p = srx.local_port()
        assert isinstance(p, int) and p != 0
        assert srx.local_addr() == ("127.0.0.1", p)


def test_udp_local_addr_and_local_port() -> None:
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    try:
        port = rx.local_port()
        assert isinstance(port, int) and port != 0
        assert rx.local_addr() == ("127.0.0.1", port)
    finally:
        rx.close()
    for getter in (rx.local_addr, rx.local_port):
        with pytest.raises(UdpError) as ei:
            getter()
        assert ei.value.kind == UdpErrorKind.CLOSED


def test_udp_local_addr_port_warns_and_equals_local_port() -> None:
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    try:
        with pytest.warns(DeprecationWarning, match=r"local_addr_port\(\).*local_port\(\).*removed in 0\.12\.0") as rec:
            old = rx.local_addr_port()
        assert len(rec) == 1
        assert rec[0].filename == __file__
        assert old == rx.local_port()
    finally:
        rx.close()
