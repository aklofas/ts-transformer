"""Type stubs for tstrans.rist (Plan A5b Wave D T18).

Available when tstrans was built with the ``rist`` cargo feature
(default-on in published wheels).
"""

from __future__ import annotations

from enum import IntEnum
from types import TracebackType
from typing import Optional, Self, Type, final

from .exceptions import RistError as RistError
from .exceptions import RistErrorKind as RistErrorKind

__all__: list[str] = [
    "RistProfile",
    "RistStats",
    "EncryptionKey",
    "Transport",
    "TransportBuilder",
    "RecvTransport",
    "RecvTransportBuilder",
    "RistError",
    "RistErrorKind",
]

# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


@final
class RistProfile(IntEnum):
    """RIST protocol profile.

    - ``SIMPLE`` — VSF TR-06-1: basic ARQ + multiplexing.
    - ``MAIN`` — VSF TR-06-2: adds encryption, RTCP, tunneling.
    """

    SIMPLE = 0
    MAIN = 1


# ---------------------------------------------------------------------------
# Stats
# ---------------------------------------------------------------------------


@final
class RistStats:
    """Cumulative stats snapshot returned by Transport/RecvTransport.stats()."""

    packets_sent: int
    packets_retransmitted: int
    packets_dropped: int
    packets_received: int
    packets_missing: int
    recovered_packets: int
    current_bandwidth_kbps: int
    rtt_us: int


# ---------------------------------------------------------------------------
# Encryption
# ---------------------------------------------------------------------------


@final
class EncryptionKey:
    """AES pre-shared key for RIST encryption.

    The secret is consumed at construction time and never exposed again.
    ``repr()`` shows only the key size.
    """

    @staticmethod
    def aes128(secret: bytes | str) -> EncryptionKey:
        """AES-128 PSK. ``secret`` may be ``bytes`` or ``str``."""
        ...

    @staticmethod
    def aes192(secret: bytes | str) -> EncryptionKey:
        """AES-192 PSK. ``secret`` may be ``bytes`` or ``str``."""
        ...

    @staticmethod
    def aes256(secret: bytes | str) -> EncryptionKey:
        """AES-256 PSK. ``secret`` may be ``bytes`` or ``str``."""
        ...


# ---------------------------------------------------------------------------
# Transport (sender)
# ---------------------------------------------------------------------------


@final
class Transport:
    """RIST sender. Construct via ``Transport.builder()``."""

    @staticmethod
    def builder() -> TransportBuilder:
        """Return a builder for configuring and constructing a ``Transport``."""
        ...

    def send(self, payload: bytes | bytearray | memoryview) -> None:
        """Send one payload.

        Raises ``RistError(kind=PAYLOAD_TOO_LARGE)`` if the payload exceeds
        the configured ``pkt_size``.

        Raises ``RistError(kind=CLOSED)`` if the transport is closed.
        """
        ...

    def close(self) -> None:
        """Close the sender. Idempotent."""
        ...

    def stats(self) -> RistStats:
        """Snapshot of cumulative wire-level statistics."""
        ...

    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> bool: ...


@final
class TransportBuilder:
    """Builder for ``Transport``. Chain setter calls, then call ``.build()``."""

    def url(self, s: str) -> Self:
        """Set the destination URL. Required. Must be ``rist://host:port``."""
        ...

    def profile(self, p: RistProfile) -> Self:
        """Override the RIST profile (``SIMPLE`` or ``MAIN``)."""
        ...

    def bandwidth_kbps(self, v: int) -> Self:
        """Sender bandwidth cap, kbps."""
        ...

    def buffer_ms(self, ms: int) -> Self:
        """Recovery buffer duration, milliseconds."""
        ...

    def cname(self, s: str) -> Self:
        """RTCP CNAME for this sender."""
        ...

    def encryption(self, key: EncryptionKey) -> Self:
        """AES encryption key. Forces profile to ``MAIN``."""
        ...

    def recovery_maxbitrate_kbps(self, v: int) -> Self:
        """Retransmit bandwidth cap, kbps."""
        ...

    def pkt_size(self, v: int) -> Self:
        """Per-send-call payload cap in bytes (default 1316)."""
        ...

    def compression(self, v: bool) -> Self:
        """Enable NULL-packet deletion / compression."""
        ...

    def build(self) -> Transport:
        """Build the ``Transport``.

        Raises:
            RistError(kind=URL): Bad destination URL.
            RistError(kind=INVALID_CONFIG): URL has ``@`` prefix (use RecvTransportBuilder).
            RistError(kind=CONTEXT_CREATE_FAILED): librist context init failed.
            RistError(kind=PEER_CREATE_FAILED): librist peer creation failed.
            RistError(kind=ENCRYPTION_DISABLED): encryption requested in a
                source build without the ``mbedtls`` feature (wheels ship it).
        """
        ...


# ---------------------------------------------------------------------------
# RecvTransport (receiver)
# ---------------------------------------------------------------------------


@final
class RecvTransport:
    """RIST receiver. Construct via ``RecvTransport.builder()``."""

    @staticmethod
    def builder() -> RecvTransportBuilder:
        """Return a builder for configuring and constructing a ``RecvTransport``."""
        ...

    def recv(self, timeout_ms: int | None = None) -> bytes:
        """Receive one payload.

        ``timeout_ms``: milliseconds to wait before raising
        ``RistError(kind=RECV_TIMEOUT)``. ``None`` (default) blocks until a
        packet arrives.

        Note: actual timeout latency may exceed ``timeout_ms`` by up to
        ~100 ms due to the internal librist poll window.

        No cross-thread cancel handle: there is no race-free way to interrupt
        a live ``recv()`` from another thread; ``close()`` is only safe to call
        after ``recv()`` returns. Use a finite ``timeout_ms`` and check a stop
        flag between calls for cooperative shutdown.

        Raises:
            RistError(kind=RECV_TIMEOUT): No packet within ``timeout_ms``.
            RistError(kind=CLOSED): Transport is closed.
            RistError(kind=IO): Underlying I/O error.
        """
        ...

    def close(self) -> None:
        """Close the receiver. Idempotent."""
        ...

    def stats(self) -> RistStats:
        """Snapshot of cumulative wire-level statistics."""
        ...

    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> bool: ...


@final
class RecvTransportBuilder:
    """Builder for ``RecvTransport``. Chain setter calls, then call ``.build()``."""

    def bind_url(self, s: str) -> Self:
        """Set the bind URL. Required. Must be ``rist://@bind_addr:port``.

        The ``@`` prefix is required and marks this as a receiver URL per
        librist / ffmpeg convention.
        """
        ...

    def profile(self, p: RistProfile) -> Self:
        """Override the RIST profile."""
        ...

    def buffer_ms(self, ms: int) -> Self:
        """Recovery buffer duration, milliseconds."""
        ...

    def cname(self, s: str) -> Self:
        """RTCP CNAME for this receiver."""
        ...

    def encryption(self, key: EncryptionKey) -> Self:
        """AES decryption key. Forces profile to ``MAIN``."""
        ...

    def session_timeout_ms(self, ms: int) -> Self:
        """Session timeout, milliseconds."""
        ...

    def build(self) -> RecvTransport:
        """Build the ``RecvTransport``.

        Raises:
            RistError(kind=URL): Bad bind URL.
            RistError(kind=INVALID_CONFIG): URL missing ``@`` prefix.
            RistError(kind=CONTEXT_CREATE_FAILED): librist context init failed.
            RistError(kind=PEER_CREATE_FAILED): librist peer creation failed.
        """
        ...
