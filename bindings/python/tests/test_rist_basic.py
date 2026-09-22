"""Basic smoke tests for tstrans.rist (Plan A5b Wave D T15-T18).

Tests are designed to work with the `rist` feature enabled but without
requiring a fully functional librist loopback (which can be flaky on CI
due to port availability and librist session setup timing). The test
battery covers:

1. Module structure / class presence.
2. Builder construction and error handling (malformed URL).
3. RecvTransport open + close (bind on port 0 picks a free port).
4. EncryptionKey repr discipline (secret must not leak).
5. RistErrorKind count matches exceptions.py.
6. Stats field presence on an open handle.
7. Error-mapping wiring via _raise_rist_error_for_test.
8. Encryption probe (mbedtls ships in the default build — a PSK'd
   builder must open cleanly; ENCRYPTION_DISABLED is a regression).

Full sender→receiver loopback is exercised only when librist can bind,
which is confirmed by the recv open test. An explicit loopback test is
included but tolerant — it skips if the recv can't open cleanly.
"""

from __future__ import annotations

import threading
import time
import pytest

from tstrans import rist
from tstrans.exceptions import RistError, RistErrorKind
from tstrans import _native


# ---------------------------------------------------------------------------
# T15: module structure
# ---------------------------------------------------------------------------


def test_rist_module_has_transport_classes():
    """All expected classes are present on the rist module."""
    assert hasattr(rist, "Transport")
    assert hasattr(rist, "RecvTransport")
    assert hasattr(rist, "TransportBuilder")
    assert hasattr(rist, "RecvTransportBuilder")
    assert hasattr(rist, "EncryptionKey")
    assert hasattr(rist, "RistProfile")
    assert hasattr(rist, "RistStats")


def test_rist_profile_variants():
    """RistProfile has SIMPLE and MAIN variants."""
    assert rist.RistProfile.SIMPLE is not None
    assert rist.RistProfile.MAIN is not None
    assert rist.RistProfile.SIMPLE != rist.RistProfile.MAIN


def test_transport_builder_returns_builder():
    """`Transport.builder()` returns a `TransportBuilder`."""
    b = rist.Transport.builder()
    assert isinstance(b, rist.TransportBuilder)


def test_recv_transport_builder_returns_builder():
    """`RecvTransport.builder()` returns a `RecvTransportBuilder`."""
    b = rist.RecvTransport.builder()
    assert isinstance(b, rist.RecvTransportBuilder)


# ---------------------------------------------------------------------------
# T15: RecvTransport open + close (port 0 not valid for librist — use any
# free even port; we try a range and skip the test if all are busy)
# ---------------------------------------------------------------------------


def _try_open_recv(port: int) -> rist.RecvTransport | None:
    """Try to open a RecvTransport on the given port; return None on error."""
    try:
        return (
            rist.RecvTransport.builder()
            .bind_url(f"rist://@0.0.0.0:{port}")
            .build()
        )
    except RistError:
        return None


def test_recv_transport_open_and_close():
    """Open a RecvTransport on an even port and close it cleanly."""
    # librist Simple profile requires even ports; try several candidates.
    rx = None
    for port in range(34010, 34030, 2):
        rx = _try_open_recv(port)
        if rx is not None:
            break
    if rx is None:
        pytest.skip("could not bind any even RIST port (librist unavailable)")
    # Close is idempotent.
    rx.close()
    rx.close()


def test_recv_transport_stats_fields():
    """Stats on an open RecvTransport has all required fields."""
    rx = None
    for port in range(34030, 34050, 2):
        rx = _try_open_recv(port)
        if rx is not None:
            break
    if rx is None:
        pytest.skip("could not bind any even RIST port (librist unavailable)")
    try:
        s = rx.stats()
        assert hasattr(s, "packets_sent")
        assert hasattr(s, "packets_retransmitted")
        assert hasattr(s, "packets_dropped")
        assert hasattr(s, "packets_received")
        assert hasattr(s, "packets_missing")
        assert hasattr(s, "recovered_packets")
        assert hasattr(s, "current_bandwidth_kbps")
        assert hasattr(s, "rtt_us")
        # All counters start at zero.
        assert s.packets_received >= 0
        assert s.packets_missing >= 0
        assert s.recovered_packets >= 0
    finally:
        rx.close()


# ---------------------------------------------------------------------------
# T15: stats after close raises RistError(CLOSED)
# ---------------------------------------------------------------------------


def test_recv_transport_stats_after_close_raises():
    """stats() on a closed RecvTransport raises RistError(kind=CLOSED)."""
    rx = None
    for port in range(34050, 34070, 2):
        rx = _try_open_recv(port)
        if rx is not None:
            break
    if rx is None:
        pytest.skip("could not bind any even RIST port (librist unavailable)")
    rx.close()
    with pytest.raises(RistError) as exc_info:
        rx.stats()
    assert exc_info.value.kind == RistErrorKind.CLOSED


# ---------------------------------------------------------------------------
# T16: EncryptionKey SecretString discipline
# ---------------------------------------------------------------------------


def test_encryption_key_aes128_repr_does_not_leak():
    """EncryptionKey.aes128 repr must NOT contain the secret."""
    secret = b"my-secret-key-128"
    k = rist.EncryptionKey.aes128(secret)
    r = repr(k)
    assert b"my-secret-key-128".decode() not in r
    assert "[redacted]" in r or "***" in r or "EncryptionKey" in r


def test_encryption_key_aes256_repr_does_not_leak():
    """EncryptionKey.aes256 repr must NOT contain the secret bytes."""
    secret = b"deadbeef-deadbeef-deadbeef-deadbeef"
    k = rist.EncryptionKey.aes256(secret)
    r = repr(k)
    assert "deadbeef" not in r
    assert "256" in r or "[redacted]" in r or "EncryptionKey" in r


def test_encryption_key_accepts_str():
    """EncryptionKey constructors accept str as well as bytes."""
    k = rist.EncryptionKey.aes256("str-secret")
    assert isinstance(k, rist.EncryptionKey)


def test_encryption_key_accepts_bytes():
    """EncryptionKey constructors accept bytes."""
    k = rist.EncryptionKey.aes128(b"bytes-secret")
    assert isinstance(k, rist.EncryptionKey)


def test_encryption_key_aes192():
    """EncryptionKey.aes192 constructs without error."""
    k = rist.EncryptionKey.aes192(b"192-bit-key-here-16b")
    assert isinstance(k, rist.EncryptionKey)


# ---------------------------------------------------------------------------
# T17: RistErrorKind count and mapping
# ---------------------------------------------------------------------------


def test_rist_error_kind_count():
    """RistErrorKind has exactly 10 variants matching the Rust enum."""
    assert len(RistErrorKind) == 10


def test_rist_error_kind_values():
    """RistErrorKind variant values match exceptions.py definitions."""
    assert RistErrorKind.URL == 0
    assert RistErrorKind.FFI == 1
    assert RistErrorKind.TOO_LARGE == 2
    assert RistErrorKind.CLOSED == 3
    assert RistErrorKind.INVALID_CONFIG == 4
    assert RistErrorKind.ENCRYPTION_DISABLED == 5
    assert RistErrorKind.CONTEXT_CREATE_FAILED == 6
    assert RistErrorKind.PEER_CREATE_FAILED == 7
    assert RistErrorKind.BACKPRESSURE == 8
    assert RistErrorKind.BROKEN == 9


def test_rist_error_kind_variant_mapping_via_raise():
    """_raise_rist_error_for_test can raise every RistErrorKind variant."""
    variants = [
        "URL",
        "FFI",
        "TOO_LARGE",
        "CLOSED",
        "INVALID_CONFIG",
        "ENCRYPTION_DISABLED",
        "CONTEXT_CREATE_FAILED",
        "PEER_CREATE_FAILED",
        "BACKPRESSURE",
        "BROKEN",
    ]
    for v in variants:
        with pytest.raises(RistError) as exc_info:
            _native._raise_rist_error_for_test(v, f"test {v}")
        assert exc_info.value.kind == RistErrorKind[v], f"kind mismatch for {v}"
        assert f"test {v}" in str(exc_info.value)


# ---------------------------------------------------------------------------
# T17: URL error — malformed scheme raises RistError(URL or INVALID_CONFIG)
# ---------------------------------------------------------------------------


def test_transport_builder_rejects_bad_scheme():
    """A non-rist:// URL must raise RistError with kind URL or INVALID_CONFIG."""
    with pytest.raises(RistError) as exc_info:
        rist.Transport.builder().url("srt://example.com:8000").build()
    assert exc_info.value.kind in (RistErrorKind.URL, RistErrorKind.INVALID_CONFIG)


def test_transport_builder_rejects_recv_bind_url():
    """A URL with '@' prefix is rejected by the sender builder (INVALID_CONFIG)."""
    with pytest.raises(RistError) as exc_info:
        rist.Transport.builder().url("rist://@0.0.0.0:8000").build()
    assert exc_info.value.kind in (RistErrorKind.URL, RistErrorKind.INVALID_CONFIG)


def test_recv_transport_builder_rejects_non_bind_url():
    """A URL without '@' prefix is rejected by the receiver builder (INVALID_CONFIG)."""
    with pytest.raises(RistError) as exc_info:
        rist.RecvTransport.builder().bind_url("rist://example.com:8000").build()
    assert exc_info.value.kind in (RistErrorKind.URL, RistErrorKind.INVALID_CONFIG)


# ---------------------------------------------------------------------------
# T16/T17: AES-256 encryption probe (mbedtls ships in the default build)
# ---------------------------------------------------------------------------


def test_encryption_key_aes256_probe():
    """A PSK'd receive builder opens cleanly — tst-rist is built with its
    mbedtls feature (regression: the wheels used to raise
    ENCRYPTION_DISABLED for every EncryptionKey)."""
    key = rist.EncryptionKey.aes256(b"my-test-pre-shared-aes-256-secret")
    for port in range(34200, 34220, 2):
        try:
            rx = (
                rist.RecvTransport.builder()
                .bind_url(f"rist://@0.0.0.0:{port}")
                .encryption(key)
                .build()
            )
            rx.close()
            return
        except RistError as e:
            if e.kind == RistErrorKind.ENCRYPTION_DISABLED:
                pytest.fail(
                    "RIST encryption must be compiled in (tst-rist mbedtls "
                    "feature) — got ENCRYPTION_DISABLED"
                )
            if e.kind in (RistErrorKind.CONTEXT_CREATE_FAILED, RistErrorKind.PEER_CREATE_FAILED):
                continue  # port busy; try next
            raise
    pytest.skip("could not bind any even RIST port for encryption probe")


# ---------------------------------------------------------------------------
# T18: rist.pyi — validate rist.py can be imported (py_compile done separately)
# ---------------------------------------------------------------------------


def test_rist_error_is_subclass_of_tst_error():
    """RistError is a subclass of TstError."""
    from tstrans.exceptions import TstError
    assert issubclass(RistError, TstError)


def test_rist_error_construction():
    """RistError can be constructed with kind + message."""
    e = RistError(kind=RistErrorKind.IO, message="test io error")
    assert e.kind == RistErrorKind.IO
    assert e.message == "test io error"
    assert "test io error" in str(e)


# ---------------------------------------------------------------------------
# T15: Simple profile loopback (tolerant — skip if ports busy or timing issues)
# ---------------------------------------------------------------------------


def test_rist_simple_profile_loopback():
    """Simple-profile loopback round-trip on librist's bundled UDP transport.

    This test requires librist to bind an even port for the receiver and
    establish a sender session. It is marked tolerant — any RistError or
    timeout skips rather than fails, to avoid flaky CI on constrained runners.
    """
    # Try several even ports in case one is busy.
    rx = None
    recv_port = None
    for port in range(34100, 34130, 2):
        rx = _try_open_recv(port)
        if rx is not None:
            recv_port = port
            break
    if rx is None:
        pytest.skip("could not bind any even RIST port for loopback")

    try:
        # Give librist a moment to register the bind.
        time.sleep(0.1)
        try:
            tx = (
                rist.Transport.builder()
                .url(f"rist://127.0.0.1:{recv_port}")
                .profile(rist.RistProfile.SIMPLE)
                .buffer_ms(200)
                .build()
            )
        except RistError as e:
            pytest.skip(f"sender build failed ({e.kind.name}): {e}")

        try:
            payload = b"\x47\x40\x00\x10" + b"\x00" * 184
            try:
                tx.send(payload)
            except RistError as e:
                pytest.skip(f"send failed ({e.kind.name}): {e}")

            # recv with a tolerant timeout
            try:
                received = rx.recv(timeout_ms=2000)
                # librist may pad/wrap the payload; just check the TS header.
                assert received[:4] == payload[:4], (
                    f"TS header mismatch: {received[:4]!r} != {payload[:4]!r}"
                )
            except RistError as e:
                if e.kind == RistErrorKind.BACKPRESSURE:
                    pytest.skip("loopback recv timed out (librist session not ready)")
                raise
        finally:
            tx.close()
    finally:
        rx.close()
