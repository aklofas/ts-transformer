"""Type stubs for `tstrans.tcp` -- raw TCP transport bindings (Plan A5b Wave B).

Mirrors the `Transport`, `TransportBuilder`, `Listener`, `ListenerBuilder`,
`SocketStats`, `TlsConfig`, and `ClientCert` PyClass-backed types exported from
`bindings/python/src/tcp/mod.rs`.

``TcpError`` and ``TcpErrorKind`` live in ``tstrans.exceptions`` and are
re-exported here for convenience.

mypy --strict clean.
"""

from __future__ import annotations

from enum import IntEnum
from types import TracebackType
from typing import Any, Optional, Type, Union, final

# A bytes-like input -- `bytes`, `bytearray`, `memoryview`, NumPy uint8,
# or any object implementing the buffer protocol. Concrete extraction
# happens in Rust via a two-path fast/fallback pattern.
_BytesLike = Union[bytes, bytearray, memoryview, Any]

__all__: list[str] = [
    "Transport",
    "TransportBuilder",
    "Listener",
    "ListenerBuilder",
    "SocketStats",
    "TlsConfig",
    "ClientCert",
    "TcpError",
    "TcpErrorKind",
]

# ---------------------------------------------------------------------------
# TcpErrorKind / TcpError -- re-exported from tstrans.exceptions
# ---------------------------------------------------------------------------


class TcpErrorKind(IntEnum):
    """Discriminator for ``TcpError.kind``. Combines ``tst_tcp::TcpErrorKind``
    construction/config-time variants with select ``TransportError``
    conditions mapped onto the same kinds at the send/recv boundary."""

    URL = 0
    IO = 1
    PAYLOAD_TOO_LARGE = 2
    CLOSED = 3
    CONNECT_TIMEOUT = 4
    INVALID_CONFIG = 5
    TLS = 6
    TLS_DISABLED = 7


class TcpError(Exception):
    """Raised by ``tstrans.tcp`` operations."""

    kind: TcpErrorKind
    message: str

    def __init__(self, *, kind: TcpErrorKind, message: str) -> None: ...


# ---------------------------------------------------------------------------
# SocketStats -- frozen wire-level statistics snapshot
# ---------------------------------------------------------------------------


@final
class SocketStats:
    """Frozen cumulative stats snapshot for a TCP transport handle.

    Returned by ``Transport.stats()``. Both send and receive counters are
    populated (TCP is full-duplex). All fields are non-negative integers;
    they never wrap (saturating add).
    """

    bytes_sent: int
    """Bytes successfully transmitted."""
    bytes_received: int
    """Bytes successfully received."""
    send_calls: int
    """Number of successful ``send()`` calls."""
    recv_calls: int
    """Number of successful ``recv()`` calls."""
    send_errors: int
    """Send-side I/O errors; excludes ``PAYLOAD_TOO_LARGE`` rejects."""
    recv_errors: int
    """Receive-side I/O errors."""

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# TlsConfig / ClientCert -- forward-compat TLS dataclasses
# ---------------------------------------------------------------------------


@final
class ClientCert:
    """Client certificate for mutual TLS authentication.

    **Note:** forward-compat only — mTLS client certificates have no
    tst-tcp backend yet. Server verification uses the ``?ca=`` URL param;
    listener certs go through ``ListenerBuilder.tls(cert, key)``.
    """

    cert_pem: bytes
    """PEM-encoded client certificate."""
    key_pem: bytes
    """PEM-encoded private key. Treat as sensitive."""

    def __new__(cls, cert_pem: bytes, key_pem: bytes) -> ClientCert: ...
    def __repr__(self) -> str: ...


@final
class TlsConfig:
    """TLS configuration for ``tcps://`` transports.

    **Note:** forward-compat only — the builder accepts but does not
    read this dataclass. The working knobs: callers verify against a
    custom CA with the ``?ca=<pem path>`` URL param (native trust roots
    otherwise); listeners serve TLS via ``ListenerBuilder.tls(cert, key)``.
    """

    ca_pem: bytes
    """PEM-encoded CA certificate bundle for server verification."""
    verify_hostname: bool
    """If ``True`` (default), verifies server hostname against the cert."""
    client_cert: ClientCert | None
    """Optional client certificate for mutual TLS."""

    def __new__(cls,
        ca_pem: Optional[bytes] = None,
        *,
        verify_hostname: bool = True,
        client_cert: ClientCert | None = None,
    ) -> TlsConfig: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# Transport -- TCP transport (send + recv on one handle)
# ---------------------------------------------------------------------------


@final
class Transport:
    """Raw TCP transport wrapping ``tst_tcp::TcpTransport``.

    Implements both sender and receiver roles on a single handle. TCP is
    full-duplex -- the caller decides whether to use this handle for
    sending, receiving, or both. There is no enforcement of mutual exclusion.

    Construct via
    ``Transport.builder().url("tcp://host:port").build()``, or obtain
    an accepted connection from ``Listener.accept_blocking()``.
    Supports the context-manager protocol (``with``).
    """

    @staticmethod
    def builder() -> TransportBuilder:
        """Return a fresh builder. Chain setters then call ``.build()``."""
        ...

    def send(self, payload: _BytesLike) -> None:
        """Send a payload over the TCP connection.

        Accepts ``bytes``, ``bytearray``, ``memoryview``, or any
        buffer-protocol object. Releases the GIL during the kernel send.

        Raises
        ------
        TcpError(kind=PAYLOAD_TOO_LARGE)
            If ``len(payload)`` exceeds the configured ``pkt_size``
            (default 64 KiB).
        TcpError(kind=CLOSED)
            If the transport has been closed.
        TcpError(kind=IO)
            On underlying socket errors.
        """
        ...

    def recv(self, buf: bytearray) -> int:
        """Receive bytes into a pre-allocated ``bytearray``.

        Returns the number of bytes written into ``buf``. The buffer
        must be large enough to hold the incoming data (at least
        ``pkt_size`` bytes on the sender side, default 64 KiB).

        Releases the GIL while blocking on kernel recv.

        Raises
        ------
        TcpError(kind=CLOSED)
            If the transport has been closed.
        TcpError(kind=IO)
            On connection errors, including graceful peer close.
        """
        ...

    def peer_addr(self) -> str:
        """Peer address as ``"host:port"``. Returns ``""`` if closed."""
        ...

    def close(self) -> None:
        """Close the transport. Idempotent."""
        ...

    def stats(self) -> SocketStats:
        """Return a frozen cumulative stats snapshot.

        ``bytes_sent`` / ``send_calls`` tick on each successful
        ``send()``. ``bytes_received`` / ``recv_calls`` tick on each
        successful ``recv()``.
        """
        ...

    def __enter__(self) -> Transport: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# TransportBuilder -- builder for Transport
# ---------------------------------------------------------------------------


@final
class TransportBuilder:
    """Builder for ``Transport``. All setters return ``self`` for chaining."""

    def url(self, s: str) -> TransportBuilder:
        """Set the destination URL. Required.

        Must be ``tcp://host:port`` or ``tcps://host:port`` (TLS;
        custom CA via ``?ca=<pem path>``, native trust roots otherwise).
        ``tcps://`` raises ``TcpError(kind=TLS_DISABLED)`` at build time
        only in source builds without the ``tls`` feature.
        """
        ...

    def nodelay(self, v: bool) -> TransportBuilder:
        """Enable or disable TCP_NODELAY (Nagle's algorithm).

        ``True`` reduces latency at the cost of throughput on small
        writes. Typically preferred for low-latency streaming.
        """
        ...

    def keepalive_ms(self, v: int) -> TransportBuilder:
        """Set SO_KEEPALIVE idle timeout in milliseconds."""
        ...

    def rcvbuf(self, v: int) -> TransportBuilder:
        """``SO_RCVBUF`` size in bytes."""
        ...

    def sndbuf(self, v: int) -> TransportBuilder:
        """``SO_SNDBUF`` size in bytes."""
        ...

    def pkt_size(self, v: int) -> TransportBuilder:
        """Maximum payload chunk size per ``send()`` call (default 64 KiB)."""
        ...

    def connect_timeout_ms(self, v: int) -> TransportBuilder:
        """Connection timeout in milliseconds (default 10 000 ms)."""
        ...

    def build(self) -> Transport:
        """Establish the TCP connection.

        Raises
        ------
        ValueError
            If ``url(...)`` was not called.
        TcpError(kind=URL)
            For a malformed URL.
        TcpError(kind=CONNECT_TIMEOUT)
            If the connection timed out.
        TcpError(kind=IO)
            On connection refused / unreachable.
        TcpError(kind=TLS_DISABLED)
            If ``tcps://`` was used but TLS is not compiled in.
        """
        ...

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# Listener -- TCP listener
# ---------------------------------------------------------------------------


@final
class Listener:
    """TCP listener wrapping ``tst_tcp::TcpListener``.

    Construct via ``Listener.builder().bind("host:port").build()``, then
    call ``accept_blocking()`` for each inbound connection.

    Binding to port 0 lets the kernel pick a free ephemeral port; read
    it back via ``local_port()`` before accepting.
    Supports the context-manager protocol (``with``).
    """

    @staticmethod
    def builder() -> ListenerBuilder:
        """Return a fresh builder. Chain setters then call ``.build()``."""
        ...

    def accept_blocking(self) -> Transport:
        """Block until a new inbound connection arrives.

        Returns a ``Transport`` wrapping the accepted connection. The
        returned transport can be used for sending, receiving, or both.

        Releases the GIL while waiting.

        Raises
        ------
        TcpError(kind=IO)
            On accept failure.
        TcpError(kind=CLOSED)
            If the listener has been closed.
        """
        ...

    def local_port(self) -> int:
        """Local bound port. Non-zero after successful ``build()``.

        Use this to discover the ephemeral port when ``bind("host:0")``
        was used.
        """
        ...

    def close(self) -> None:
        """Close the listener. Idempotent."""
        ...

    def __enter__(self) -> Listener: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# ListenerBuilder -- builder for Listener
# ---------------------------------------------------------------------------


@final
class ListenerBuilder:
    """Builder for ``Listener``. All setters return ``self`` for chaining."""

    def bind(self, s: str) -> ListenerBuilder:
        """Set the bind address as ``"host:port"``. Required.

        Examples: ``"127.0.0.1:0"`` (loopback, ephemeral),
        ``"0.0.0.0:5001"`` (all interfaces, fixed port).
        Port 0 requests an ephemeral port from the kernel.
        """
        ...

    def nodelay(self, v: bool) -> ListenerBuilder:
        """Enable or disable TCP_NODELAY for accepted connections."""
        ...

    def rcvbuf(self, v: int) -> ListenerBuilder:
        """``SO_RCVBUF`` size in bytes for accepted connections."""
        ...

    def sndbuf(self, v: int) -> ListenerBuilder:
        """``SO_SNDBUF`` size in bytes for accepted connections."""
        ...

    def pkt_size(self, v: int) -> ListenerBuilder:
        """Maximum payload chunk size for accepted connections (default 64 KiB)."""
        ...

    def tls(self, cert: str, key: str) -> ListenerBuilder:
        """Serve TLS (``tcps://``) on accepted connections.

        ``cert`` / ``key`` are PEM certificate-chain / private-key *file
        paths*, read at ``build()`` (missing or malformed files raise
        ``TcpError(kind=TLS)``). A source build without the ``tls``
        feature raises ``TcpError(kind=TLS_DISABLED)`` at ``build()``.
        A path containing ``&``, ``#``, or ``?`` raises
        ``TcpError(kind=INVALID_CONFIG)`` at ``build()``.
        """
        ...

    def build(self) -> Listener:
        """Bind the listener socket.

        Raises
        ------
        ValueError
            If ``bind(...)`` was not called.
        TcpError(kind=IO)
            If the port is already in use or permissions are insufficient.
        """
        ...

    def __repr__(self) -> str: ...
