"""tstrans.rtp — RTP + RTSP bindings.

Available when tstrans was built with the `rtp` cargo feature
(default-on in published wheels). Source-built without `--features rtp`
will fail to import this submodule with a friendly ImportError.

Submodule contents are populated by `tstrans._native.rtp`:
- `Sender`, `Receiver`, `SocketStats`, `CancelHandle`
- `MuxSender`, `DemuxReceiver`
- `RtspClient`, `RtspSession`, `BasicAuth`, `DigestAuth`,
  `RtspClientConfig`, `RtspStats`, `RtspCancelHandle`,
  `DigestAlgorithm`, `TransportPref`, `RtspVersion`
- `RtspServer`, `MountHandle`, `RtspServerConfig`,
  `ServerStats`, `MountStats`, `RtspServerCancelHandle`
- `PublishMount`, `PublishMountStats`, `PublisherInfo`, `PublishShape`,
  `ClockAlignment` (the RTSP publisher role)
"""

from __future__ import annotations

import enum
from dataclasses import dataclass
from typing import Optional, Union

try:
    from . import _native
    _rtp = _native.rtp
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.rtp is unavailable. Wheels published to PyPI include "
        "RTP by default; if you built tstrans from source, ensure the "
        "`rtp` cargo feature is enabled (it is on by default)."
    ) from exc


class StreamEndReason(enum.IntEnum):
    """Why an RTP receive session ended. Mirrors `tst_rtp::StreamEndReason`.

    Returned by `.end_reason()` on `Receiver` / `DemuxReceiver` /
    `H264Receiver`; `None` means the session either hasn't ended yet or
    ended through a path this type doesn't instrument (e.g. a plain
    `rtp://` receiver that was never closed or cancelled).

    Numeric values are pinned across the C, Python, and JVM bindings
    (the C `TstStreamEndReason` enum additionally has `NONE = 0`; Python
    uses `None` for that case instead of a member). Pure Python — unlike
    the RTSP/transport config enums in this module (`RtspVersion`,
    `TransportPref`, ...), which are Rust-backed `#[pyclass(eq, eq_int)]`
    types because they cross the Python→Rust boundary as constructor
    arguments, `StreamEndReason` only ever flows Rust→Python as a return
    value, so it follows the same pure-Python-enum convention as
    `tstrans.mpegts.NonConformantKind` / `StreamKindTag`.

    `.end_detail()` carries the free-text `msg` for `KEEPALIVE_FAILED` /
    `TRANSPORT_FAILED` / `PROTOCOL_ERROR`; `None` for the other three
    variants.
    """

    CLEAN_TEARDOWN = 1
    SESSION_EXPIRED = 2
    KEEPALIVE_FAILED = 3
    TRANSPORT_FAILED = 4
    PROTOCOL_ERROR = 5
    CANCELLED = 6


class PublishShape(enum.IntEnum):
    """Wire shape a publisher's ANNOUNCE declared. Mirrors
    `tst_rtp::rtsp::server::publish::PublishShape`.

    Returned by `PublisherInfo.shape`. `MP2T` is one MPEG-TS-over-RTP
    track (RFC 2250); `ELEMENTARY` is one H.264 (RFC 6184) video track,
    optionally with one KLV (RFC 6597) track — `PublisherInfo.klv` says
    which. Numeric values are pinned across the C, Python, and JVM
    bindings. Pure Python for the same reason as `StreamEndReason`: it
    only ever flows Rust→Python as a return value.
    """

    MP2T = 0
    ELEMENTARY = 1


class ClockAlignment(enum.IntEnum):
    """How a publish mount aligns its announced tracks to one clock.
    Mirrors `tst_rtp::rtsp::server::publish::ClockAlignment`.

    Returned by `PublishMountStats.alignment`. Only an elementary
    publisher with a KLV track needs alignment; an MP2T or video-only
    mount reads `NOT_APPLICABLE`. `PENDING`: KLV is held until RTCP
    sender reports arrive for both tracks or the two-second fallback
    engages (also after a source restart). `PROVISIONAL`: aligned by
    first-packet coincidence. `SENDER_REPORT`: aligned through RTCP
    sender reports. Numeric values are pinned across the C, Python, and
    JVM bindings.
    """

    NOT_APPLICABLE = 0
    PENDING = 1
    PROVISIONAL = 2
    SENDER_REPORT = 3


# RTP transport types.
Sender = _rtp.Sender
Receiver = _rtp.Receiver
SocketStats = _rtp.SocketStats
CancelHandle = _rtp.CancelHandle

# MuxSender + DemuxReceiver convenience wrappers.
MuxSender = _rtp.MuxSender
DemuxReceiver = _rtp.DemuxReceiver

# RtspClient, RtspSession, auth, config, stats.
# BasicAuth + DigestAuth are PyClass-backed dataclass-equivalents living
# in src/rtp/client.rs (NOT pure-Python). Client auth and server auth use
# these same classes.
RtspClient = _rtp.RtspClient
RtspSession = _rtp.RtspSession
RtspClientConfig = _rtp.RtspClientConfig
RtspStats = _rtp.RtspStats
RtspCancelHandle = _rtp.RtspCancelHandle
BasicAuth = _rtp.BasicAuth
DigestAuth = _rtp.DigestAuth
DigestAlgorithm = _rtp.DigestAlgorithm
TransportPref = _rtp.TransportPref
RtspVersion = _rtp.RtspVersion

# RtspServer + MountHandle + server-side stats.
RtspServer = _rtp.RtspServer
MountHandle = _rtp.MountHandle
ServerStats = _rtp.ServerStats
MountStats = _rtp.MountStats
RtspServerCancelHandle = _rtp.RtspServerCancelHandle

# RTSP publisher role (ANNOUNCE / RECORD ingest on RtspServer).
PublishMount = _rtp.PublishMount
PublishMountStats = _rtp.PublishMountStats
PublisherInfo = _rtp.PublisherInfo

# RFC 6184 — H.264 depacketizer + blocking receiver.
ParameterSetInjection = _rtp.ParameterSetInjection
H264DepayConfig = _rtp.H264DepayConfig
H264AccessUnit = _rtp.H264AccessUnit
H264DepayStats = _rtp.H264DepayStats
RtpStats = _rtp.RtpStats
H264Receiver = _rtp.H264Receiver


# ---------------------------------------------------------------------------
# RtspServerConfig dataclass. Lives Python-side (not PyClass) because
# the underlying Rust builder takes a stream of fluent setter calls rather
# than a typed config struct; the dataclass is the natural Python shape and
# `RtspServer.start(cfg)` reads its attributes in `src/rtp/server.rs`.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class RtspServerConfig:
    """Configuration for :py:meth:`RtspServer.start`.

    All fields have sensible defaults; in the common case, callers only
    set `bind_addr` (and optionally `auth` for credentialed servers).
    """

    bind_addr: str = "0.0.0.0:8554"
    """Bind URL or `host:port` (the latter assumed `rtsp://`)."""

    auth: Optional[Union[BasicAuth, DigestAuth]] = None
    """Optional Basic / Digest auth challenge. `None` allows anonymous
    SETUP."""

    max_sessions: int = 100
    """Cap on concurrent client connections."""

    session_timeout_secs: int = 60
    """Advertised session timeout (seconds). Clients are expected to
    keepalive at timeout/2."""

    fanout_capacity: int = 256
    """Per-mount broadcast channel capacity (frames). Slow peers drop
    oldest beyond this; the muxer is never back-pressured."""

    graceful_shutdown_drain_ms: int = 2000
    """Drain window (ms) after `stop()` to let in-flight RTP finish."""

    tls_cert: Optional[str] = None
    """Path to a PEM server certificate chain file (for `rtsps://`
    binds). Set together with `tls_key`. Missing/unreadable paths
    raise `RtspError(TLS)` at `start()`. Path-based to match
    `hls.HlsPublisher`'s `enable_tls(cert, key)` and the tcps://
    listener convention."""

    tls_key: Optional[str] = None
    """Path to a PEM server private key file (for `rtsps://` binds).
    Set together with `tls_cert`."""

    accept_unregistered_publishers: bool = False
    """Accept an ANNOUNCE on a path with no registered mount by creating
    a publish mount there on demand; `RtspServer.next_publisher()` hands
    each one to the application. Off by default. Each on-demand
    publisher can hold tens of MB, so configure `auth` and size
    `max_sessions` before turning this on for a reachable port."""

    def __post_init__(self) -> None:
        if self.max_sessions <= 0:
            raise ValueError(
                f"RtspServerConfig.max_sessions must be > 0; got {self.max_sessions}"
            )
        if self.session_timeout_secs <= 0:
            raise ValueError(
                f"RtspServerConfig.session_timeout_secs must be > 0; "
                f"got {self.session_timeout_secs}"
            )
        if self.fanout_capacity <= 0:
            raise ValueError(
                f"RtspServerConfig.fanout_capacity must be > 0; "
                f"got {self.fanout_capacity}"
            )
        if self.graceful_shutdown_drain_ms < 0:
            raise ValueError(
                f"RtspServerConfig.graceful_shutdown_drain_ms must be >= 0; "
                f"got {self.graceful_shutdown_drain_ms}"
            )
        if not isinstance(self.accept_unregistered_publishers, bool):
            raise TypeError(
                f"RtspServerConfig.accept_unregistered_publishers must be a bool; "
                f"got {type(self.accept_unregistered_publishers).__name__}"
            )
        cert_set = self.tls_cert is not None
        key_set = self.tls_key is not None
        if cert_set != key_set:
            raise ValueError(
                "RtspServerConfig.tls_cert and tls_key must be set together "
                "(both or neither)"
            )


__all__: list[str] = [
    # transport
    "StreamEndReason",
    "Sender",
    "Receiver",
    "SocketStats",
    "CancelHandle",
    # mux/demux convenience wrappers
    "MuxSender",
    "DemuxReceiver",
    # RTSP client
    "RtspClient",
    "RtspSession",
    "RtspClientConfig",
    "RtspStats",
    "RtspCancelHandle",
    "BasicAuth",
    "DigestAuth",
    "DigestAlgorithm",
    "TransportPref",
    "RtspVersion",
    # RTSP server
    "RtspServer",
    "MountHandle",
    "ServerStats",
    "MountStats",
    "RtspServerCancelHandle",
    "RtspServerConfig",
    # RTSP publisher role
    "PublishMount",
    "PublishMountStats",
    "PublisherInfo",
    "PublishShape",
    "ClockAlignment",
    # RFC 6184 H.264 receiver
    "ParameterSetInjection",
    "H264DepayConfig",
    "H264AccessUnit",
    "H264DepayStats",
    "RtpStats",
    "H264Receiver",
]
