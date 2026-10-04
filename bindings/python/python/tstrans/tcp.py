"""tstrans.tcp -- raw TCP transport bindings (Plan A5b Wave B).

Available when tstrans was built with the `tcp` cargo feature (default-on
in published wheels). Raises `ImportError` on a source build without
``--features tcp``.

Classes
-------
Transport
    Raw TCP transport (send + recv on one handle). Construct via
    ``Transport.builder().url("tcp://host:port").build()``.
TransportBuilder
    Builder for ``Transport`` -- supports ``url``, ``nodelay``,
    ``keepalive_ms``, ``rcvbuf``, ``sndbuf``, ``pkt_size``,
    ``connect_timeout_ms`` knobs before ``build()``.
Listener
    TCP listener. Construct via
    ``Listener.builder().bind("host:port").build()``, then call
    ``accept_blocking()`` per inbound connection.
ListenerBuilder
    Builder for ``Listener`` -- supports ``bind``, ``nodelay``,
    ``rcvbuf``, ``sndbuf``, ``pkt_size`` knobs before ``build()``.
SocketStats
    Frozen stats snapshot returned by ``Transport.stats()``.
CancelHandle
    Cross-thread cancel handle from ``Transport.cancel_handle()`` /
    ``Listener.cancel_handle()``.
TlsConfig
    Forward-compat TLS configuration dataclass. Working ``tcps://`` knobs:
    ``?ca=`` URL param for callers, ``ListenerBuilder.tls(cert, key)`` for
    listeners.
ClientCert
    PEM-encoded client certificate + key for mutual TLS.
TcpError
    Base exception for all ``tstrans.tcp`` errors; available in
    ``tstrans.exceptions`` as well.
TcpErrorKind
    ``IntEnum`` discriminator on ``TcpError.kind``.
"""

from __future__ import annotations

from . import _native

try:
    _tcp = _native.tcp
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.tcp is unavailable. Published wheels include TCP by default; "
        "if you built from source, enable the `tcp` cargo feature (on by default)."
    ) from exc

# Re-export native classes so users can write ``from tstrans.tcp import Transport``.
Transport = _tcp.Transport
TransportBuilder = _tcp.TransportBuilder
Listener = _tcp.Listener
ListenerBuilder = _tcp.ListenerBuilder
SocketStats = _tcp.SocketStats
CancelHandle = _tcp.CancelHandle
TlsConfig = _tcp.TlsConfig
ClientCert = _tcp.ClientCert

# TcpError + TcpErrorKind live in tstrans.exceptions but are also exposed
# here for convenience (mirrors the rtp/srt/udp pattern).
from .exceptions import TcpError, TcpErrorKind  # noqa: E402

__all__: list[str] = [
    "Transport",
    "TransportBuilder",
    "Listener",
    "ListenerBuilder",
    "SocketStats",
    "CancelHandle",
    "TlsConfig",
    "ClientCert",
    "TcpError",
    "TcpErrorKind",
]
