"""The receivers report the publisher's address and what the source pin
turned away.

`udp.RecvTransport.recv()` returns the datagram with its sender's address;
`peer_addr()` names the sender delivered last on the UDP and RIST receivers
and on the receiving shells; `source_rejected` counts what a `source` pin
refused. Two-sender tests send the second sender's datagrams from
127.0.0.2 (or, where that cannot be bound, from `::1` to a dual-stack
receiver); on Linux a missing second address fails the test.
"""

from __future__ import annotations

import select
import socket
import sys
import threading
import time
from typing import Callable, Optional

import pytest

from tstrans import rist, shells, udp
from tstrans.exceptions import RistError, RistErrorKind, UdpError, UdpErrorKind

from _builders.ports import free_udp_port
from _builders.rist_ports import bind_even_port_or_skip

#: Every wait in this file.
WAIT_S = 15.0
#: Items a negative claim ("nothing from B was delivered") is judged over.
ENOUGH = 200
TAG_A = 0xA1
TAG_B = 0xB2


def _ts_chunk(tag: int, packets: int = 7) -> bytes:
    """`packets` TS packets, each carrying `tag` in its second byte."""
    pkt = bytes([0x47, tag]) + b"\xff" * 186
    return pkt * packets


def _wait_for(what: str, cond: Callable[[], bool]) -> None:
    deadline = time.monotonic() + WAIT_S
    while not cond():
        if time.monotonic() >= deadline:
            pytest.fail(f"{what}: not seen within {WAIT_S} s")
        time.sleep(0.005)


def _finishes(fn: Callable[[], object], seconds: float = 2.0) -> object:
    """Run `fn` on its own thread; fail unless it returns within `seconds`."""
    out: list[object] = []
    t = threading.Thread(target=lambda: out.append(fn()), daemon=True)
    t.start()
    t.join(seconds)
    assert not t.is_alive(), "the getter waited behind the parked call"
    return out[0]


class _TwoSenders:
    """Two UDP sockets that reach one receiver from two IP addresses.

    `bind_host` is the host to put in the receiver's URL; `send(sock, data,
    port)` sends to the receiver on `port`.
    """

    def __init__(self) -> None:
        self.a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.a.bind(("127.0.0.1", 0))
        self.a_addr = ("127.0.0.1", self.a.getsockname()[1])
        try:
            b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            b.bind(("127.0.0.2", 0))
            self.b, self.b_dest, self.bind_host = b, "127.0.0.1", "127.0.0.1"
        except OSError as v4_err:
            b.close()
            if sys.platform.startswith("linux"):
                pytest.fail(f"no second loopback address: {v4_err}")
            try:
                b = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
                b.bind(("::1", 0))
            except OSError as v6_err:
                pytest.skip(f"neither 127.0.0.2 nor ::1 can be bound: {v4_err}; {v6_err}")
            self.b, self.b_dest, self.bind_host = b, "::1", "[::]"
        self.b_addr = (self.b.getsockname()[0], self.b.getsockname()[1])

    def send_a(self, data: bytes, port: int) -> None:
        self.a.sendto(data, ("127.0.0.1", port))

    def send_b(self, data: bytes, port: int) -> None:
        self.b.sendto(data, (self.b_dest, port))

    def close(self) -> None:
        self.a.close()
        self.b.close()


# ---------------------------------------------------------------------------
# udp.RecvTransport
# ---------------------------------------------------------------------------


def test_udp_recv_returns_the_senders_address() -> None:
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        tx.bind(("127.0.0.1", 0))
        tx.sendto(b"hello", ("127.0.0.1", rx.local_addr_port()))
        data, (host, port) = rx.recv(timeout_ms=2000)
        assert data == b"hello"
        assert host == "127.0.0.1"
        assert port == tx.getsockname()[1]
    finally:
        tx.close()
        rx.close()


def test_udp_peer_addr_none_then_address() -> None:
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        assert rx.peer_addr() is None
        tx.bind(("127.0.0.1", 0))
        tx.sendto(b"x", ("127.0.0.1", rx.local_addr_port()))
        rx.recv(timeout_ms=2000)
        assert rx.peer_addr() == ("127.0.0.1", tx.getsockname()[1])
    finally:
        tx.close()
        rx.close()


def test_udp_recv_names_the_sender_of_an_empty_datagram() -> None:
    # recv() names an empty datagram's sender, which does not become the peer.
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        a.bind(("127.0.0.1", 0))
        b.bind(("127.0.0.1", 0))
        a.sendto(b"x", ("127.0.0.1", rx.local_addr_port()))
        assert rx.recv(timeout_ms=2000) == (b"x", ("127.0.0.1", a.getsockname()[1]))
        b.sendto(b"", ("127.0.0.1", rx.local_addr_port()))
        assert rx.recv(timeout_ms=2000) == (b"", ("127.0.0.1", b.getsockname()[1]))
        assert rx.peer_addr() == ("127.0.0.1", a.getsockname()[1])
    finally:
        a.close()
        b.close()
        rx.close()


def test_udp_builder_source_first_drops_and_counts() -> None:
    pair = _TwoSenders()
    rx = (
        udp.RecvTransport.builder()
        .bind_url(f"udp://{pair.bind_host}:0")
        .source("first")
        .build()
    )
    try:
        port = rx.local_addr_port()
        # A delivers first and takes the lock; B is refused from then on.
        pair.send_a(bytes([TAG_A]), port)
        assert rx.recv(timeout_ms=2000)[0] == bytes([TAG_A])
        tags: list[int] = []
        while len(tags) < ENOUGH:
            pair.send_b(bytes([TAG_B]), port)
            pair.send_a(bytes([TAG_A]), port)
            data, (host, _) = rx.recv(timeout_ms=2000)
            assert host == "127.0.0.1"
            tags.append(data[0])
        assert TAG_B not in tags
        assert rx.stats().source_rejected > 0
        assert rx.peer_addr() == pair.a_addr
    finally:
        rx.close()
        pair.close()


def test_udp_builder_source_refuses_a_bad_value_like_the_url() -> None:
    with pytest.raises(UdpError) as from_url:
        udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0?source=nowhere").build()
    with pytest.raises(UdpError) as from_builder:
        udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").source("nowhere")
    assert from_builder.value.kind == from_url.value.kind == UdpErrorKind.URL
    assert str(from_builder.value) == str(from_url.value)


def test_udp_stats_has_source_rejected() -> None:
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    try:
        assert rx.stats().source_rejected == 0
    finally:
        rx.close()


# ---------------------------------------------------------------------------
# rist.RecvTransport
# ---------------------------------------------------------------------------


def _open_rist_rx(query: str = "", source: Optional[str] = None) -> tuple[rist.RecvTransport, int]:
    def open_fn(port: int) -> rist.RecvTransport:
        b = rist.RecvTransport.builder().bind_url(f"rist://@0.0.0.0:{port}?buffer=100{query}")
        if source is not None:
            b = b.source(source)
        return b.build()

    rx, port = bind_even_port_or_skip(open_fn)
    return rx, port  # type: ignore[return-value]


class _RistTx:
    """A RIST sender on its own thread sending one tagged chunk every 5 ms."""

    def __init__(self, port: int, tag: int) -> None:
        self.tx = rist.Transport.builder().url(f"rist://127.0.0.1:{port}?buffer=100").build()
        self.stop = threading.Event()
        self.errors: list[BaseException] = []
        chunk = _ts_chunk(tag)

        def run() -> None:
            while not self.stop.is_set():
                try:
                    self.tx.send(chunk)
                except RistError as e:
                    if e.kind != RistErrorKind.BACKPRESSURE:
                        self.errors.append(e)
                        return
                time.sleep(0.005)

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.stop.set()
        self.thread.join(5)
        self.tx.close()
        assert not self.errors, self.errors


class _Relay:
    """A two-way UDP relay: a sender talks to `front_port` on 127.0.0.1, and
    the receiver sees its datagrams come from 127.0.0.2."""

    def __init__(self, to_port: int) -> None:
        self.back = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            self.back.bind(("127.0.0.2", 0))
        except OSError as e:
            self.back.close()
            if sys.platform.startswith("linux"):
                pytest.fail(f"no second loopback address: {e}")
            pytest.skip(f"127.0.0.2 cannot be bound here: {e}")
        self.front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.front.bind(("127.0.0.1", 0))
        self.front_port = self.front.getsockname()[1]
        self.forwarded = 0
        self.stop = threading.Event()
        to = ("127.0.0.1", to_port)

        def run() -> None:
            sender = None
            while not self.stop.is_set():
                ready, _, _ = select.select([self.front, self.back], [], [], 0.01)
                for s in ready:
                    data, addr = s.recvfrom(65536)
                    if s is self.front:
                        sender = addr
                        self.back.sendto(data, to)
                        self.forwarded += 1
                    elif sender is not None:
                        self.front.sendto(data, sender)

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.stop.set()
        self.thread.join(5)
        self.front.close()
        self.back.close()


def _rist_read_tags(rx: rist.RecvTransport, done: Callable[[list[int]], bool]) -> list[int]:
    deadline = time.monotonic() + WAIT_S
    tags: list[int] = []
    while not done(tags):
        if time.monotonic() >= deadline:
            pytest.fail(
                f"{len(tags)} blocks read in {WAIT_S} s; rejected {rx.stats().source_rejected}"
            )
        try:
            block = rx.recv(timeout_ms=200)
        except RistError as e:
            if e.kind == RistErrorKind.BACKPRESSURE:
                continue
            raise
        tags.append(block[1])
    return tags


def test_rist_peer_addr() -> None:
    rx, port = _open_rist_rx()
    try:
        assert rx.peer_addr() is None
        tx = _RistTx(port, TAG_A)
        try:
            tags = _rist_read_tags(rx, lambda t: len(t) > 0)
            assert tags[0] == TAG_A
            peer = rx.peer_addr()
            assert peer is not None
            host, peer_port = peer
            assert host == "127.0.0.1"
            assert isinstance(peer_port, int) and peer_port != 0
        finally:
            tx.close()
    finally:
        rx.close()


def test_rist_builder_source_first() -> None:
    rx, port = _open_rist_rx(source="first")
    relay = _Relay(port)
    try:
        a = _RistTx(port, TAG_A)
        try:
            _rist_read_tags(rx, lambda t: len(t) > 0)
            b = _RistTx(relay.front_port, TAG_B)
            try:
                tags = _rist_read_tags(
                    rx, lambda t: len(t) >= ENOUGH and rx.stats().source_rejected > 0
                )
                assert TAG_B not in tags, f"{tags.count(TAG_B)} of {len(tags)} blocks are B's"
                peer = rx.peer_addr()
                assert peer is not None and peer[0] == "127.0.0.1"
            finally:
                b.close()
        finally:
            a.close()
    finally:
        relay.close()
        rx.close()


def test_rist_builder_source_refuses_a_bad_value_like_the_url() -> None:
    with pytest.raises(RistError) as from_url:
        rist.RecvTransport.builder().bind_url("rist://@127.0.0.1:34200?source=nowhere").build()
    with pytest.raises(RistError) as from_builder:
        rist.RecvTransport.builder().bind_url("rist://@127.0.0.1:34200").source("nowhere")
    assert from_builder.value.kind == from_url.value.kind == RistErrorKind.URL
    assert str(from_builder.value) == str(from_url.value)


# ---------------------------------------------------------------------------
# The receiving shells
# ---------------------------------------------------------------------------


def test_shell_receiver_peer_addr_udp() -> None:
    port = free_udp_port()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.bind(("127.0.0.1", 0))
    try:
        with shells.Receiver.open(f"udp://127.0.0.1:{port}") as rx:
            assert rx.peer_addr() is None
            assert rx.source_rejected() == 0
            tx.sendto(_ts_chunk(TAG_A), ("127.0.0.1", port))
            pkt = rx.recv_bytes()
            assert pkt[1] == TAG_A
            assert rx.peer_addr() == ("127.0.0.1", tx.getsockname()[1])
    finally:
        tx.close()


def _iterate(rx: shells.DemuxReceiver, errors: list[BaseException]) -> None:
    """Iterate until the receiver ends; a `close()` mid-wait raises CLOSED."""
    try:
        for _ in rx:
            pass
    except UdpError as e:
        if e.kind != UdpErrorKind.CLOSED:
            errors.append(e)
    except BaseException as e:  # noqa: BLE001
        errors.append(e)


def test_shell_demux_receiver_peer_addr_udp() -> None:
    port = free_udp_port()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.bind(("127.0.0.1", 0))
    rx = shells.DemuxReceiver.open(f"udp://127.0.0.1:{port}")
    errors: list[BaseException] = []
    it = threading.Thread(target=_iterate, args=(rx, errors), daemon=True)
    it.start()
    try:
        assert rx.peer_addr() is None
        assert rx.source_rejected() == 0
        # Null-like packets: the demuxer is fed but emits no event, so the
        # iteration stays parked while the address is read.
        _wait_for(
            "the sender's address",
            lambda: tx.sendto(_ts_chunk(TAG_A), ("127.0.0.1", port)) > 0
            and rx.peer_addr() is not None,
        )
        assert rx.peer_addr() == ("127.0.0.1", tx.getsockname()[1])
        assert rx.ts_bytes_demuxed() > 0
    finally:
        rx.close()
        it.join(5)
        tx.close()
    assert not it.is_alive()
    assert not errors, errors


def test_shell_source_rejected() -> None:
    pair = _TwoSenders()
    port = free_udp_port()
    try:
        with shells.Receiver.open(f"udp://{pair.bind_host}:{port}?source=127.0.0.1") as rx:
            tags: list[int] = []
            while len(tags) < ENOUGH:
                # B's datagram queued before A's, so the pin, not arrival
                # order, keeps it out.
                pair.send_b(_ts_chunk(TAG_B), port)
                pair.send_a(_ts_chunk(TAG_A), port)
                for _ in range(7):
                    tags.append(rx.recv_bytes()[1])
            assert TAG_B not in tags
            assert rx.source_rejected() > 0
            assert rx.peer_addr() == pair.a_addr
    finally:
        pair.close()


def test_peer_addr_while_recv_parked() -> None:
    # udp.RecvTransport: a recv() parked with no deadline holds the slot.
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.bind(("127.0.0.1", 0))
    got: list[object] = []

    def recv_once() -> None:
        try:
            got.append(rx.recv())
        except UdpError as e:
            # The close() below ends the second parked recv().
            got.append(e.kind)

    try:
        parked = threading.Thread(target=recv_once, daemon=True)
        parked.start()
        time.sleep(0.2)
        assert parked.is_alive()
        assert _finishes(rx.peer_addr) is None
        tx.sendto(b"one", ("127.0.0.1", rx.local_addr_port()))
        parked.join(5)
        assert got and got[0] == (b"one", ("127.0.0.1", tx.getsockname()[1]))

        parked = threading.Thread(target=recv_once, daemon=True)
        parked.start()
        time.sleep(0.2)
        assert parked.is_alive()
        assert _finishes(rx.peer_addr) == ("127.0.0.1", tx.getsockname()[1])
    finally:
        rx.close()
        tx.close()
    parked.join(5)
    assert got[-1] == UdpErrorKind.CLOSED

    # The shells: an iteration parked on another thread.
    port = free_udp_port()
    shell = shells.DemuxReceiver.open(f"udp://127.0.0.1:{port}")
    errors: list[BaseException] = []
    it = threading.Thread(target=_iterate, args=(shell, errors), daemon=True)
    it.start()
    try:
        time.sleep(0.2)
        assert it.is_alive()
        assert _finishes(shell.peer_addr) is None
        assert _finishes(shell.source_rejected) == 0
    finally:
        shell.close()
        it.join(5)
    assert not errors, errors


def test_closed_raises() -> None:
    # The transports' snapshot getter raises CLOSED once closed, like
    # `local_addr_port()`.
    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    rx.close()
    with pytest.raises(UdpError) as e:
        rx.peer_addr()
    assert e.value.kind == UdpErrorKind.CLOSED

    rrx, _ = _open_rist_rx()
    rrx.close()
    with pytest.raises(RistError) as re:
        rrx.peer_addr()
    assert re.value.kind == RistErrorKind.CLOSED


def test_shell_getters_answer_after_close() -> None:
    # The shells' getters keep the last value after close(), like
    # `ts_bytes_demuxed()` and `end_reason()`.
    port = free_udp_port()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.bind(("127.0.0.1", 0))
    try:
        rx = shells.Receiver.open(f"udp://127.0.0.1:{port}")
        tx.sendto(_ts_chunk(TAG_A), ("127.0.0.1", port))
        rx.recv_bytes()
        rx.close()
        assert rx.peer_addr() == ("127.0.0.1", tx.getsockname()[1])
        assert rx.source_rejected() == 0
    finally:
        tx.close()


def test_shell_over_tcp_reports_none() -> None:
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    conns: list[socket.socket] = []

    def serve() -> None:
        conn, _ = srv.accept()
        conns.append(conn)
        conn.sendall(_ts_chunk(TAG_A))

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    try:
        with shells.Receiver.open(f"tcp://127.0.0.1:{srv.getsockname()[1]}") as rx:
            assert rx.recv_bytes()[1] == TAG_A
            assert rx.peer_addr() is None
            assert rx.source_rejected() == 0
    finally:
        server.join(5)
        for c in conns:
            c.close()
        srv.close()
