"""Type stubs for `tstrans.rtp` — RTP + RTSP bindings.

Mirrors the public surface exported from
`bindings/python/python/tstrans/rtp.py`. Continues `py.typed` discipline.
mypy --strict clean.

The PyClass-backed types (Transport, RecvTransport, RtspClient, RtspSession,
RtspServer, MountHandle, etc.) live in `_native.rtp` (Rust source under
`bindings/python/src/rtp/`). The pure-Python dataclass `RtspServerConfig`
lives in `rtp.py`.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass
from typing import (
    Any,
    Callable,
    Iterator,
    List,
    Literal,
    Optional,
    Tuple,
    Type,
    Union,
    final,
)

# Cross-module types are imported from the `tstrans.mpegts` stub, which
# now ships a sibling `.pyi` declaring these as real classes.
from tstrans.shells import (
    CancelHandle as CancelHandle,
    DemuxReceiver as DemuxReceiver,
    MuxSender as MuxSender,
    SocketStats as SocketStats,
)
from tstrans.shells import EndReason as StreamEndReason
from tstrans.mpegts import (
    AudioStreamHandle,
    DataStreamHandle,
    DemuxerConfig,
    DemuxEvent,
    KlvStreamHandle,
    MuxerProgramConfig,
    MuxerStats,
    Pts90khz,
    SubtitleStreamHandle,
    VideoStreamHandle,
)

# A bytes-like input — `bytes`, `bytearray`, `memoryview`, NumPy uint8,
# or any object implementing the buffer protocol. Concrete extraction
# happens in Rust via a two-path fast/fallback pattern.
_BytesLike = Union[bytes, bytearray, memoryview, Any]

__all__: list[str] = [
    "Transport",
    "RecvTransport",
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
    "RtspServer",
    "MountHandle",
    "ServerStats",
    "MountStats",
    "RtspServerCancelHandle",
    "RtspServerConfig",
    "MountCredential",
    "MountPolicy",
    "MountRule",
    "MountRules",
    "RefuseStatus",
    "AuthScheme",
    "PublishMount",
    "PublishMountStats",
    "PublisherInfo",
    "PublishShape",
    "ClockAlignment",
    "ParameterSetInjection",
    "H264DepayConfig",
    "H264AccessUnit",
    "H264DepayStats",
    "RtpStats",
    "H264Receiver",
]

# ---------------------------------------------------------------------------
# Raw RTP transport (Transport / RecvTransport)
# ---------------------------------------------------------------------------


@final
class Transport:
    """Raw RTP sender (the TS shells over `rtp://` are in `tstrans.shells`),
    wrapping `tst_rtp::RtpTransport`.

    `pkt_size` overrides the UDP datagram size (RTP header + TS payload).
    `ssrc` pins the RTP synchronization source identifier; when omitted
    the transport picks a random one.
    """

    def __new__(cls,
        url: str,
        *,
        pkt_size: int = ...,
        ssrc: Optional[int] = ...,
    ) -> Transport: ...
    def send(self, ts_bytes: _BytesLike) -> None: ...
    def stats(self) -> SocketStats: ...
    def cancel_handle(self) -> CancelHandle: ...
    def close(self) -> None: ...
    def __enter__(self) -> Transport: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class RecvTransport:
    """Raw RTP receiver wrapping `tst_rtp::RtpRecvTransport`.

    Binds to `url` (literal IP:port). For multicast URLs, joins the
    group automatically. The receive buffer sizes itself to the
    transport's deliverable ceiling; ``?pkt_size=`` on a receiver URL
    is rejected.

    `end_reason()` / `end_detail()` report why the receive session
    ended (`EndReason` / free-text detail), or `None` for both
    while the session is still live. Readable after `close()`.
    """

    def __new__(cls, url: str) -> RecvTransport: ...
    def recv(self, timeout_ms: Optional[int] = ...) -> bytes: ...
    def stats(self) -> SocketStats: ...
    def cancel_handle(self) -> CancelHandle: ...
    def end_reason(self) -> Optional[StreamEndReason]: ...
    def end_detail(self) -> Optional[str]: ...
    def close(self) -> None: ...
    def __enter__(self) -> RecvTransport: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# Deprecated alias (removed in 0.10.0)
Sender = Transport
# Deprecated alias (removed in 0.10.0)
Receiver = RecvTransport


# ---------------------------------------------------------------------------
# RTSP client (enums, auth dataclasses, config, stats, session)
# ---------------------------------------------------------------------------


@final
class RtspVersion:
    """Wire-time RTSP version preference (IntEnum-shaped PyClass).

    Mirrors `tst_rtp::RtspVersion` — only the 1.0 / 2.0 split matters
    at the SETUP / PLAY layer.
    """

    V1_0: RtspVersion
    V2_0: RtspVersion


@final
class TransportPref:
    """Transport preference at SETUP time (IntEnum-shaped PyClass).

    `AUTO` = UDP-first with TCP fallback on 461 Unsupported Transport.
    `UDP` / `TCP` force a single transport (no fallback).
    """

    AUTO: TransportPref
    UDP: TransportPref
    TCP: TransportPref


@final
class DigestAlgorithm:
    """Digest authentication algorithm selector (IntEnum-shaped PyClass).

    `MD5` — RFC 7616 §3.4 with `algorithm=MD5`.
    `SHA256` — RFC 7616 §3.4 with `algorithm=SHA-256`.
    """

    MD5: DigestAlgorithm
    SHA256: DigestAlgorithm


@final
class BasicAuth:
    """HTTP Basic auth credentials per RFC 7617. Frozen.

    Password held in Rust memory and never re-exposed to Python; only
    `user` and `realm` are readable via getters. `realm` is optional
    and only used when this credential is passed as
    `RtspServerConfig.auth` (server-side challenge); client-side use
    reads the realm from the peer's 401.

    As a client credential it answers Digest when the server offers it
    and Basic otherwise; use `DigestAuth` to refuse Basic.
    """

    def __new__(cls,
        user: str,
        password: str,
        realm: str | None = None,
    ) -> BasicAuth: ...
    @property
    def user(self) -> str: ...
    @property
    def realm(self) -> str | None: ...
    def __repr__(self) -> str: ...


@final
class DigestAuth:
    """HTTP Digest auth credentials per RFC 7616 (MD5 + SHA-256) and
    RFC 2617 (legacy MD5). Frozen.

    Password held in Rust memory and never re-exposed to Python.
    `realm` is optional; same server-side-config semantics as
    `BasicAuth.realm`.

    As a client credential it never falls back to Basic: a server that
    offers no Digest challenge fails the request with an auth error and
    the password is never sent in Basic form.
    """

    def __new__(cls,
        user: str,
        password: str,
        algorithm: DigestAlgorithm = ...,
        realm: str | None = None,
    ) -> DigestAuth: ...
    @property
    def user(self) -> str: ...
    @property
    def algorithm(self) -> DigestAlgorithm: ...
    @property
    def realm(self) -> str | None: ...
    def __repr__(self) -> str: ...


@final
class RtspClientConfig:
    """RTSP client connection configuration. Frozen PyClass dataclass.

    `auth` is one of `BasicAuth`, `DigestAuth`, or `None`.
    `transport_pref` controls UDP/TCP selection at SETUP and `rtsp_version` the
    request-line version; a non-default value overrides the URL's
    `?transport=` / `?rtsp_version=`, the defaults leave the URL in charge.
    `tls_root_certs_pem` is a PEM bundle of trust anchors for
    `rtsps://` connections (private-CA / self-signed camera certs);
    platform native trust roots are used when it is `None`.
    """

    def __new__(cls,
        url: str,
        *,
        auth: Optional[Union[BasicAuth, DigestAuth]] = ...,
        transport_pref: TransportPref = ...,
        tls_root_certs_pem: Optional[bytes] = ...,
        keepalive: bool = ...,
        rtsp_version: RtspVersion = ...,
    ) -> RtspClientConfig: ...
    @property
    def url(self) -> str: ...
    @property
    def auth(self) -> Optional[Union[BasicAuth, DigestAuth]]: ...
    @property
    def transport_pref(self) -> TransportPref: ...
    @property
    def tls_root_certs_pem(self) -> Optional[bytes]: ...
    @property
    def keepalive(self) -> bool: ...
    @property
    def rtsp_version(self) -> RtspVersion: ...
    def __repr__(self) -> str: ...


@final
class RtspStats:
    """RTSP session stats snapshot. Reserved — all fields always report
    zero until RTCP session stats land; see
    docs/project/deferred-features.md. Frozen.
    """

    @property
    def rr_packets_received(self) -> int: ...
    @property
    def sr_packets_received(self) -> int: ...
    @property
    def interarrival_jitter_us(self) -> int: ...
    @property
    def fraction_lost_q8(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class RtspCancelHandle:
    """RTSP control-plane cancel handle. Frozen. Flipping `.cancel()`
    breaks any in-flight `connect` / `pause` / `play` / `teardown` out
    of blocking I/O at the next poll (typically <100 ms). The interrupted
    call raises `RtspError(CLOSED)` — the one cancel outcome every tstrans
    handle shares.
    """

    def cancel(self) -> None: ...
    def is_cancelled(self) -> bool: ...
    def __repr__(self) -> str: ...


@final
class RtspClient:
    """Static facade. `connect(config)` runs OPTIONS / DESCRIBE / SETUP
    / PLAY and returns a live `RtspSession` for MPEG-TS streams.
    `connect_h264(config)` runs the same flow but targets an H.264
    media line and stashes the negotiated `H264DepayConfig` in the
    returned session for use with `into_h264_receiver()`.
    """

    @staticmethod
    def connect(config: RtspClientConfig) -> RtspSession: ...
    @staticmethod
    def connect_h264(config: RtspClientConfig) -> RtspSession: ...


@final
class RtspSession:
    """Live RTSP session — server is in PLAY state. Methods drive
    `pause` / `play` / `teardown` and expose RTCP-derived stats.

    `into_demux_receiver()` — consume the data plane for MPEG-TS demuxing
    (created by `RtspClient.connect()`).
    `into_h264_receiver()` — consume the data plane for H.264 AU iteration
    (created by `RtspClient.connect_h264()`).
    The data plane is taken once: any `into_*` call after it has been
    consumed (by either method) raises `RtspError(CLOSED)`. Both directions
    are guarded — calling the wrong method on a fresh session raises
    `RtspError(PROTOCOL)` and leaves the data plane in place.
    """

    def pause(self) -> None: ...
    def play(self) -> None: ...
    def teardown(self) -> None: ...
    def cancel_handle(self) -> RtspCancelHandle: ...
    def stats(self) -> RtspStats: ...
    def into_demux_receiver(
        self,
        demux_config: Optional[DemuxerConfig] = ...,
    ) -> DemuxReceiver:
        """Consume the session's data plane into a `DemuxReceiver`; a
        rejected config raises and leaves the source unconsumed."""
        ...
    def into_h264_receiver(self) -> H264Receiver: ...
    def is_torn_down(self) -> bool: ...
    def __enter__(self) -> RtspSession: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...


# ---------------------------------------------------------------------------
# RTSP server (RtspServer + MountHandle + stats)
# ---------------------------------------------------------------------------


@final
class RtspServerCancelHandle:
    """Cross-thread hard-cancel handle for an `RtspServer`. Frozen.

    Calling `.cancel()` aborts every in-flight session at its next poll
    boundary, bypassing the graceful Notice-5402 path.
    """

    def cancel(self) -> None: ...
    def is_cancelled(self) -> bool: ...
    def __repr__(self) -> str: ...


# BEGIN GENERATED by print-records (ServerStats); do not edit
@final
class ServerStats:
    @property
    def active_sessions(self) -> int: ...
    @property
    def total_rtp_packets_sent(self) -> int: ...
    @property
    def total_rtp_bytes_sent(self) -> int: ...
    @property
    def mounts(self) -> int: ...
    @property
    def active_publishers(self) -> int: ...
    @property
    def total_rtp_packets_received(self) -> int: ...
    @property
    def total_rtp_bytes_received(self) -> int: ...
# END GENERATED by print-records (ServerStats)


# BEGIN GENERATED by print-records (MountStats); do not edit
@final
class MountStats:
    @property
    def bytes_pushed(self) -> int: ...
    @property
    def packets_pushed(self) -> int: ...
    @property
    def peer_count(self) -> int: ...
    @property
    def frames_dropped_total(self) -> int: ...
# END GENERATED by print-records (MountStats)


@final
class MountHandle:
    """Public mount surface returned by `RtspServer.add_unicast_mount` /
    `add_multicast_mount`. Cloneable (Arc-shared); multiple holders can
    push from different threads.

    All `push_*` methods release the GIL via `py.allow_threads()` so
    concurrent Python threads can run while the muxer/fanout work
    proceeds. `pts` is keyword-only.
    """

    # Identity / introspection.
    def mount_path(self) -> str: ...
    def peer_count(self) -> int: ...
    def mount_kind(self) -> Literal["unicast", "multicast", "unknown"]: ...
    def stats(self) -> MountStats: ...

    # Push surface — single stream (lone configured stream of each kind).
    def push_video(
        self,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        key_frame: bool = ...,
    ) -> None: ...
    def push_klv(
        self,
        klv: _BytesLike,
        *,
        pts: Pts90khz,
        metadata_service_id: int = ...,
    ) -> None: ...
    def push_audio(self, frames: _BytesLike, *, pts: Pts90khz) -> None: ...
    def push_subtitle(self, payload: _BytesLike, *, pts: Pts90khz) -> None: ...
    def push_data(self, data: _BytesLike, *, pts: Pts90khz) -> None: ...

    # Push surface — multi-stream (explicit handle).
    def push_video_to(
        self,
        handle: VideoStreamHandle,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        key_frame: bool = ...,
    ) -> None: ...
    def push_klv_to(
        self,
        handle: KlvStreamHandle,
        klv: _BytesLike,
        *,
        pts: Pts90khz,
        metadata_service_id: int = ...,
    ) -> None: ...
    def push_audio_to(
        self,
        handle: AudioStreamHandle,
        frames: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...
    def push_subtitle_to(
        self,
        handle: SubtitleStreamHandle,
        payload: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...
    def push_data_to(
        self,
        handle: DataStreamHandle,
        data: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...

    # Stream-handle accessors (first-of-kind + all-of-kind).
    def video_handle(self) -> Optional[VideoStreamHandle]: ...
    def klv_handle(self) -> Optional[KlvStreamHandle]: ...
    def audio_handle(self) -> Optional[AudioStreamHandle]: ...
    def subtitle_handle(self) -> Optional[SubtitleStreamHandle]: ...
    def data_handle(self) -> Optional[DataStreamHandle]: ...
    def video_handles(self) -> List[VideoStreamHandle]: ...
    def klv_handles(self) -> List[KlvStreamHandle]: ...
    def audio_handles(self) -> List[AudioStreamHandle]: ...
    def subtitle_handles(self) -> List[SubtitleStreamHandle]: ...
    def data_handles(self) -> List[DataStreamHandle]: ...

    # Lifecycle.
    def flush(self) -> None: ...
    def reset_stats(self) -> None: ...
    def __repr__(self) -> str: ...


class RefuseStatus(enum.IntEnum):
    """The status line a `MountPolicy.refuse` answers with. Pure-Python
    IntEnum."""

    FORBIDDEN = 403
    NOT_FOUND = 404
    NOT_ENOUGH_BANDWIDTH = 453
    SERVICE_UNAVAILABLE = 503


class AuthScheme(enum.IntEnum):
    """The scheme `MountRules.set_challenge` offers. Pure-Python IntEnum."""

    BASIC = 0
    DIGEST_MD5 = 1
    DIGEST_SHA256 = 2


@final
class MountCredential:
    """One username and password a client may present. The password cannot
    be read back and `repr` redacts it."""

    def __new__(cls, username: str, password: str) -> MountCredential: ...
    @property
    def username(self) -> str: ...


@final
class MountPolicy:
    """What the server does with one direction of traffic on a mount path."""

    @staticmethod
    def inherit() -> MountPolicy: ...
    @staticmethod
    def open() -> MountPolicy: ...
    @staticmethod
    def require(*credentials: MountCredential) -> MountPolicy: ...
    @staticmethod
    def refuse(status: Union[RefuseStatus, int]) -> MountPolicy: ...


@final
class MountRule:
    """The publish and read policies for one mount path (or the default). A
    direction left out inherits: it follows the server-wide `auth` setting
    (open when there is none), not the table's default rule."""

    def __new__(
        cls,
        publish: Optional[MountPolicy] = ...,
        read: Optional[MountPolicy] = ...,
    ) -> MountRule: ...


@final
class MountRules:
    """One server's per-mount-path rules, from `RtspServer.mount_rules()`."""

    def set(self, path: str, rule: MountRule) -> None: ...
    def remove(self, path: str) -> bool: ...
    def set_default(self, rule: MountRule) -> None: ...
    def clear(self) -> None: ...
    def set_challenge(self, scheme: Union[AuthScheme, int], realm: str) -> None: ...
    def is_closed(self) -> bool: ...
    def __len__(self) -> int: ...


@dataclass(frozen=True)
class RtspServerConfig:
    """Pure-Python frozen dataclass — configuration for `RtspServer.start`.

    `auth` accepts the same `BasicAuth` / `DigestAuth` PyClass instances
    that the client side uses.

    `tls_cert` / `tls_key` are PEM certificate-chain / private-key *file
    paths* for an `rtsps://` bind (set together). Missing/unreadable
    paths raise `RtspError(TLS)` at `start()`.
    """

    bind_addr: str = ...
    auth: Optional[Union[BasicAuth, DigestAuth]] = ...
    max_sessions: int = ...
    session_timeout_secs: int = ...
    fanout_capacity: int = ...
    graceful_shutdown_drain_ms: int = ...
    tls_cert: Optional[str] = ...
    tls_key: Optional[str] = ...
    accept_unregistered_publishers: bool = ...
    default_mount_rule: Optional[MountRule] = ...

    def __post_init__(self) -> None: ...


class PublishShape(enum.IntEnum):
    """Wire shape a publisher's ANNOUNCE declared. Pure-Python IntEnum."""

    MP2T = 0
    ELEMENTARY = 1


class ClockAlignment(enum.IntEnum):
    """How a publish mount aligns its announced tracks to one clock.
    Pure-Python IntEnum."""

    NOT_APPLICABLE = 0
    PENDING = 1
    PROVISIONAL = 2
    SENDER_REPORT = 3


@final
class PublisherInfo:
    """Frozen snapshot of the publisher holding a publish mount."""

    @property
    def peer(self) -> str: ...
    @property
    def shape(self) -> PublishShape: ...
    @property
    def klv(self) -> bool: ...
    @property
    def since_unix_ms(self) -> int: ...
    @property
    def generation(self) -> int: ...
    def __repr__(self) -> str: ...


# BEGIN GENERATED by print-records (PublishMountStats); do not edit
@final
class PublishMountStats:
    @property
    def rtp_packets_received(self) -> int: ...
    @property
    def bytes_received(self) -> int: ...
    @property
    def malformed_packets(self) -> int: ...
    @property
    def source_rejected(self) -> int: ...
    @property
    def frames_emitted(self) -> int: ...
    @property
    def frames_dropped_app(self) -> int: ...
    @property
    def frames_dropped_readers(self) -> int: ...
    @property
    def aus_emitted(self) -> int: ...
    @property
    def aus_dropped(self) -> int: ...
    @property
    def aus_reordered(self) -> int: ...
    @property
    def klv_units_emitted(self) -> int: ...
    @property
    def klv_units_dropped(self) -> int: ...
    @property
    def alignment(self) -> ClockAlignment: ...
    @property
    def alignment_steps(self) -> int: ...
    @property
    def ssrc_changes(self) -> int: ...
    @property
    def generation(self) -> int: ...
    @property
    def peer_count(self) -> int: ...
# END GENERATED by print-records (PublishMountStats)


@final
class PublishMount:
    """A publish mount returned by `RtspServer.add_publish_mount` or
    `RtspServer.next_publisher`. Frozen; every handle to one mount shares
    its state.

    `into_demux_receiver` is take-once across every handle to the mount
    (a second call raises `RtspError(CLOSED)`). The receiver stops
    iterating after `RtspServer.remove_mount` / `RtspServer.stop()`;
    after `cancel()` its next read raises `RtpError(CLOSED)`.
    """

    def mount_path(self) -> str: ...
    def peer_count(self) -> int: ...
    def generation(self) -> int: ...
    def publisher(self) -> Optional[PublisherInfo]: ...
    def stats(self) -> PublishMountStats: ...
    def cancel(self) -> None: ...
    def into_demux_receiver(
        self,
        demux_config: Optional[DemuxerConfig] = ...,
    ) -> DemuxReceiver: ...
    def __repr__(self) -> str: ...


@final
class RtspServer:
    """Sync RTSP server. Construct via `RtspServer.start(config)`.

    The underlying `tst_rtp::RtspServer` owns a tokio Runtime that lives
    for the server's lifetime; `__exit__` (or explicit `.stop()`) sends
    an RFC 7826 §13.5.1 Notice 5402 ("Server-Initiated TEARDOWN") to
    each active session before closing.
    """

    @staticmethod
    def start(config: RtspServerConfig) -> RtspServer: ...
    def add_unicast_mount(
        self,
        path: str,
        program_config: MuxerProgramConfig,
    ) -> MountHandle: ...
    def add_multicast_mount(
        self,
        path: str,
        group: str,
        port: int,
        *,
        ttl: int = ...,
        iface: Optional[str] = ...,
        program_config: MuxerProgramConfig,
    ) -> MountHandle: ...
    def add_publish_mount(self, path: str) -> PublishMount: ...
    def mount_rules(self) -> MountRules: ...
    # Releases the GIL; `None` timeout waits until a mount arrives or the
    # server stops (Ctrl-C interrupts). Concurrent callers are served one
    # at a time. Raises RtspError(SERVER) once the server has stopped.
    def next_publisher(
        self, timeout: Optional[float] = ...
    ) -> Optional[PublishMount]: ...
    def remove_mount(self, path: str) -> None: ...
    def stats(self) -> ServerStats: ...
    def local_addr(self) -> Optional[str]: ...
    def stop(self, *, drain_ms: int = ...) -> None: ...
    def cancel_handle(self) -> RtspServerCancelHandle: ...
    def __enter__(self) -> RtspServer: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]] = ...,
        exc_value: Optional[BaseException] = ...,
        traceback: Optional[Any] = ...,
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# RFC 6184 — H.264 depacketizer types + H264Receiver
# ---------------------------------------------------------------------------


@final
class ParameterSetInjection:
    """Controls whether out-of-band SPS/PPS are injected before IDR frames
    (IntEnum-shaped PyClass). Mirrors `tst_rtp::ParameterSetInjection`.

    `NONE`       — pass NALUs through exactly as received.
    `BEFORE_IDR` — inject cached SPS/PPS before every IDR frame (default).
    """

    NONE: ParameterSetInjection
    BEFORE_IDR: ParameterSetInjection


@final
class H264DepayConfig:
    """H.264 depacketizer configuration. Frozen.

    All defaults mirror `tst_rtp::H264DepayConfig::default()` (payload_type=96,
    parameter_set_injection=BEFORE_IDR, initial_parameter_sets=[], max_au_bytes=8388608).

    `initial_parameter_sets` accepts a list of raw NALU bytes (type 7 = SPS,
    type 8 = PPS). Last SPS and PPS each win when multiple are provided.
    """

    def __new__(cls,
        *,
        payload_type: Optional[int] = None,
        parameter_set_injection: Optional[ParameterSetInjection] = None,
        initial_parameter_sets: Optional[List[bytes]] = None,
        max_au_bytes: Optional[int] = None,
    ) -> H264DepayConfig: ...
    @property
    def payload_type(self) -> int: ...
    @property
    def parameter_set_injection(self) -> ParameterSetInjection: ...
    @property
    def initial_parameter_sets(self) -> List[bytes]: ...
    @property
    def max_au_bytes(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class H264AccessUnit:
    """A fully reassembled H.264 Access Unit. Frozen.

    `annexb`        — Annex B–framed NALU bytes (start codes prepended).
    `pts`           — 90 kHz decode-order timestamp as `int` (i64 ticks).
    `key_frame`     — `True` if the AU contains an IDR slice (NALU type 5).
    `rtp_timestamp` — raw 32-bit RTP timestamp from the packet header.
    """

    annexb: bytes
    pts: int
    key_frame: bool
    rtp_timestamp: int

    def __repr__(self) -> str: ...


# BEGIN GENERATED by print-records (H264DepayStats); do not edit
@final
class H264DepayStats:
    @property
    def aus_emitted(self) -> int: ...
    @property
    def aus_dropped(self) -> int: ...
    @property
    def aus_dropped_oversize(self) -> int: ...
    @property
    def packets_discarded(self) -> int: ...
    @property
    def nalus_discarded(self) -> int: ...
    @property
    def seq_gaps(self) -> int: ...
    @property
    def duplicate_packets(self) -> int: ...
    @property
    def parameter_set_updates(self) -> int: ...
    @property
    def ssrc_changes(self) -> int: ...
# END GENERATED by print-records (H264DepayStats)


# BEGIN GENERATED by print-records (RtpStats); do not edit
@final
class RtpStats:
    @property
    def malformed_packets(self) -> int: ...
    @property
    def source_rejected(self) -> int: ...
# END GENERATED by print-records (RtpStats)


@final
class H264Receiver:
    """Blocking H.264-over-RTP receiver (RFC 6184).

    `listen(url, config=None)` — bind to `rtp://host:port?pt=N` and return
    a ready receiver. `?pt=` is required (1..=127; 33 is rejected).

    `recv_au(timeout_ms=None)` — block until an Access Unit is reassembled
    (releasing the GIL via `py.allow_threads()`). Returns `H264AccessUnit`
    or `None` at EOS. `timeout_ms=N` bounds a single call; expiry raises
    `RtpError(BACKPRESSURE)` — the receiver stays open, call again to retry.

    Acts as its own iterator: `for au in receiver: ...` yields AUs until EOS.

    `depay_stats()` / `rtp_stats()` / `socket_stats()` — snapshot counters.
    `local_addr()` — UDP bind address as `"host:port"`. `None` only for a
    live TCP-interleaved (RTSP) receiver where no UDP socket exists;
    raises `RtpError(CLOSED)` if the receiver is closed.
    `cancel_handle()` — returns a `CancelHandle` for cross-thread cancellation.
    `end_reason()` / `end_detail()` — why the session ended
    (`EndReason` / free-text detail), or `None` for both while the
    session is still live. Readable after `close()`.
    `close()` — idempotent; fires cancel + drops the socket.
    """

    @staticmethod
    def listen(
        url: str,
        config: Optional[H264DepayConfig] = ...,
    ) -> H264Receiver: ...
    def recv_au(self, timeout_ms: Optional[int] = ...) -> Optional[H264AccessUnit]: ...
    def __iter__(self) -> Iterator[H264AccessUnit]: ...
    def __next__(self) -> H264AccessUnit: ...
    def depay_stats(self) -> H264DepayStats: ...
    def rtp_stats(self) -> RtpStats: ...
    def socket_stats(self) -> SocketStats: ...
    def local_addr(self) -> Optional[str]: ...
    def cancel_handle(self) -> CancelHandle: ...
    def end_reason(self) -> Optional[StreamEndReason]: ...
    def end_detail(self) -> Optional[str]: ...
    def close(self) -> None: ...
    def __enter__(self) -> H264Receiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# @overload sanity — RtspClientConfig accepts BasicAuth | DigestAuth | None.
# A single typed parameter handles all three forms; explicit overloads
# would be redundant given the Union annotation.
# ---------------------------------------------------------------------------
