"""Arguments and classes that do nothing warn `DeprecationWarning` before
their removal in 0.12.0; the call otherwise behaves as before, and the same
call without the argument does not warn."""

from __future__ import annotations

import re
import socket
import warnings

import pytest

from tstrans import shells, tcp
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.rtp import RtspServer, RtspServerConfig

from test_srt_managed_basic import _make_managed_pair
from _builders.ports import free_udp_port

NULL = bytes([0x47, 0x1F, 0xFF, 0x10]) + b"\xff" * 184


def _server() -> RtspServer:
    return RtspServer.start(RtspServerConfig(bind_addr="127.0.0.1:0", graceful_shutdown_drain_ms=50))


def _assert_stopped(server: RtspServer) -> None:
    """An ended server refuses a mount edit with `RtspError(CLOSED)`."""
    from _builders.mux_programs import video_only_program
    from tstrans.exceptions import RtspError, RtspErrorKind

    with pytest.raises(RtspError) as ei:
        server.add_unicast_mount("/after", video_only_program())
    assert ei.value.kind == RtspErrorKind.CLOSED


def test_stop_with_drain_ms_warns() -> None:
    server = _server()
    with pytest.warns(
        DeprecationWarning,
        match=r"RtspServer\.stop\(drain_ms=\.\.\.\) is deprecated: the argument is ignored; removed in 0\.12\.0",
    ) as rec:
        server.stop(drain_ms=10)
    assert len(rec) == 1
    assert rec[0].filename == __file__
    _assert_stopped(server)


def test_stop_without_drain_ms_does_not_warn() -> None:
    server = _server()
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        server.stop()
    _assert_stopped(server)


def _udp_receiver() -> tuple[shells.Receiver, int]:
    rx = shells.Receiver.open("udp://127.0.0.1:0")
    return rx, rx.local_port()


def _send_null_packets(port: int, n: int = 8) -> None:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as tx:
        tx.sendto(NULL * n, ("127.0.0.1", port))


def test_receiver_recv_bytes_with_max_len_warns() -> None:
    rx, port = _udp_receiver()
    try:
        _send_null_packets(port)
        with pytest.warns(
            DeprecationWarning,
            match=r"^Receiver\.recv_bytes\(max_len=\.\.\.\) is deprecated: the argument is ignored; removed in 0\.12\.0",
        ) as rec:
            assert rx.recv_bytes(max_len=1500) == NULL
        assert len(rec) == 1
    finally:
        rx.close()


def test_receiver_recv_bytes_without_max_len_does_not_warn() -> None:
    rx, port = _udp_receiver()
    try:
        _send_null_packets(port)
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            assert rx.recv_bytes() == NULL
    finally:
        rx.close()


def test_recv_bytes_with_and_without_max_len_return_the_same() -> None:
    """The argument changes nothing: one packet per call either way, even
    a `max_len` below one packet."""
    rx, port = _udp_receiver()
    try:
        _send_null_packets(port, 8)
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", DeprecationWarning)
            got = [rx.recv_bytes(), rx.recv_bytes(max_len=1), rx.recv_bytes(), rx.recv_bytes(max_len=1500)]
        assert got == [NULL] * 4
    finally:
        rx.close()


def test_managed_receiver_recv_bytes_max_len_warns_and_without_does_not() -> None:
    sender, receiver = _make_managed_pair(free_udp_port())
    try:
        for _ in range(14):
            sender.send_bytes(NULL)
        with pytest.warns(
            DeprecationWarning,
            match=r"ManagedReceiver\.recv_bytes\(max_len=\.\.\.\) is deprecated: the argument is ignored; removed in 0\.12\.0",
        ) as rec:
            first = receiver.recv_bytes(max_len=1500)
        assert len(rec) == 1
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            second = receiver.recv_bytes()
        assert first == second == NULL
    finally:
        sender.close()
        receiver.close()


@pytest.mark.parametrize("side", ["sender", "receiver"])
def test_managed_srt_stats_warns_then_raises(side: str) -> None:
    sender, receiver = _make_managed_pair(free_udp_port())
    shell = sender if side == "sender" else receiver
    name = "ManagedSender" if side == "sender" else "ManagedReceiver"
    try:
        with pytest.warns(
            DeprecationWarning,
            match=rf"{name}\.srt_stats\(\) is deprecated: it always raises; use socket_stats\(\); removed in 0\.12\.0",
        ) as rec:
            with pytest.raises(SrtError) as ei:
                shell.srt_stats()
        assert len(rec) == 1
        assert ei.value.kind == SrtErrorKind.IO
    finally:
        sender.close()
        receiver.close()


def test_tls_config_warns() -> None:
    with pytest.warns(
        DeprecationWarning,
        match=r"tcp\.TlsConfig is deprecated: no method accepts it; removed in 0\.12\.0",
    ) as rec:
        cfg = tcp.TlsConfig(ca_pem=b"CA")
    assert len(rec) == 1
    assert cfg.ca_pem == b"CA"
    with pytest.warns(
        DeprecationWarning,
        match=r"tcp\.ClientCert is deprecated: no method accepts it; removed in 0\.12\.0",
    ) as rec:
        cert = tcp.ClientCert(cert_pem=b"C", key_pem=b"K")
    assert len(rec) == 1
    assert cert.cert_pem == b"C"



def _last_seen_micros_of_a_demux_receiver(call):  # type: ignore[no-untyped-def]
    with shells.DemuxReceiver.open("udp://127.0.0.1:0") as rx:
        assert rx.last_seen(0x101) is None
        return call(lambda: rx.last_seen_micros(0x101))


def _last_seen_micros_of_a_managed_demux_receiver(call):  # type: ignore[no-untyped-def]
    from test_srt_managed_convenience import _make_managed_pair

    sender, receiver = _make_managed_pair(free_udp_port())
    try:
        assert receiver.last_seen(0x101) is None
        return call(lambda: receiver.last_seen_micros(0x101))
    finally:
        sender.close()
        receiver.close()


def _since_unix_ms_of_a_publisher_info(call):  # type: ignore[no-untyped-def]
    from test_rtsp_publish import RawPublisher, _port, _start

    server = _start()
    try:
        mount = server.add_publish_mount("/cam")
        pub = RawPublisher(_port(server), "/cam")
        try:
            info = mount.publisher()
            assert info.since is not None
            return call(lambda: info.since_unix_ms)
        finally:
            pub.close()
    finally:
        server.stop()


@pytest.mark.parametrize(
    "use, message",
    [
        (
            _last_seen_micros_of_a_demux_receiver,
            r"^DemuxReceiver\.last_seen_micros\(\) is deprecated: use last_seen\(\); removed in 0\.12\.0$",
        ),
        (
            _last_seen_micros_of_a_managed_demux_receiver,
            r"^ManagedDemuxReceiver\.last_seen_micros\(\) is deprecated: use last_seen\(\); removed in 0\.12\.0$",
        ),
        (
            _since_unix_ms_of_a_publisher_info,
            r"^PublisherInfo\.since_unix_ms is deprecated: use since; removed in 0\.12\.0$",
        ),
    ],
    ids=["DemuxReceiver.last_seen_micros", "ManagedDemuxReceiver.last_seen_micros", "PublisherInfo.since_unix_ms"],
)
def test_the_old_names_warn(use, message) -> None:  # type: ignore[no-untyped-def]
    """Each old name warns once, at the caller's line, and still answers;
    the new name beside it (read inside `use`) does not warn."""

    def call(read):  # type: ignore[no-untyped-def]
        with warnings.catch_warnings(record=True) as rec:
            warnings.simplefilter("always")
            value = read()
        assert len(rec) == 1, [str(w.message) for w in rec]
        assert rec[0].category is DeprecationWarning
        assert re.match(message, str(rec[0].message)), str(rec[0].message)
        assert rec[0].filename == __file__
        return value

    with warnings.catch_warnings():
        warnings.simplefilter("error")
        value = use(call)
    assert value is None or isinstance(value, int)
