"""UDP transport loopback + error-case tests (Plan A5b Wave A T3/T4/T5).

Covers:
- T3: unicast loopback round-trip via Transport + RecvTransport
- T4: builder URL validation, local_addr_port, stats fields
- T5: TOO_LARGE error-kind propagation, UdpErrorKind variant count
"""

import pytest

from tstrans import udp
from tstrans.exceptions import UdpError, UdpErrorKind


# ---------------------------------------------------------------------------
# T3: unicast loopback round-trip
# ---------------------------------------------------------------------------


def test_udp_unicast_loopback_round_trip() -> None:
    """UdpTransport pushes raw bytes; UdpRecvTransport receives them."""
    # Bind receiver on port 0 — kernel picks a free ephemeral port.
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    assert port > 0

    tx = udp.Transport.builder().url(f"udp://127.0.0.1:{port}").build()

    payload = b"\x47\x40\x00\x10" + b"\x00" * 184  # one valid 188-byte TS packet
    tx.send(payload)

    received, _addr = rx.recv(timeout_ms=2000)
    assert received == payload

    tx.close()
    rx.close()


def test_udp_send_bytearray() -> None:
    """Transport.send() accepts bytearray (fallback bytes-like path)."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    tx = udp.Transport.builder().url(f"udp://127.0.0.1:{port}").build()

    payload = bytearray(b"\x47\x41\x00\x10" + b"\xff" * 184)
    tx.send(payload)
    received, _ = rx.recv(timeout_ms=2000)
    assert received == bytes(payload)

    tx.close()
    rx.close()


# ---------------------------------------------------------------------------
# T4: builders, URL validation, local_addr_port, stats
# ---------------------------------------------------------------------------


def test_udp_url_parse_rejects_bad_scheme() -> None:
    """TransportBuilder.build() raises UdpError(kind=URL) for wrong scheme."""
    with pytest.raises(UdpError) as excinfo:
        udp.Transport.builder().url("rtp://1.2.3.4:5004").build()
    assert excinfo.value.kind == UdpErrorKind.URL


def test_udp_url_parse_rejects_missing_url() -> None:
    """TransportBuilder.build() raises ValueError when url() was not called."""
    with pytest.raises(ValueError):
        udp.Transport.builder().build()


def test_udp_recv_url_parse_rejects_missing_url() -> None:
    """RecvTransportBuilder.build() raises ValueError when bind_url() not called."""
    with pytest.raises(ValueError):
        udp.RecvTransport.builder().build()


def test_udp_recv_local_addr_after_bind_zero() -> None:
    """local_addr_port() returns a non-zero port after binding to port 0."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    assert isinstance(port, int)
    assert port > 0
    rx.close()


def test_udp_stats_fields_default_zero_on_recv() -> None:
    """RecvTransport.stats() fields are zero before any recv()."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    s = rx.stats()
    assert s.datagrams_received == 0
    assert s.bytes_received == 0
    assert s.recv_errors == 0
    rx.close()


def test_udp_stats_fields_update_after_send() -> None:
    """Transport.stats() counters tick after a successful send."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    tx = udp.Transport.builder().url(f"udp://127.0.0.1:{port}").build()

    payload = b"\x47" * 188
    tx.send(payload)
    s = tx.stats()
    assert s.datagrams_sent == 1
    assert s.bytes_sent == len(payload)

    # Drain so the socket buffer is empty for the next test.
    rx.recv(timeout_ms=500)
    tx.close()
    rx.close()


def test_udp_transport_context_manager() -> None:
    """Transport supports the context-manager (with) protocol."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    with udp.Transport.builder().url(f"udp://127.0.0.1:{port}").build() as tx:
        tx.send(b"\x47" * 188)
    rx.close()


def test_udp_recv_transport_context_manager() -> None:
    """RecvTransport supports the context-manager (with) protocol."""
    with udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build() as rx:
        assert rx.local_addr_port() > 0


# ---------------------------------------------------------------------------
# T5: error-kind propagation + variant count sentinel
# ---------------------------------------------------------------------------


def test_udp_error_payload_too_large() -> None:
    """Transport.send() raises UdpError(kind=TOO_LARGE) for oversized payload."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    port = rx.local_addr_port()
    # pkt_size=188 means any payload > 188 bytes is rejected.
    tx = udp.Transport.builder().url(f"udp://127.0.0.1:{port}").pkt_size(188).build()
    huge = b"\x47" * 1316  # 7×188, exceeds cap of 188
    with pytest.raises(UdpError) as excinfo:
        tx.send(huge)
    assert excinfo.value.kind == UdpErrorKind.TOO_LARGE

    tx.close()
    rx.close()


def test_udp_error_recv_timeout() -> None:
    """RecvTransport.recv(timeout_ms=...) raises UdpError(kind=BACKPRESSURE)
    on timeout — retryable, the receiver stays open (was IO before 0.7.0)."""
    rx = udp.RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()
    with pytest.raises(UdpError) as excinfo:
        rx.recv(timeout_ms=50)  # very short timeout, no sender
    # An expired recv deadline is transient refusal, not an I/O failure.
    assert excinfo.value.kind == UdpErrorKind.BACKPRESSURE
    rx.close()


def test_udp_error_kind_count() -> None:
    """Sentinel: catches drift if Rust adds a new UdpErrorKind variant."""
    assert len(UdpErrorKind) == 7


def test_udp_error_wiring_via_test_helper() -> None:
    """Verify the raise path wiring for every canonical kind via test helper."""
    from tstrans._native import _raise_udp_error_for_test

    for kind in UdpErrorKind:
        with pytest.raises(UdpError) as excinfo:
            _raise_udp_error_for_test(kind.name, f"test {kind.name}")
        assert excinfo.value.kind == kind


# ---------------------------------------------------------------------------
# pkt_size recv rejection
# ---------------------------------------------------------------------------


def test_recv_builder_rejects_pkt_size_url():
    b = udp.RecvTransport.builder().bind_url("udp://@127.0.0.1:0?pkt_size=1316")
    with pytest.raises(UdpError) as ei:
        b.build()
    assert ei.value.kind == UdpErrorKind.URL
    assert "send-side knob" in str(ei.value)


def test_udp_recv_deadline_is_backpressure_and_keeps_the_handle_open() -> None:
    """Arc 2: a recv deadline expiry is BACKPRESSURE (retryable), the
    handle stays open, and a datagram delivered afterwards is received."""
    import socket as _socket

    rx = udp.RecvTransport.builder().bind_url("udp://127.0.0.1:0").build()
    try:
        with pytest.raises(UdpError) as ei:
            rx.recv(timeout_ms=50)
        assert ei.value.kind == UdpErrorKind.BACKPRESSURE
        assert "timed out" in str(ei.value)
        assert "open" in repr(rx)
        s = _socket.socket(_socket.AF_INET, _socket.SOCK_DGRAM)
        s.sendto(b"\x47" * 188, ("127.0.0.1", rx.local_addr_port()))
        s.close()
        payload, _ = rx.recv(timeout_ms=2000)
        assert payload == b"\x47" * 188
    finally:
        rx.close()
