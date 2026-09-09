"""Type stubs for `tstrans.udp` — raw UDP transport bindings (Plan A5b Wave A).

Mirrors the `Transport`, `RecvTransport`, `TransportBuilder`,
`RecvTransportBuilder`, and `SocketStats` PyClass-backed types exported from
`bindings/python/src/udp/mod.rs`.

``UdpError`` and ``UdpErrorKind`` live in ``tstrans.exceptions`` and are
re-exported here for convenience.

mypy --strict clean.
"""

from __future__ import annotations

from enum import IntEnum
from types import TracebackType
from typing import Any, Optional, Type, Union, final

# A bytes-like input — `bytes`, `bytearray`, `memoryview`, NumPy uint8,
# or any object implementing the buffer protocol. Concrete extraction
# happens in Rust via a two-path fast/fallback pattern.
_BytesLike = Union[bytes, bytearray, memoryview, Any]

__all__: list[str] = [
    "Transport",
    "RecvTransport",
    "TransportBuilder",
    "RecvTransportBuilder",
    "SocketStats",
    "UdpError",
    "UdpErrorKind",
]

# ---------------------------------------------------------------------------
# UdpErrorKind / UdpError — re-exported from tstrans.exceptions
# ---------------------------------------------------------------------------


class UdpErrorKind(IntEnum):
    """Discriminator for ``UdpError.kind``. Combines ``tst_udp::UdpErrorKind``'s
    own variants (URL, IO, INVALID_CONFIG) with transport-level kinds the
    ``tstrans.udp`` binding maps from ``tst_core::transport::TransportError``
    onto the same exception (CLOSED, PAYLOAD_TOO_LARGE)."""

    URL = 0
    IO = 2
    PAYLOAD_TOO_LARGE = 4
    CLOSED = 5
    INVALID_CONFIG = 6


class UdpError(Exception):
    """Raised by ``tstrans.udp`` operations."""

    kind: UdpErrorKind
    message: str

    def __init__(self, *, kind: UdpErrorKind, message: str) -> None: ...


# ---------------------------------------------------------------------------
# SocketStats — frozen wire-level statistics snapshot
# ---------------------------------------------------------------------------


@final
class SocketStats:
    """Frozen cumulative stats snapshot for a single UDP transport handle.

    Returned by ``Transport.stats()`` and ``RecvTransport.stats()``.
    Send-side counters are zero on a receive-only handle and vice-versa.
    All fields are non-negative integers; they never wrap (saturating add).
    """

    datagrams_sent: int
    """Datagrams successfully transmitted (sender only)."""
    bytes_sent: int
    """Bytes successfully transmitted (sender only)."""
    datagrams_received: int
    """Datagrams successfully received (receiver only)."""
    bytes_received: int
    """Bytes successfully received (receiver only)."""
    send_errors: int
    """Send-side I/O errors; excludes ``PAYLOAD_TOO_LARGE`` rejects."""
    recv_errors: int
    """Receive-side I/O errors; excludes ``WouldBlock``/``TimedOut``."""

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# Transport — UDP sender
# ---------------------------------------------------------------------------


@final
class Transport:
    """Raw UDP sender wrapping ``tst_udp::UdpTransport``.

    Construct via ``Transport.builder().url("udp://host:port").build()``.
    Sends to a fixed peer; to change destination close and rebuild.
    Supports the context-manager protocol (``with``).
    """

    @staticmethod
    def builder() -> TransportBuilder:
        """Return a fresh builder. Chain setters then call ``.build()``."""
        ...

    def send(self, payload: _BytesLike) -> None:
        """Send one datagram payload.

        Accepts ``bytes``, ``bytearray``, ``memoryview``, or any
        buffer-protocol object. Releases the GIL during the kernel send.

        Raises
        ------
        UdpError(kind=PAYLOAD_TOO_LARGE)
            If ``len(payload)`` exceeds the configured ``pkt_size``
            (default 1316 bytes = 7 × 188 TS packets).
        UdpError(kind=CLOSED)
            If the transport has been closed.
        UdpError(kind=IO)
            On underlying socket errors.
        """
        ...

    def close(self) -> None:
        """Close the sender. Idempotent."""
        ...

    def stats(self) -> SocketStats:
        """Return a frozen cumulative stats snapshot.

        ``datagrams_sent`` / ``bytes_sent`` tick on each successful
        ``send()``.
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
# TransportBuilder — builder for Transport
# ---------------------------------------------------------------------------


@final
class TransportBuilder:
    """Builder for ``Transport``. All setters return ``self`` for chaining."""

    def url(self, s: str) -> TransportBuilder:
        """Set the destination URL. Required. Must be ``udp://host:port``."""
        ...

    def pkt_size(self, v: int) -> TransportBuilder:
        """Override datagram payload cap (default 1316 = 7 × 188 bytes)."""
        ...

    def tos(self, v: int) -> TransportBuilder:
        """IP TOS / DSCP byte (e.g. ``0xb8`` for Expedited Forwarding)."""
        ...

    def sndbuf(self, v: int) -> TransportBuilder:
        """``SO_SNDBUF`` size in bytes."""
        ...

    def ttl(self, v: int) -> TransportBuilder:
        """Multicast TTL / IPv6 hop limit (1–255)."""
        ...

    def build(self) -> Transport:
        """Build the ``Transport``.

        Raises
        ------
        ValueError
            If ``url(...)`` was not called.
        UdpError(kind=URL)
            For a malformed URL.
        UdpError(kind=IO)
            On socket bind/connect failure.
        """
        ...

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# RecvTransport — UDP receiver
# ---------------------------------------------------------------------------


@final
class RecvTransport:
    """Raw UDP receiver wrapping ``tst_udp::UdpRecvTransport``.

    Construct via
    ``RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()``.
    Binding to port 0 lets the kernel pick a free port; read it back via
    ``local_addr_port()``. Supports the context-manager protocol.
    """

    @staticmethod
    def builder() -> RecvTransportBuilder:
        """Return a fresh builder. Chain setters then call ``.build()``."""
        ...

    def recv(self, timeout_ms: int | None = None) -> tuple[bytes, str]:
        """Receive one datagram.

        Returns ``(payload_bytes, sender_addr_str)``. The sender address
        string is currently always ``""``; the underlying ``recv_bytes``
        API does not expose it.

        No cross-thread cancel handle: there is no race-free way to interrupt
        a live ``recv()`` from another thread; ``close()`` is only safe to call
        after ``recv()`` returns. Use a finite ``timeout_ms`` and check a stop
        flag between calls for cooperative shutdown.

        Parameters
        ----------
        timeout_ms:
            Milliseconds to wait. ``None`` (default) blocks indefinitely.

        Raises
        ------
        UdpError(kind=IO)
            On timeout (message: "recv timed out") or socket error.
        UdpError(kind=CLOSED)
            If the transport has been closed.
        """
        ...

    def local_addr_port(self) -> int:
        """Local bound port. Non-zero after successful ``build()``.

        Use this to discover the ephemeral port when ``bind_url`` was
        ``udp://0.0.0.0:0``.
        """
        ...

    def close(self) -> None:
        """Close the receiver. Idempotent."""
        ...

    def stats(self) -> SocketStats:
        """Return a frozen cumulative stats snapshot.

        ``datagrams_received`` / ``bytes_received`` tick on each
        successful ``recv()``.
        """
        ...

    def __enter__(self) -> RecvTransport: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# RecvTransportBuilder — builder for RecvTransport
# ---------------------------------------------------------------------------


@final
class RecvTransportBuilder:
    """Builder for ``RecvTransport``. All setters return ``self``
    for chaining."""

    def bind_url(self, s: str) -> RecvTransportBuilder:
        """Set the bind URL. Required.

        Must be ``udp://bind_addr:port`` or ``udp://@group:port`` for
        multicast receive.
        """
        ...

    def rcvbuf(self, v: int) -> RecvTransportBuilder:
        """``SO_RCVBUF`` size in bytes."""
        ...

    def iface(self, s: str) -> RecvTransportBuilder:
        """Multicast interface name or literal IP for the join call."""
        ...

    def build(self) -> RecvTransport:
        """Build the ``RecvTransport``.

        Raises
        ------
        ValueError
            If ``bind_url(...)`` was not called.
        UdpError(kind=URL)
            For a malformed bind URL.
        UdpError(kind=IO)
            On socket bind failure.
        """
        ...

    def __repr__(self) -> str: ...
