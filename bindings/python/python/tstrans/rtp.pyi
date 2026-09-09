"""Type stubs for `tstrans.rtp` — RTP + RTSP bindings.

Mirrors the Wave A / Wave B public surface exported from
`bindings/python/python/tstrans/rtp.py`. Continues `py.typed` discipline.
mypy --strict clean.

The PyClass-backed types (Sender, Receiver, RtspClient, RtspSession,
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
# happens in Rust via a two-path fast/fallback pattern (audit #10).
_BytesLike = Union[bytes, bytearray, memoryview, Any]

__all__: list[str] = [
    "StreamEndReason",
    "Sender",
    "Receiver",
    "SocketStats",
    "CancelHandle",
    "MuxSender",
    "DemuxReceiver",
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
    "ParameterSetInjection",
    "H264DepayConfig",
    "H264AccessUnit",
    "H264DepayStats",
    "RtpStats",
    "H264Receiver",
]

# ---------------------------------------------------------------------------
# T20 — RTP transport (Sender / Receiver / SocketStats / CancelHandle)
# ---------------------------------------------------------------------------


class StreamEndReason(enum.IntEnum):
    """Why an RTP receive session ended. Mirrors `tst_rtp::StreamEndReason`.

    Returned by `.end_reason()` on `Receiver` / `DemuxReceiver` /
    `H264Receiver`; `None` means the session hasn't ended yet (or ended
    through a path this arc doesn't instrument).
    """

    CLEAN_TEARDOWN = 1
    SESSION_EXPIRED = 2
    KEEPALIVE_FAILED = 3
    TRANSPORT_FAILED = 4
    PROTOCOL_ERROR = 5
    CANCELLED = 6


@final
class SocketStats:
    """Frozen wire-level statistics snapshot. Mirror of
    `tst_core::transport::SocketStats`. All fields are integer-valued.

    `RtpTransport` populates `bytes_sent` / `packets_sent` only in
    Phase 1; `RtpRecvTransport` populates the receive-side counters.
    RTCP-derived fields (`rtt_us`, `packets_lost_*`) stay zero until
    RTCP RR/SR ingest is wired through the receiver.
    """

    rtt_us: int
    send_bandwidth_bps: int
    recv_bandwidth_bps: int
    link_bandwidth_bps: int
    bytes_sent: int
    packets_sent: int
    bytes_received: int
    packets_received: int
    bytes_lost_recv: int
    packets_lost_recv: int
    packets_lost_send: int
    packets_retransmitted: int
    packets_dropped_send: int
    packets_dropped_recv: int
    send_buffer_packets: int
    recv_buffer_packets: int

    def __repr__(self) -> str: ...


@final
class CancelHandle:
    """Transport-side cancel handle for `Sender` / `Receiver`. Calling
    `.cancel()` wakes a thread parked in `.send()` / `.recv()` within
    ~100 ms; that call returns `RtpError(kind=CANCELLED)`.
    """

    def cancel(self) -> None: ...
    def __repr__(self) -> str: ...


@final
class Sender:
    """Python RTP sender wrapping `tst_rtp::RtpTransport`.

    `pkt_size` overrides the UDP datagram size (RTP header + TS payload).
    `ssrc` pins the RTP synchronization source identifier; when omitted
    the transport picks a random one.
    """

    def __new__(cls,
        url: str,
        *,
        pkt_size: int = ...,
        ssrc: Optional[int] = ...,
    ) -> Sender: ...
    def send(self, ts_bytes: _BytesLike) -> None: ...
    def stats(self) -> SocketStats: ...
    def cancel_handle(self) -> CancelHandle: ...
    def close(self) -> None: ...
    def __enter__(self) -> Sender: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class Receiver:
    """Python RTP receiver wrapping `tst_rtp::RtpRecvTransport`.

    Binds to `url` (literal IP:port). For multicast URLs, joins the
    group automatically. The receive buffer sizes itself to the
    transport's deliverable ceiling; ``?pkt_size=`` on a receiver URL
    is rejected.

    `end_reason()` / `end_detail()` report why the receive session
    ended (`StreamEndReason` / free-text detail), or `None` for both
    while the session is still live. Readable after `close()`.
    """

    def __new__(cls, url: str) -> Receiver: ...
    def recv(self, timeout_ms: Optional[int] = ...) -> bytes: ...
    def stats(self) -> SocketStats: ...
    def cancel_handle(self) -> CancelHandle: ...
    def end_reason(self) -> Optional[StreamEndReason]: ...
    def end_detail(self) -> Optional[str]: ...
    def close(self) -> None: ...
    def __enter__(self) -> Receiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T21 — RTSP client (enums, auth dataclasses, config, stats, session)
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
    `transport_pref` controls UDP/TCP selection at SETUP.
    `tls_root_certs_pem` is a PEM bundle of trust anchors for
    `rtsps://` connections (private-CA / self-signed camera certs);
    platform native trust roots are used when it is `None`.
    """

    def __new__(cls,
        url: str,
        *,
        auth: Optional[Union[BasicAuth, DigestAuth]] = ...,
        transport_pref: TransportPref = ...,
        rtcp: bool = ...,
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
    def rtcp(self) -> bool: ...
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
    def rr_packets_sent(self) -> int: ...
    @property
    def sr_packets_sent(self) -> int: ...
    @property
    def interarrival_jitter_us(self) -> int: ...
    @property
    def fraction_lost_q8(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class RtspCancelHandle:
    """RTSP control-plane cancel handle. Frozen. Flipping `.cancel()`
    breaks any in-flight `connect` / `pause` / `play` / `teardown` out
    of blocking I/O at the next poll (typically <100 ms).
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
    (created by `RtspClient.connect()`). Raises `RtspError(PROTOCOL)` when
    called on an H.264 session (created by `RtspClient.connect_h264()`) or
    when the data plane has already been consumed.
    `into_h264_receiver()` — consume the data plane for H.264 AU iteration
    (created by `RtspClient.connect_h264()`). Raises `RtspError(PROTOCOL)`
    when called on an MPEG-TS session (created by `RtspClient.connect()`)
    or when the data plane has already been consumed.
    Both directions are guarded — calling the wrong method for the session
    type raises `RtspError(PROTOCOL)`.
    """

    def pause(self) -> None: ...
    def play(self) -> None: ...
    def teardown(self) -> None: ...
    def cancel_handle(self) -> RtspCancelHandle: ...
    def stats(self) -> RtspStats: ...
    def into_demux_receiver(
        self,
        demux_config: Optional[DemuxerConfig] = ...,
    ) -> DemuxReceiver: ...
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
# T22 — RTSP server (RtspServer + MountHandle + stats)
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


@final
class ServerStats:
    """Frozen snapshot of aggregate `RtspServer` stats."""

    @property
    def active_sessions(self) -> int: ...
    @property
    def total_rtp_packets_sent(self) -> int: ...
    @property
    def total_rtp_bytes_sent(self) -> int: ...
    @property
    def mounts(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class MountStats:
    """Frozen snapshot of per-mount stats."""

    @property
    def bytes_pushed(self) -> int: ...
    @property
    def packets_pushed(self) -> int: ...
    @property
    def peer_count(self) -> int: ...
    @property
    def frames_dropped_total(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class MountHandle:
    """Public mount surface returned by `RtspServer.add_unicast_mount` /
    `add_multicast_mount`. Cloneable (Arc-shared); multiple holders can
    push from different threads.

    All `push_*` methods release the GIL via `py.allow_threads()` so
    concurrent Python threads can run while the muxer/fanout work
    proceeds. `pts` is keyword-only per Wave C normalization.
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


@dataclass(frozen=True)
class RtspServerConfig:
    """Pure-Python frozen dataclass — configuration for `RtspServer.start`.

    `auth` accepts the same `BasicAuth` / `DigestAuth` PyClass instances
    that T21 introduced for the client side.

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

    def __post_init__(self) -> None: ...


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
# T23 — MuxSender + DemuxReceiver convenience wrappers (Wave B).
#
# The Rust implementation is being authored in a parallel worktree; the
# stub here mirrors the surface declared in the plan
# (`docs/plans/2026-05-26-tst-rtp-phase-4-binding-exposure.md`,
# Task 23 lines 2456-2470). The merge phase will reconcile if the
# implementation diverges.
# ---------------------------------------------------------------------------


@final
class MuxSender:
    """Convenience wrapper — `Sender` + `Muxer` constructed together.

    Single-call constructor; push methods accept any bytes-like input
    (`bytes`, `bytearray`, `memoryview`, NumPy `uint8`). `pts` is
    keyword-only on every push method. The GIL is released around the
    muxer + transport work via `py.allow_threads()`.
    """

    def __new__(cls,
        url: str,
        program_config: MuxerProgramConfig,
        *,
        pkt_size: int = ...,
    ) -> MuxSender: ...

    # Send surface — single stream.
    def send_video(
        self,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        key_frame: bool = ...,
    ) -> None: ...
    def send_klv(
        self,
        klv: _BytesLike,
        *,
        pts: Pts90khz,
        metadata_service_id: int = ...,
    ) -> None: ...
    def send_audio(self, adts: _BytesLike, *, pts: Pts90khz) -> None: ...
    def send_subtitle(self, payload: _BytesLike, *, pts: Pts90khz) -> None: ...
    def send_data(self, data: _BytesLike, *, pts: Pts90khz) -> None: ...

    # Send surface — multi-stream variants.
    def send_video_to(
        self,
        handle: VideoStreamHandle,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        key_frame: bool = ...,
    ) -> None: ...
    def send_klv_to(
        self,
        handle: KlvStreamHandle,
        klv: _BytesLike,
        *,
        pts: Pts90khz,
        metadata_service_id: int = ...,
    ) -> None: ...
    def send_audio_to(
        self,
        handle: AudioStreamHandle,
        adts: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...
    def send_subtitle_to(
        self,
        handle: SubtitleStreamHandle,
        payload: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...
    def send_data_to(
        self,
        handle: DataStreamHandle,
        data: _BytesLike,
        *,
        pts: Pts90khz,
    ) -> None: ...

    # Stream handle accessors.
    def video_handle(self) -> Optional[VideoStreamHandle]: ...
    def klv_handle(self) -> Optional[KlvStreamHandle]: ...
    def audio_handle(self) -> Optional[AudioStreamHandle]: ...
    def subtitle_handle(self) -> Optional[SubtitleStreamHandle]: ...
    def data_handle(self) -> Optional[DataStreamHandle]: ...

    def stats(self) -> Tuple[SocketStats, MuxerStats]: ...
    def close(self) -> None: ...
    def __enter__(self) -> MuxSender: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class DemuxReceiver:
    """Convenience wrapper — `Receiver` + `Demuxer` constructed together.

    Iterating yields `tstrans.mpegts.DemuxEvent` subclass instances —
    no duplicated event type hierarchy. `__next__` blocks (releases the
    GIL) on the underlying transport read.

    `stats()` returns a tuple `(SocketStats, Any)` — the second
    element is the demuxer-side stats snapshot whose concrete type is
    pending T23's final pick (no `DemuxerStats` PyClass exists in
    `tstrans.mpegts` yet).

    `end_reason()` / `end_detail()` report why the receive session
    ended (`StreamEndReason` / free-text detail), or `None` for both
    while the session is still live. Readable after `close()`.
    """

    def __new__(cls,
        url: str,
        *,
        demux_config: Optional[DemuxerConfig] = ...,
    ) -> DemuxReceiver: ...
    def __iter__(self) -> Iterator[DemuxEvent]: ...
    def __next__(self) -> DemuxEvent: ...
    # Register a fan-out callback invoked with every 188-byte TS packet
    # (as `bytes`) BEFORE demuxing, in registration order. If the
    # callback raises, the exception is re-raised fail-loud from the next
    # `__next__` and iteration stops. Append-only for the receiver's life.
    def add_byte_sink(self, callback: Callable[[bytes], None]) -> None: ...
    def stats(self) -> Tuple[SocketStats, Any]: ...
    def last_seen_micros(self, pid: int) -> Optional[int]: ...
    def end_reason(self) -> Optional[StreamEndReason]: ...
    def end_detail(self) -> Optional[str]: ...
    def close(self) -> None: ...
    def __enter__(self) -> DemuxReceiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# WP-3 RFC 6184 — H.264 depacketizer types + H264Receiver
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


@final
class H264DepayStats:
    """RFC 6184 depacketizer counters. Frozen snapshot.

    All 9 counters are `int`; zero until activity occurs.
    """

    aus_emitted: int
    aus_dropped: int
    aus_dropped_oversize: int
    packets_discarded: int
    nalus_discarded: int
    seq_gaps: int
    duplicate_packets: int
    parameter_set_updates: int
    ssrc_changes: int

    def __repr__(self) -> str: ...


@final
class RtpStats:
    """RTP protocol–level statistics snapshot. Frozen.

    `malformed_packets` — datagrams with an invalid RTP header, wrong payload
    type, or empty payload. Cumulative since `listen()`.
    """

    malformed_packets: int

    def __repr__(self) -> str: ...


@final
class H264Receiver:
    """Blocking H.264-over-RTP receiver (RFC 6184).

    `listen(url, config=None)` — bind to `rtp://host:port?pt=N` and return
    a ready receiver. `?pt=` is required (1..=127; 33 is rejected).

    `recv_au(timeout_ms=None)` — block until an Access Unit is reassembled
    (releasing the GIL via `py.allow_threads()`). Returns `H264AccessUnit`
    or `None` at EOS. `timeout_ms=N` bounds a single call; expiry raises
    `RtpError(TIMEOUT)` — the receiver stays open, call again to retry.

    Acts as its own iterator: `for au in receiver: ...` yields AUs until EOS.

    `depay_stats()` / `rtp_stats()` / `socket_stats()` — snapshot counters.
    `local_addr()` — UDP bind address as `"host:port"`. `None` only for a
    live TCP-interleaved (RTSP) receiver where no UDP socket exists;
    raises `RtpError(TRANSPORT)` if the receiver is closed.
    `cancel_handle()` — returns a `CancelHandle` for cross-thread cancellation.
    `end_reason()` / `end_detail()` — why the session ended
    (`StreamEndReason` / free-text detail), or `None` for both while the
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
