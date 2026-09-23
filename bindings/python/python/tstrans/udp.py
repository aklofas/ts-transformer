"""tstrans.udp — raw UDP transport bindings (Plan A5b Wave A).

Available when tstrans was built with the `udp` cargo feature (default-on
in published wheels). Raises `ImportError` on a source build without
``--features udp``.

Classes
-------
Transport
    Raw UDP sender. Construct via ``Transport.builder().url(...).build()``.
RecvTransport
    Raw UDP receiver. Construct via
    ``RecvTransport.builder().bind_url(...).build()``.
TransportBuilder
    Builder for ``Transport`` — supports ``url``, ``pkt_size``, ``tos``,
    ``sndbuf``, ``ttl`` knobs before ``build()``.
RecvTransportBuilder
    Builder for ``RecvTransport`` — supports ``bind_url``, ``rcvbuf``,
    ``pkt_size``, ``iface`` knobs before ``build()``.
SocketStats
    Frozen stats snapshot returned by ``Transport.stats()`` /
    ``RecvTransport.stats()``.
CancelHandle
    Cross-thread cancel handle from ``Transport.cancel_handle()`` /
    ``RecvTransport.cancel_handle()``.
UdpError
    Base exception for all ``tstrans.udp`` errors; available in
    ``tstrans.exceptions`` as well.
UdpErrorKind
    ``IntEnum`` discriminator on ``UdpError.kind``.
"""

from __future__ import annotations

from . import _native

try:
    _udp = _native.udp
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.udp is unavailable. Published wheels include UDP by default; "
        "if you built from source, enable the `udp` cargo feature (on by default)."
    ) from exc

# Re-export native classes so users can write ``from tstrans.udp import Transport``.
Transport = _udp.Transport
RecvTransport = _udp.RecvTransport
TransportBuilder = _udp.TransportBuilder
RecvTransportBuilder = _udp.RecvTransportBuilder
SocketStats = _udp.SocketStats
CancelHandle = _udp.CancelHandle

# UdpError + UdpErrorKind live in tstrans.exceptions but are also exposed
# here for convenience (mirrors the rtp/srt pattern).
from .exceptions import UdpError, UdpErrorKind  # noqa: E402

__all__: list[str] = [
    "Transport",
    "RecvTransport",
    "TransportBuilder",
    "RecvTransportBuilder",
    "SocketStats",
    "CancelHandle",
    "UdpError",
    "UdpErrorKind",
]
