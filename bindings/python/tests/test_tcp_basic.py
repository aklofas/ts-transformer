"""TCP transport loopback + error-case tests.

Covers:
- the 4 caller/listener x send/recv combo loopback round-trips
- TLS dataclass round-trip (forward-compat) + tcps:// loopback e2e
- error-kind propagation, TcpErrorKind count sentinel, test-helper wiring
"""

import socket
import sys
import threading
import time

import pathlib

import pytest

from tstrans import tcp
from tstrans.exceptions import MuxError, MuxErrorKind, TcpError, TcpErrorKind
from tstrans.mpegts import DemuxerConfig, Pts90khz

from _builders.hang_deadline import call_with_hang_deadline
from _builders.mux_programs import masquerade_program, video_only_program

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_PAYLOAD = b"\x47\x40\x00\x10" + b"\x00" * 184  # one valid 188-byte TS packet
NAL_AUD = b"\x00\x00\x00\x01\x09\xF0"


def _listener_and_port() -> tuple[tcp.Listener, int]:
    """Bind a listener on loopback port 0 and return (listener, port)."""
    listener = tcp.Listener.builder().bind("127.0.0.1:0").build()
    port = listener.local_port()
    assert port > 0
    return listener, port


# ---------------------------------------------------------------------------
# 4 caller/listener x send/recv combos
# ---------------------------------------------------------------------------


def test_tcp_caller_sends_listener_receives() -> None:
    """Combo 1: caller connects + sends; listener accepts + receives."""
    listener, port = _listener_and_port()
    received: list[bytes] = []
    barrier = threading.Barrier(2)

    def server_thread() -> None:
        barrier.wait()  # sync: listener is ready
        peer = listener.accept_blocking()
        buf = bytearray(4096)
        n = peer.recv(buf)
        received.append(bytes(buf[:n]))
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    barrier.wait()  # wait for server_thread to reach barrier

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    caller.send(_PAYLOAD)
    caller.close()
    t.join(timeout=5.0)
    listener.close()

    assert len(received) == 1
    assert received[0] == _PAYLOAD


def test_tcp_listener_sends_caller_receives() -> None:
    """Combo 2: listener accepts + sends; caller connects + receives."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        peer.send(_PAYLOAD)
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    buf = bytearray(4096)
    n = caller.recv(buf)
    caller.close()
    t.join(timeout=5.0)
    listener.close()

    assert bytes(buf[:n]) == _PAYLOAD


def test_tcp_caller_bidirectional_echo() -> None:
    """Bidirectional echo: caller sends, server echoes, caller receives."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        buf = bytearray(4096)
        n = peer.recv(buf)
        peer.send(bytes(buf[:n]))  # echo
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    caller.send(_PAYLOAD)
    buf = bytearray(4096)
    n = caller.recv(buf)
    caller.close()
    t.join(timeout=5.0)
    listener.close()

    assert bytes(buf[:n]) == _PAYLOAD


def test_tcp_multiple_send_recv_round_trips() -> None:
    """Caller sends N packets; listener reads the full byte stream.

    TCP is a byte stream with no message framing — multiple `send` calls
    may coalesce into fewer (or split across more) `recv` reads. The
    listener accumulates until it has the full expected length, then
    verifies the concatenated stream rather than asserting a 1:1
    send->recv correspondence.
    """
    n_packets = 5
    total = n_packets * 188
    listener, port = _listener_and_port()
    received = bytearray()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        # Read until the full payload arrives (TCP delivers all buffered
        # bytes before the peer's FIN, so we never recv past `total`).
        while len(received) < total:
            buf = bytearray(4096)
            n = peer.recv(buf)
            if n == 0:
                break
            received.extend(buf[:n])
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    expected = b"".join(bytes([i]) * 188 for i in range(n_packets))
    for i in range(n_packets):
        caller.send(bytes([i]) * 188)
    caller.close()
    t.join(timeout=10.0)
    listener.close()

    assert bytes(received) == expected


def test_tcp_send_bytearray() -> None:
    """Transport.send() accepts bytearray (fallback bytes-like path)."""
    listener, port = _listener_and_port()
    received: list[bytes] = []
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        buf = bytearray(4096)
        n = peer.recv(buf)
        received.append(bytes(buf[:n]))
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    payload = bytearray(b"\x47\x41\x00\x10" + b"\xff" * 184)
    caller.send(payload)
    caller.close()
    t.join(timeout=5.0)
    listener.close()

    assert len(received) == 1
    assert received[0] == bytes(payload)


# ---------------------------------------------------------------------------
# Builder / listener API tests
# ---------------------------------------------------------------------------


def test_tcp_listener_local_port_nonzero() -> None:
    """Listener.local_port() returns a non-zero port after bind."""
    listener, port = _listener_and_port()
    assert isinstance(port, int)
    assert port > 0
    listener.close()


def test_tcp_transport_builder_missing_url_raises() -> None:
    """TransportBuilder.build() raises ValueError when url() was not called."""
    with pytest.raises(ValueError):
        tcp.Transport.builder().build()


def test_tcp_listener_builder_missing_bind_raises() -> None:
    """ListenerBuilder.build() raises ValueError when bind() was not called."""
    with pytest.raises(ValueError):
        tcp.Listener.builder().build()


def test_tcp_transport_context_manager() -> None:
    """Transport supports the context-manager (with) protocol."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    with tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build() as tx:
        assert "Transport" in repr(tx)

    t.join(timeout=5.0)
    listener.close()


def test_tcp_listener_context_manager() -> None:
    """Listener supports the context-manager (with) protocol."""
    with tcp.Listener.builder().bind("127.0.0.1:0").build() as listener:
        assert listener.local_port() > 0


def test_tcp_transport_stats_initial_zero() -> None:
    """Transport.stats() counters are zero before any I/O."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    s = caller.stats()
    assert s.bytes_sent == 0
    assert s.send_calls == 0
    assert s.bytes_received == 0
    assert s.recv_calls == 0
    caller.close()
    t.join(timeout=5.0)
    listener.close()


def test_tcp_transport_stats_update_after_send() -> None:
    """Transport.stats().bytes_sent / send_calls tick after a successful send."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        buf = bytearray(4096)
        peer.recv(buf)
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
    caller.send(_PAYLOAD)
    s = caller.stats()
    assert s.bytes_sent == len(_PAYLOAD)
    assert s.send_calls == 1
    caller.close()
    t.join(timeout=5.0)
    listener.close()


def _accepted_pair() -> tuple[socket.socket, tcp.Transport, tcp.Listener]:
    """A plain client socket connected to a listener on a kernel-picked
    port, the listener's accepted `Transport`, and the listener. The
    connect completes against the backlog, so no worker thread is needed."""
    listener, port = _listener_and_port()
    client = socket.create_connection(("127.0.0.1", port), timeout=5.0)
    try:
        accepted = listener.accept_blocking()
    except BaseException:
        client.close()
        listener.close()
        raise
    return client, accepted, listener


def test_peer_addr_is_a_host_port_tuple() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{listener.local_port()}").build()
        try:
            assert caller.peer_addr() == ("127.0.0.1", listener.local_port())
        finally:
            caller.close()
        assert accepted.peer_addr() == client.getsockname()
    finally:
        accepted.close()
        client.close()
        listener.close()


def test_peer_addr_after_close_raises_closed() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        accepted.close()
        with pytest.raises(TcpError) as ei:
            accepted.peer_addr()
        assert ei.value.kind == TcpErrorKind.CLOSED
    finally:
        client.close()
        listener.close()


def test_peer_addr_of_an_ipv4_client_on_a_dual_stack_listener_is_ipv4() -> None:
    # The kernel hands the listener ::ffff:127.0.0.1; the tuple reports it in
    # IPv4 form, as the UDP and RIST source records do.
    try:
        listener = tcp.Listener.open("tcp://[::]:0?listen=1")
    except TcpError as e:
        if sys.platform.startswith("linux"):
            pytest.fail(f"no IPv6 wildcard bind on Linux: {e}")
        pytest.skip(f"no IPv6 wildcard bind: {e}")
    try:
        try:
            client = socket.create_connection(("127.0.0.1", listener.local_port()), timeout=5.0)
        except OSError as e:
            if sys.platform.startswith("linux"):
                raise
            pytest.skip(f"IPv4 does not reach a [::] listener here: {e}")
        try:
            accepted = listener.accept_blocking()
            assert accepted.peer_addr() == client.getsockname()
            assert client.getsockname()[0] == "127.0.0.1"
            accepted.close()
        finally:
            client.close()
    finally:
        listener.close()


def test_listener_open_binds_and_reports_its_port() -> None:
    listener = tcp.Listener.open("tcp://127.0.0.1:0?listen=1")
    try:
        port = listener.local_port()
        assert port > 0
        client = socket.create_connection(("127.0.0.1", port), timeout=5.0)
        try:
            accepted = listener.accept_blocking()
            client.sendall(_PAYLOAD)
            buf = bytearray(4096)
            n = accepted.recv(buf)
            assert bytes(buf[:n]) == _PAYLOAD
            accepted.close()
        finally:
            client.close()
    finally:
        listener.close()


@pytest.mark.parametrize(
    "url",
    [
        "tcp://127.0.0.1:0",
        "tcp://localhost:0?listen=1",
        "tcp://127.0.0.1:0?listen=maybe",
    ],
)
def test_listener_open_without_listen_is_a_url_error(url: str) -> None:
    with pytest.raises(TcpError) as ei:
        tcp.Listener.open(url)
    assert ei.value.kind == TcpErrorKind.URL


def test_listener_open_serves_tls() -> None:
    cert, key = _tls_fixture_paths()
    listener = tcp.Listener.open(f"tcps://127.0.0.1:0?listen=1&cert={cert}&key={key}")
    received: list[bytes] = []
    try:
        port = listener.local_port()

        def serve() -> None:
            peer = listener.accept_blocking()
            buf = bytearray(4096)
            n = peer.recv(buf)
            received.append(bytes(buf[:n]))
            peer.close()

        t = threading.Thread(target=serve, daemon=True)
        t.start()
        caller = tcp.Transport.builder().url(f"tcps://127.0.0.1:{port}?ca={cert}").build()
        caller.send(_PAYLOAD)
        caller.close()
        t.join(timeout=10.0)
    finally:
        listener.close()
    assert received == [_PAYLOAD]


def _ts_packets(count: int) -> bytes:
    """`count` aligned null-PID TS packets, each tagged with its index."""
    return b"".join(bytes([0x47, 0x1F, 0xFF, 0x10, i]) + b"\xff" * 183 for i in range(count))


def _recv_exactly(rx: object, n: int, what: str) -> bytes:
    """`n` bytes from a receiving shell; a watchdog cancels the shell if they
    have not all arrived within 10 s, so a lost byte fails with `what`."""
    watchdog = threading.Timer(10.0, rx.cancel_handle().cancel)  # type: ignore[attr-defined]
    watchdog.start()
    got = b""
    try:
        while len(got) < n:
            got += rx.recv_bytes()  # type: ignore[attr-defined]
    except TcpError as e:
        pytest.fail(f"{what}: {len(got)} of {n} bytes, then {e.kind.name}")
    finally:
        watchdog.cancel()
    return got


def test_into_receiver_delivers_the_peers_ts() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        rx = accepted.into_receiver()
        try:
            data = _ts_packets(8)
            client.sendall(data)
            assert _recv_exactly(rx, len(data), "the peer's packets") == data
            with pytest.raises(TcpError) as ei:
                accepted.peer_addr()
            assert ei.value.kind == TcpErrorKind.CLOSED
        finally:
            rx.close()
    finally:
        client.close()
        listener.close()


def test_into_demux_receiver_and_into_sender_consume_the_transport() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        rx = accepted.into_demux_receiver(DemuxerConfig())
        rx.close()
        for call in (
            accepted.into_receiver,
            accepted.into_sender,
            accepted.into_demux_receiver,
            lambda: accepted.into_mux_sender(video_only_program()),
            lambda: accepted.send(_PAYLOAD),
        ):
            with pytest.raises(TcpError) as ei:
                call()
            assert ei.value.kind == TcpErrorKind.CLOSED
    finally:
        client.close()
        listener.close()

    client, accepted, listener = _accepted_pair()
    try:
        tx = accepted.into_sender()
        try:
            tx.send_bytes(_ts_packets(4))
            tx.flush()
            client.settimeout(5.0)
            got = b""
            while len(got) < 4 * 188:
                chunk = client.recv(4096)
                assert chunk, "the peer closed before the sender's bytes arrived"
                got += chunk
            assert got == _ts_packets(4)
        finally:
            tx.close()
    finally:
        client.close()
        listener.close()


def test_into_mux_sender_with_a_refused_config_leaves_the_transport_usable() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        with pytest.raises(MuxError) as ei:
            accepted.into_mux_sender(masquerade_program())
        assert ei.value.kind is MuxErrorKind.CONFIG_INVALID
        assert accepted.peer_addr() == client.getsockname()
        with pytest.raises(OverflowError):
            accepted.into_demux_receiver(DemuxerConfig(pes_cap_per_pid=1 << 100))
        assert accepted.peer_addr() == client.getsockname()
        tx = accepted.into_mux_sender(video_only_program())
        try:
            tx.send_video(NAL_AUD, pts=Pts90khz.from_raw(0))
            client.settimeout(5.0)
            first = client.recv(188)
            assert first[:1] == b"\x47"
        finally:
            tx.close()
    finally:
        client.close()
        listener.close()


def test_recv_after_a_clean_peer_close_is_broken() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        client.close()
        with pytest.raises(TcpError) as ei:
            accepted.recv(bytearray(4096))
        assert ei.value.kind == TcpErrorKind.BROKEN
    finally:
        accepted.close()
        listener.close()


def test_tls_with_a_missing_pem_is_io(tmp_path: pathlib.Path) -> None:
    missing = str(tmp_path / "missing.pem")
    with pytest.raises(TcpError) as ei:
        tcp.Listener.builder().bind("127.0.0.1:0").tls(missing, missing).build()
    assert ei.value.kind == TcpErrorKind.IO


# ---------------------------------------------------------------------------
# TLS dataclass round-trip (forward-compat)
# ---------------------------------------------------------------------------


def test_tcp_tls_config_dataclass_round_trip() -> None:
    """TlsConfig is constructible and fields are accessible."""
    with pytest.warns(DeprecationWarning, match="TlsConfig"):
        cfg = tcp.TlsConfig(ca_pem=b"-----BEGIN CERTIFICATE-----\n...", verify_hostname=True)
    assert cfg.verify_hostname is True
    assert cfg.ca_pem.startswith(b"-----BEGIN")


def test_tcp_tls_config_default_empty_pem() -> None:
    """TlsConfig() with no args uses an empty ca_pem."""
    with pytest.warns(DeprecationWarning, match="TlsConfig"):
        cfg = tcp.TlsConfig()
    assert cfg.ca_pem == b""
    assert cfg.verify_hostname is True
    assert cfg.client_cert is None


def test_tcp_client_cert_dataclass() -> None:
    """ClientCert stores cert_pem and key_pem."""
    with pytest.warns(DeprecationWarning, match="ClientCert"):
        cert = tcp.ClientCert(cert_pem=b"CERT_PEM", key_pem=b"KEY_PEM")
    assert cert.cert_pem == b"CERT_PEM"
    assert cert.key_pem == b"KEY_PEM"


def test_tcp_tls_config_with_client_cert() -> None:
    """TlsConfig accepts a ClientCert in client_cert."""
    with pytest.warns(DeprecationWarning, match="ClientCert"):
        client_cert = tcp.ClientCert(cert_pem=b"CERT", key_pem=b"KEY")
    with pytest.warns(DeprecationWarning, match="TlsConfig"):
        cfg = tcp.TlsConfig(ca_pem=b"CA", client_cert=client_cert)
    assert cfg.client_cert is not None
    assert cfg.client_cert.cert_pem == b"CERT"


def _tls_fixture_paths() -> tuple[str, str]:
    d = pathlib.Path(__file__).parent / "fixtures" / "tls"
    return str(d / "cert.pem"), str(d / "key.pem")


def test_tcp_tls_loopback_round_trip() -> None:
    """tcps:// end-to-end: a TLS listener (fixture cert + key via
    ListenerBuilder.tls) and a caller verifying against the same cert via
    the ?ca= URL param round-trip bytes over the encrypted channel.
    Regression: tcps:// used to raise TcpError(TLS_DISABLED) because the
    wheels were built without the tls feature."""
    cert, key = _tls_fixture_paths()
    listener = tcp.Listener.builder().bind("127.0.0.1:0").tls(cert, key).build()
    port = listener.local_port()
    assert port > 0

    received: list[bytes] = []
    barrier = threading.Barrier(2)

    def server_thread() -> None:
        barrier.wait()  # sync: listener is ready
        peer = listener.accept_blocking()
        buf = bytearray(4096)
        n = peer.recv(buf)
        received.append(bytes(buf[:n]))
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    barrier.wait()

    # The fixture cert has an IP:127.0.0.1 SAN; it's a self-signed non-CA
    # cert (CA:FALSE), which rustls accepts as a trust anchor while
    # rejecting CA certs presented as end-entity.
    caller = tcp.Transport.builder().url(f"tcps://127.0.0.1:{port}?ca={cert}").build()
    caller.send(_PAYLOAD)
    caller.close()
    t.join(timeout=10.0)
    listener.close()

    assert received == [_PAYLOAD]


def test_tcp_tls_path_with_url_char_raises_invalid_config() -> None:
    """A cert/key path containing a URL-structural character would corrupt
    the internal listener URL — build() fails fast with INVALID_CONFIG."""
    with pytest.raises(TcpError) as excinfo:
        tcp.Listener.builder().bind("127.0.0.1:0").tls("/tmp/a&b.pem", "/tmp/k.pem").build()
    assert excinfo.value.kind == TcpErrorKind.INVALID_CONFIG


def test_tcp_tls_caller_rejects_untrusted_cert() -> None:
    """Without ?ca=, the self-signed listener cert fails native-root
    verification: the caller raises TcpError (TLS handshake / IO — never a
    silent success), proving certificate verification is actually on."""
    cert, key = _tls_fixture_paths()
    listener = tcp.Listener.builder().bind("127.0.0.1:0").tls(cert, key).build()
    port = listener.local_port()

    def server_thread() -> None:
        # The accept fails or the connection drops mid-handshake; both fine.
        try:
            peer = listener.accept_blocking()
            peer.close()
        except TcpError:
            pass

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()

    with pytest.raises(TcpError) as excinfo:
        caller = tcp.Transport.builder().url(f"tcps://127.0.0.1:{port}").build()
        # Some TLS stacks only surface the handshake failure at first I/O.
        caller.send(_PAYLOAD)
    assert excinfo.value.kind != TcpErrorKind.TLS_DISABLED
    listener.close()
    t.join(timeout=10.0)


# ---------------------------------------------------------------------------
# Error-kind propagation + count sentinel + test-helper wiring
# ---------------------------------------------------------------------------


def test_tcp_error_kind_count() -> None:
    """Sentinel: catches drift if Rust adds a new TcpErrorKind variant."""
    assert len(TcpErrorKind) == 13


def test_tcp_error_url_bad_scheme() -> None:
    """TransportBuilder.build() raises TcpError(kind=URL) for wrong scheme."""
    with pytest.raises(TcpError) as excinfo:
        tcp.Transport.builder().url("udp://1.2.3.4:5000").build()
    assert excinfo.value.kind == TcpErrorKind.URL


def test_tcp_error_connect_timeout() -> None:
    """Short connect_timeout_ms to an unroutable address raises CONNECT_TIMEOUT or IO."""
    with pytest.raises(TcpError) as excinfo:
        # RFC 5737 test address (192.0.2.0/24) -- not routable, short timeout.
        tcp.Transport.builder() \
            .url("tcp://192.0.2.1:1234") \
            .connect_timeout_ms(100) \
            .build()
    # Either CONNECT_TIMEOUT (clean timeout) or IO (routing rejection) is valid.
    assert excinfo.value.kind in (TcpErrorKind.CONNECT_TIMEOUT, TcpErrorKind.IO)


def test_tcp_error_payload_too_large() -> None:
    """Transport.send() raises TcpError(kind=TOO_LARGE) for oversized payload."""
    listener, port = _listener_and_port()
    ready = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    t.start()
    ready.wait(timeout=2.0)

    # pkt_size=188 means any payload > 188 bytes is rejected.
    caller = tcp.Transport.builder() \
        .url(f"tcp://127.0.0.1:{port}") \
        .pkt_size(188) \
        .build()
    huge = b"\x47" * 1316  # 7x188, exceeds cap of 188
    with pytest.raises(TcpError) as excinfo:
        caller.send(huge)
    assert excinfo.value.kind == TcpErrorKind.TOO_LARGE

    caller.close()
    t.join(timeout=5.0)
    listener.close()


def test_tcp_pkt_size_zero_is_refused() -> None:
    """``pkt_size(0)`` is refused at ``build()`` (caller and listener) and
    in a URL, before any connect or bind."""
    with pytest.raises(TcpError) as excinfo:
        tcp.Transport.builder().url("tcp://127.0.0.1:1").pkt_size(0).build()
    assert excinfo.value.kind == TcpErrorKind.INVALID_CONFIG
    with pytest.raises(TcpError) as excinfo:
        tcp.Listener.builder().bind("127.0.0.1:0").pkt_size(0).build()
    assert excinfo.value.kind == TcpErrorKind.INVALID_CONFIG
    with pytest.raises(TcpError) as excinfo:
        tcp.Transport.builder().url("tcp://127.0.0.1:1?pkt_size=0").build()
    assert excinfo.value.kind == TcpErrorKind.URL


def test_tcp_error_wiring_via_test_helper() -> None:
    """Verify make_tcp_error wiring for all 8 kind variants via test helper."""
    from tstrans._native import _raise_tcp_error_for_test

    for kind in TcpErrorKind:
        with pytest.raises(TcpError) as excinfo:
            _raise_tcp_error_for_test(kind.name, f"test {kind.name}")
        assert excinfo.value.kind == kind


# ---------------------------------------------------------------------------
# cancel_handle()
# ---------------------------------------------------------------------------


def test_tcp_cancel_handle_unparks_recv_and_later_calls_are_closed() -> None:
    """tcp exposes `cancel_handle()` like every other shell. The handle
    ends a parked recv() with TcpError(CLOSED) without freeing the object,
    every later call raises CLOSED, and close() afterwards is quiet."""
    listener, port = _listener_and_port()
    ready = threading.Event()
    hold = threading.Event()

    def server_thread() -> None:
        ready.set()
        peer = listener.accept_blocking()
        hold.wait(timeout=10.0)  # keep the peer open, send nothing
        peer.close()

    t = threading.Thread(target=server_thread, daemon=True)
    caller = None
    w = None
    captured: list[BaseException] = []
    try:
        t.start()
        assert ready.wait(timeout=2.0), "server thread did not start"
        caller = tcp.Transport.builder().url(f"tcp://127.0.0.1:{port}").build()
        ch = caller.cancel_handle()
        assert not ch.is_cancelled()
        assert repr(ch) == "CancelHandle(cancelled=false)"
        parked = threading.Event()

        def worker() -> None:
            buf = bytearray(4096)
            parked.set()
            try:
                caller.recv(buf)
            except BaseException as exc:  # noqa: BLE001 — capture whatever is raised
                captured.append(exc)

        w = threading.Thread(target=worker, daemon=True)
        w.start()
        assert parked.wait(timeout=2.0), "worker thread did not start"
        time.sleep(0.2)  # let recv() enter its poll loop
        ch.cancel()
        w.join(timeout=5.0)
        assert not w.is_alive(), "recv() did not unpark after cancel()"
        assert len(captured) == 1 and isinstance(captured[0], TcpError)
        assert captured[0].kind == TcpErrorKind.CLOSED
        assert ch.is_cancelled()
        with pytest.raises(TcpError) as later:
            caller.send(b"x")
        assert later.value.kind == TcpErrorKind.CLOSED
        caller.close()  # quiet after a cancel
    finally:
        # Runs even when an assertion fails above, so no thread is left
        # parked in native recv()/accept() at interpreter exit.
        hold.set()
        if caller is not None:
            caller.close()  # idempotent; fires the cancel first
        listener.close()  # unparks a still-pending accept_blocking()
        if w is not None:
            w.join(timeout=5.0)
        t.join(timeout=5.0)
    assert not t.is_alive(), "server thread did not exit"


def test_tcp_listener_cancel_handle_unparks_accept() -> None:
    listener, _port = _listener_and_port()
    captured: list[BaseException] = []

    def worker() -> None:
        try:
            listener.accept_blocking()
        except BaseException as exc:  # noqa: BLE001
            captured.append(exc)

    w = threading.Thread(target=worker, daemon=True)
    try:
        ch = listener.cancel_handle()
        w.start()
        time.sleep(0.2)
        ch.cancel()
        w.join(timeout=5.0)
        assert not w.is_alive(), "accept_blocking() did not unpark after cancel()"
        assert len(captured) == 1
        assert isinstance(captured[0], TcpError) and captured[0].kind == TcpErrorKind.CLOSED
        assert ch.is_cancelled()
    finally:
        listener.close()  # fires the cancel first; a parked accept ends
        if w.is_alive():
            w.join(timeout=5.0)


def _send_and_read(tx: object, client: socket.socket) -> None:
    data = _ts_packets(4)
    tx.send_bytes(data)  # type: ignore[attr-defined]
    tx.flush()  # type: ignore[attr-defined]
    client.settimeout(5.0)
    got = b""
    while len(got) < len(data):
        chunk = client.recv(4096)
        assert chunk, "the connection ended before the sender's bytes arrived"
        got += chunk
    assert got == data


def test_a_temporary_transport_converts_into_a_working_shell() -> None:
    listener, port = _listener_and_port()
    client = socket.create_connection(("127.0.0.1", port), timeout=5.0)
    try:
        # The accepted Transport is collected at once; its drop must not
        # reach the connection the receiver now owns.
        rx = listener.accept_blocking().into_receiver()
        try:
            data = _ts_packets(8)
            client.sendall(data)
            assert _recv_exactly(rx, len(data), "the peer's packets") == data
        finally:
            rx.close()
    finally:
        client.close()
        listener.close()


def test_closing_a_converted_transport_leaves_its_shell_working() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        early = accepted.cancel_handle()
        tx = accepted.into_sender()
        try:
            accepted.close()
            early.cancel()
            _send_and_read(tx, client)
        finally:
            tx.close()
    finally:
        client.close()
        listener.close()

    client, accepted, listener = _accepted_pair()
    try:
        with accepted:
            rx = accepted.into_receiver()
        try:
            data = _ts_packets(8)
            client.sendall(data)
            assert _recv_exactly(rx, len(data), "the peer's packets") == data
        finally:
            rx.close()
    finally:
        client.close()
        listener.close()


def test_a_cancelled_transport_is_not_converted() -> None:
    client, accepted, listener = _accepted_pair()
    try:
        accepted.cancel_handle().cancel()
        with pytest.raises(TcpError) as ei:
            accepted.into_sender()
        assert ei.value.kind == TcpErrorKind.CLOSED
    finally:
        accepted.close()
        client.close()
        listener.close()


def test_a_close_racing_a_conversion_never_hands_out_a_cancelled_shell() -> None:
    # Either outcome is right; no round may give a shell that cannot send.
    # Odd rounds start the close first (it tends to win), even rounds release
    # both from a barrier (the conversion tends to win).
    for i in range(60):
        client, accepted, listener = _accepted_pair()
        try:
            together = threading.Barrier(2) if i % 2 == 0 else None

            def close_it() -> None:
                if together is not None:
                    together.wait(5.0)
                accepted.close()

            closer = threading.Thread(target=close_it, daemon=True)
            closer.start()
            if together is not None:
                together.wait(5.0)
            try:
                tx = accepted.into_sender()
            except TcpError as e:
                assert e.kind == TcpErrorKind.CLOSED
                tx = None
            closer.join(5.0)
            assert not closer.is_alive(), "close() did not return"
            if tx is not None:
                try:
                    _send_and_read(tx, client)
                finally:
                    tx.close()
        finally:
            client.close()
            listener.close()


def test_a_conversion_does_not_wait_behind_a_recv_on_another_thread() -> None:
    client, accepted, listener = _accepted_pair()
    ended: list[object] = []

    def recv_once() -> None:
        try:
            ended.append(accepted.recv(bytearray(4096)))
        except TcpError as e:
            ended.append(e.kind)

    reader = threading.Thread(target=recv_once, daemon=True)
    try:
        reader.start()
        blocked, outcome = call_with_hang_deadline(accepted.into_receiver, 10.0)
        if blocked:
            accepted.cancel_handle().cancel()
        assert not blocked, "the conversion waited behind the recv()"
        if "exc" in outcome:
            # The recv held the connection: nothing was consumed.
            assert isinstance(outcome["exc"], RuntimeError), outcome["exc"]
            assert accepted.peer_addr() == client.getsockname()
            client.sendall(_PAYLOAD)
            reader.join(5.0)
            assert ended == [len(_PAYLOAD)]
        else:
            # The conversion ran first: the recv found the transport consumed.
            reader.join(5.0)
            assert ended == [TcpErrorKind.CLOSED]
            outcome["value"].close()  # type: ignore[attr-defined]
    finally:
        accepted.close()
        reader.join(5.0)
        client.close()
        listener.close()


def test_a_conversion_after_a_cancel_reports_closed_even_behind_a_recv() -> None:
    # The cancel latch is the one `close()` sets first. Right after it fires,
    # the recv it woke may still be counted in flight (it notices within a
    # poll tick); the conversion must answer CLOSED, never the
    # "holds the connection" RuntimeError, whichever side of that it lands.
    for _ in range(20):
        client, accepted, listener = _accepted_pair()
        started = threading.Event()
        ended: list[object] = []

        def recv_once() -> None:
            started.set()
            try:
                ended.append(accepted.recv(bytearray(4096)))
            except TcpError as e:
                ended.append(e.kind)

        reader = threading.Thread(target=recv_once, daemon=True)
        try:
            reader.start()
            assert started.wait(5.0)
            accepted.cancel_handle().cancel()
            with pytest.raises(TcpError) as ei:
                accepted.into_receiver()
            assert ei.value.kind == TcpErrorKind.CLOSED
            reader.join(5.0)
            assert ended == [TcpErrorKind.CLOSED]
        finally:
            accepted.close()
            reader.join(5.0)
            client.close()
            listener.close()
