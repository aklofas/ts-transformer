"""TCP transport loopback + error-case tests (Plan A5b Wave B T6/T7/T8/T9).

Covers:
- T6/T7: the 4 caller/listener x send/recv combo loopback round-trips
- T8: TLS dataclass round-trip (forward-compat) + tcps:// loopback e2e
- T9: error-kind propagation, TcpErrorKind count sentinel, test-helper wiring
"""

import threading

import pathlib

import pytest

from tstrans import tcp
from tstrans.exceptions import TcpError, TcpErrorKind

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_PAYLOAD = b"\x47\x40\x00\x10" + b"\x00" * 184  # one valid 188-byte TS packet


def _listener_and_port() -> tuple[tcp.Listener, int]:
    """Bind a listener on loopback port 0 and return (listener, port)."""
    listener = tcp.Listener.builder().bind("127.0.0.1:0").build()
    port = listener.local_port()
    assert port > 0
    return listener, port


# ---------------------------------------------------------------------------
# T6 + T7: 4 caller/listener x send/recv combos
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


def test_tcp_transport_peer_addr() -> None:
    """Transport.peer_addr() returns a non-empty string when open."""
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
    addr = caller.peer_addr()
    assert addr != ""
    assert str(port) in addr
    caller.close()
    t.join(timeout=5.0)
    listener.close()


# ---------------------------------------------------------------------------
# T8: TLS dataclass round-trip (forward-compat)
# ---------------------------------------------------------------------------


def test_tcp_tls_config_dataclass_round_trip() -> None:
    """TlsConfig is constructible and fields are accessible."""
    cfg = tcp.TlsConfig(ca_pem=b"-----BEGIN CERTIFICATE-----\n...", verify_hostname=True)
    assert cfg.verify_hostname is True
    assert cfg.ca_pem.startswith(b"-----BEGIN")


def test_tcp_tls_config_default_empty_pem() -> None:
    """TlsConfig() with no args uses an empty ca_pem."""
    cfg = tcp.TlsConfig()
    assert cfg.ca_pem == b""
    assert cfg.verify_hostname is True
    assert cfg.client_cert is None


def test_tcp_client_cert_dataclass() -> None:
    """ClientCert stores cert_pem and key_pem."""
    cert = tcp.ClientCert(cert_pem=b"CERT_PEM", key_pem=b"KEY_PEM")
    assert cert.cert_pem == b"CERT_PEM"
    assert cert.key_pem == b"KEY_PEM"


def test_tcp_tls_config_with_client_cert() -> None:
    """TlsConfig accepts a ClientCert in client_cert."""
    client_cert = tcp.ClientCert(cert_pem=b"CERT", key_pem=b"KEY")
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
# T9: error-kind propagation + count sentinel + test-helper wiring
# ---------------------------------------------------------------------------


def test_tcp_error_kind_count() -> None:
    """Sentinel: catches drift if Rust adds a new TcpErrorKind variant."""
    assert len(TcpErrorKind) == 10


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


def test_tcp_error_wiring_via_test_helper() -> None:
    """Verify make_tcp_error wiring for all 8 kind variants via test helper."""
    from tstrans._native import _raise_tcp_error_for_test

    for kind in TcpErrorKind:
        with pytest.raises(TcpError) as excinfo:
            _raise_tcp_error_for_test(kind.name, f"test {kind.name}")
        assert excinfo.value.kind == kind
