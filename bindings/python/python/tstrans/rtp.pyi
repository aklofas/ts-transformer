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
    Sequence,
    Tuple,
    Type,
    Union,
    final,
)

# Cross-module types are imported from the `tstrans.mpegts` stub, which
# now ships a sibling `.pyi` declaring these as real classes.
from tstrans.shells import (
    CancelHandle,
    DemuxReceiver,
    EndReason,
    MuxSender,
    Sender,
    SocketStats,
)
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
    "PushDescription",
    "RtspPushSession",
    "RtspPushStats",
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

    `pkt_size` overrides the UDP datagram size (RTP header + TS payload);
    a value above 65424 raises `RtpError`. `ssrc` pins the RTP
    synchronization source identifier; when omitted the transport picks a
    random one.
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
    def end_reason(self) -> Optional[EndReason]: ...
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
    and Basic otherwise; use `DigestAuth` to refuse Basic. As a client
    credential an empty `user` is no credentials: a challenge then raises
    `RtspError(AUTH_FAILED)`.
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
    the password is never sent in Basic form. As a client credential an
    empty `user` is no credentials: a challenge then raises
    `RtspError(AUTH_FAILED)`.
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
    """RTSP control-plane cancel handle. Frozen. `.cancel()` ends an
    in-flight `pause` / `play` / `teardown` on its session: a request
    waiting for its reply notices the cancel within 100 ms; a request
    whose bytes the server is not taking notices it only when its write
    completes, its deadline passes or 5 s without progress end the
    control connection. The interrupted call raises `RtspError(CLOSED)`
    — the one cancel outcome every tstrans handle shares. `RtspClient.connect` returns the session only after PLAY, so
    no handle reaches a connect in progress.
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
    @staticmethod
    def push(config: RtspClientConfig, description: PushDescription) -> RtspPushSession:
        """Connect to `config.url` and publish to it: ANNOUNCE
        `description`, SETUP every track (`mode=record`, interleaved on the
        control connection) and RECORD. Returns the recording session.

        Raises `RtspError` when any of the four steps fails; the connection
        is closed then (after a TEARDOWN once a session is open).
        `transport_pref=TransportPref.UDP`, or a URL carrying
        `?transport=udp`, raises `UNSUPPORTED_TRANSPORT` before any SETUP
        is written (publishing over UDP is not implemented), and
        `rtsp_version=V2_0` raises `PROTOCOL` before the ANNOUNCE is
        written (RTSP 2.0 has no ANNOUNCE). GIL released for the network
        exchange.

        `keepalive=True` (the default) sends keepalive pings at half the
        session timeout the server advertises; the config has no interval.
        The pings do not keep alive a push that goes silent for less than
        that: a server may end a publisher it reads nothing from sooner
        (mediamtx 1.21.4 after 10 s, while it advertises 55 s), so keep
        media flowing."""
        ...


@final
class RtspSession:
    """Live RTSP session — server is in PLAY state. Methods drive
    `pause` / `play` / `teardown` and expose RTCP-derived stats.

    `into_demux_receiver()` — consume the data plane for MPEG-TS demuxing
    (created by `RtspClient.connect()`).
    `into_h264_receiver()` — consume the data plane for H.264 AU iteration
    (created by `RtspClient.connect_h264()`).
    The data plane is taken once: any `into_*` call after it has been
    consumed (by either method), or once `close()` has begun, raises
    `RtspError(CLOSED)`; take it before closing. Both directions
    are guarded — calling the wrong method on a fresh session raises
    `RtspError(PROTOCOL)` and leaves the data plane in place.

    `close()` (and the `with` exit, which calls it) ends the session and
    its client; `teardown()` reports what the server answered and keeps
    the client until then. A session collected without `close()` writes a
    best-effort TEARDOWN with the GIL released (500 ms bounds the write;
    the reply is not awaited), then tells the client's thread to stop
    without waiting for it; that thread closes the connection on its own.

    A receiver never owns the client, so keep the session object alive
    while one receives: collecting it ends a live TCP-interleaved receiver
    with `RtpError(CLOSED)` after what is already queued, whatever the
    server then does. `teardown()` or `close()` ends it with end of stream
    once the server accepts the TEARDOWN, on a session whose cancel was
    not set; on a session whose cancel handle had fired it is a cancel,
    `RtpError(CLOSED)`, whatever the server then does. A TEARDOWN the
    server refuses leaves the receiver running until `close()`, which ends
    it with `RtpError(CLOSED)`, as it does when its own TEARDOWN is refused
    or not answered within its bound. A session that had already ended for
    its own reason keeps that reason and that outcome. One end reason is
    kept per session: use a new session for each stream. A UDP receiver
    has its own socket and is not ended by any of this.

    `cancel_handle()`, `is_torn_down()` and `stats()` answer at once from
    any thread, also while a control call is parked; `close()` cancels a
    control call in progress on another thread first (it raises
    `RtspError(CLOSED)`), then closes. The control calls (`pause()`,
    `play()`, `teardown()`) run one at a time: a second one waits, GIL
    released, until the call in progress returns or is cancelled (up to its
    request timeout).
    """

    def pause(self) -> None:
        """Send PAUSE. Raises `RtspError(CLOSED)` after `close()`."""
        ...
    def play(self) -> None:
        """Send PLAY (resume). Raises `RtspError(CLOSED)` after `close()`."""
        ...
    def teardown(self) -> None:
        """Send TEARDOWN. A 2xx reply, or `454 Session Not Found` (the
        session is already gone), returns `None`; a TEARDOWN the server
        refuses raises `RtspError` of the status's kind (`PROTOCOL`, or
        `NOT_FOUND` for a 404). A first 401 is answered once with the
        credentials, unless it carries no `WWW-Authenticate` header
        (`PROTOCOL`), the client has no credentials (`AUTH_FAILED`) or its
        challenge offers no scheme the client may answer (`AUTH_REQUIRED`);
        a 401 to that retry raises `AUTH_FAILED`. The session counts as torn down either way: a
        second call, or a call after `close()`, is a no-op. The reply is
        awaited up to the request timeout (10 s). The client, its thread
        and the control connection stay until `close()`."""
        ...
    def close(self) -> None:
        """End the session and its client: TEARDOWN with one 500 ms bound
        over writing it and its reply, then up to 2 s for the client's
        thread; the control connection is closed on return unless that
        wait ran out. Returns `None` whatever the server answered (call
        `teardown()` first for the answer). A call made while another
        `close()` is running returns at once without waiting for it; any
        later call is a no-op. A control call in progress on another
        thread is cancelled first, so the session then ends as a cancel (a
        live receiver ends `RtpError(CLOSED)`), not as a clean teardown.
        Afterwards `pause()`, `play()`, `cancel_handle()`,
        `into_demux_receiver()` and `into_h264_receiver()` raise
        `RtspError(CLOSED)` (take the data plane before closing),
        `teardown()` returns `None` and a cancel handle taken earlier reads
        `is_cancelled() == True`. The `with` exit calls this."""
        ...
    def cancel_handle(self) -> RtspCancelHandle:
        """A handle on the session's cancel; answers at once, also while
        another thread's call is parked, and after `teardown()`. Raises
        `RtspError(CLOSED)` once `close()` has begun."""
        ...
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
# Publishing to an RTSP server (RtspClient.push)
# ---------------------------------------------------------------------------


@final
class PushDescription:
    """What a push announces: one MPEG-TS track, or one video track (H.264
    or H.265) and, with `klv=True`, one KLV track (RFC 6597). Frozen; built
    by the static constructors. Parameter sets are NAL units with their
    header and without a start code."""

    @staticmethod
    def mp2t() -> PushDescription:
        """One MPEG-TS track (payload type 33): send with `send_ts`, or
        convert the session with `into_mux_sender` / `into_sender`."""
        ...
    @staticmethod
    def h264(parameter_sets: Sequence[_BytesLike], *, klv: bool = False) -> PushDescription:
        """An H.264 track: at least one SPS and one PPS (a NAL unit of
        another type is ignored), each any bytes-like object. A list
        lacking one is refused by `RtspClient.push` with
        `RtspError(PROTOCOL)`."""
        ...
    @staticmethod
    def h264_from_key_frame(annexb: _BytesLike, *, klv: bool = False) -> PushDescription:
        """An H.264 track described by every SPS and PPS of one access unit
        in Annex B form (a key frame as an encoder emits it). Raises
        `ValueError` when it carries no SPS or no PPS."""
        ...
    @staticmethod
    def h265(
        vps: _BytesLike, sps: _BytesLike, pps: _BytesLike, *, klv: bool = False
    ) -> PushDescription:
        """An H.265 track described by its VPS, SPS and PPS, each any
        bytes-like object."""
        ...
    @staticmethod
    def h265_from_key_frame(annexb: _BytesLike, *, klv: bool = False) -> PushDescription:
        """An H.265 track described by the first VPS, SPS and PPS of one
        access unit in Annex B form. Raises `ValueError` when it lacks one
        of the three."""
        ...


@final
class RtspPushSession:
    """A recording RTSP session: what `RtspClient.push` returns. Media goes
    to the server interleaved on the control connection.

    A data call (`send_ts`, `send_video`, `send_klv`) returns once its
    packets are queued, not once they are on the wire. The queue is
    bounded: against a server that reads slowly a data call waits, GIL
    released, until room frees, the cancel handle fires (within about
    100 ms), the session ends, or this machine's kernel has taken no byte
    of the connection for 5 s (the connection then ends). Data calls raise
    `RtpError`: `CLOSED` after a cancel or `close()`, `BROKEN` once the
    server closed or the connection failed, `TOO_LARGE` for a TS payload
    over 1316 bytes; an empty or not-whole TS payload is `BROKEN` and
    leaves the session open. A call naming a track the push did not
    announce (`send_klv` without `klv=True`, `send_video` / `send_klv` on
    an MPEG-TS push, `send_ts` on an elementary one) or an empty access
    unit or KLV unit raises `ValueError` and sends nothing. Video and KLV
    share one timeline: `pts` is the presentation time, and successive
    access units are in decoding order.

    Control calls (`pause`, `resume`, `teardown`) raise `RtspError`.
    `pause()` and `close()` first wait for the queued media to be written,
    with the same ends as a data call.

    The calls that use the push run one at a time: a second thread's call
    waits, GIL released, for the one in progress. `stats()`, `end_reason()`
    and `end_detail()` never wait: they read the push's counters and end
    without the session, and keep answering after `close()`, `teardown()`
    and a conversion. `is_session_alive()` never waits: a call in progress
    on another thread counts as live. `cancel_handle()` answers at once,
    also while a send is parked on another thread, and is the way to end a
    parked send.

    A `close()` or `teardown()` fires the cancel first only when a send,
    `pause()` or `resume()` on another thread holds the session: that call
    raises `CLOSED`, what is queued is dropped, and the session ends as a
    cancel. One that finds only `is_session_alive()` peeking at the
    session waits for it and closes cleanly. A send, `pause()` or
    `resume()` still waiting for the session is neither cancelled nor
    waited for: it gets the session after the close began, runs nothing
    and raises `CLOSED`. So does one that gets the session after a
    conversion.

    `close()` (and the `with` exit) ends the session: a last sender report
    per track, the queued media written, TEARDOWN with one 500 ms bound
    over writing it and its reply, then up to 2 s for the client's thread.
    `teardown()` does the same and raises what the TEARDOWN returned;
    unlike `RtspSession.teardown()`, the session is closed after it. After
    either, every call raises `CLOSED` (`RtpError` for the data calls,
    `RtspError` for the rest); `session_id`, `stats()`, `end_reason()` and
    `end_detail()` stay readable. A session
    collected without `close()` sets its cancel, discards what is queued
    and writes TEARDOWN without waiting for the reply. Interpreter exit
    cancels a call parked in a session left open.

    `into_mux_sender` / `into_sender` move an MPEG-TS push into a sender
    shell; the session is closed afterwards. They never wait for a call
    that may park: while a send, `pause()` or `resume()` on another thread
    holds the session they raise `RuntimeError`, consume nothing and leave
    the session open. One that finds only `is_session_alive()` peeking at
    the session waits for it and converts.
    The shell's `close()` cancels first, so it can drop
    packets queued a moment before. `MuxSender.finish()` cancels nothing:
    it writes what the sender holds and closes the push, which, while the
    session is up, writes its queue and then sends TEARDOWN (it cannot
    report whether that happened). A plain `Sender` has no such end: when
    you must know that everything was written, send with `send_ts` and end
    the session with `teardown()`.

    A panic inside a call (a bug) poisons the session: from then on every
    call that needs the session raises `RtspError(CLOSED)` naming the
    poisoning, `is_session_alive()` reads `False`, the reads above still
    answer, and `close()` / `teardown()` still end the push.
    """

    def send_ts(self, ts: bytes) -> None:
        """Send whole TS packets as one RTP packet on an MPEG-TS push: a
        multiple of 188 bytes, at most 1316, each starting with 0x47."""
        ...
    def send_video(self, annexb: bytes, *, pts: Pts90khz) -> None:
        """Send one video access unit in Annex B form. A cancel or the
        session's end during the call can leave it sent in part."""
        ...
    def send_klv(self, klv: bytes, *, pts: Pts90khz) -> None:
        """Send one KLV unit (a whole local or universal set, without a
        metadata access unit header) on a push announced with `klv=True`."""
        ...
    def pause(self) -> None:
        """Write the queued media, then send PAUSE. Raises
        `RtspError(PROTOCOL)` on a push already paused. Data calls still
        send while paused; some servers end the session on such a packet."""
        ...
    def resume(self) -> None:
        """Send RECORD again on a paused push. Raises `RtspError(PROTOCOL)`
        on a push that is not paused."""
        ...
    def teardown(self) -> None:
        """End the session as `close()` does and raise what it reports,
        in this order: `RtspError(CLOSED)` on a cancelled push (and on a
        session already closed); otherwise `PROTOCOL` with status 454 when
        queued media was lost to an expired session; otherwise the
        TEARDOWN's own result: `PROTOCOL` (or `NOT_FOUND` for a 404) when
        the server refused it, `TIMEOUT` when no reply came within the
        500 ms and `IO` when the connection had ended. Returns `None` only
        when the queued media was written and the server accepted the
        TEARDOWN. The session is closed after it, whatever the outcome."""
        ...
    def close(self) -> None:
        """End the session; returns `None` whatever the server answered.
        Idempotent: a later call, or one made while another `close()` runs,
        returns at once."""
        ...
    def cancel_handle(self) -> RtspCancelHandle:
        """A handle on the push's cancel: `cancel()` ends a waiting data
        call, `pause` or `close` from any thread, and the push sends nothing
        more. Raises `RtspError(CLOSED)` once the session is closed."""
        ...
    def stats(self) -> RtspPushStats:
        """The push's counters now. Never waits (a send parked on another
        thread does not hold it up) and never raises; after `close()`,
        `teardown()` or a conversion, the last values, a converted push's
        sends included. `frames_dropped_at_close` counts media a close
        discarded; `teardown()` is the call that reports such a loss."""
        ...
    def end_reason(self) -> Optional[EndReason]:
        """Why the push ended, or `None` while it has not: `CLEAN_TEARDOWN`
        once the server closed the connection or accepted the TEARDOWN,
        `CANCELLED` after a cancel. Never waits; kept after `close()` and
        after a conversion (it follows the sender that owns the push)."""
        ...
    def end_detail(self) -> Optional[str]:
        """Free-text detail for `end_reason()`, or `None`."""
        ...
    @property
    def session_id(self) -> str:
        """The session id the server named at SETUP."""
        ...
    def is_session_alive(self) -> bool:
        """False once the session is known dead (the control connection
        ended, or a keepalive ping was answered `454`), once it is closed
        or converted, and once a panic poisoned it. Never waits and never
        raises: a send,
        `pause()` or `resume()` in progress on another thread counts as
        live."""
        ...
    def into_mux_sender(self, program_config: MuxerProgramConfig) -> MuxSender:
        """Move an MPEG-TS push into a `MuxSender`; the session is closed
        afterwards. Raises `ValueError` on an elementary push,
        `MuxError(CONFIG_INVALID)` for a refused config and `RuntimeError`
        while a send, `pause()` or `resume()` on another thread holds the
        session, each leaving the session as it was; an
        `is_session_alive()` peeking at it is waited for. The sender's
        `close()` cancels first; `finish()` cancels nothing. `stats()` and
        `end_reason()` keep answering for the push the sender owns."""
        ...
    def into_sender(self) -> Sender:
        """Move an MPEG-TS push into a `Sender`; the session is closed
        afterwards. Raises `ValueError` on an elementary push and
        `RuntimeError` while a send, `pause()` or `resume()` on another
        thread holds the session, both leaving the session as it was; an
        `is_session_alive()` peeking at it is waited for. The sender's
        `close()` cancels first, and a `Sender` has no lossless end: for
        one, send with `send_ts` and call `teardown()`. `stats()` and
        `end_reason()` keep answering for the push the sender owns."""
        ...
    def __enter__(self) -> RtspPushSession: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...


# BEGIN GENERATED by print-records (RtspPushStats); do not edit
@final
class RtspPushStats:
    @property
    def packets_sent(self) -> int: ...
    @property
    def bytes_sent(self) -> int: ...
    @property
    def sender_reports_sent(self) -> int: ...
    @property
    def receiver_reports(self) -> int: ...
    @property
    def fraction_lost_q8(self) -> int: ...
    @property
    def cumulative_lost(self) -> int: ...
    @property
    def frames_dropped_inbound(self) -> int: ...
    @property
    def frames_dropped_at_close(self) -> int: ...
# END GENERATED by print-records (RtspPushStats)


# ---------------------------------------------------------------------------
# RTSP server (RtspServer + MountHandle + stats)
# ---------------------------------------------------------------------------


@final
class RtspServerCancelHandle:
    """Cross-thread hard-cancel handle for an `RtspServer`. Frozen.

    `.cancel()` sets a flag and returns. From then on mount pushes, mount
    registration and removal, rule edits and a new `next_publisher` raise
    `RtspError(CLOSED)` at once; a parked `next_publisher` raises it within
    100 ms; and within 100 ms the server notices the cancel and ends its
    sessions, its listener, its multicast senders and every publish mount's
    receiver (end of stream, as after `stop()`). No Notice 5402 is sent. A
    `stop()` after it ends what is left without the graceful path. A
    `cancel()` while a `stop()` (or the `with` exit) runs does not shorten
    that call: cancel before it for an end without the Notice.
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
    proceeds. `pts` is keyword-only. A push raises `RtspError(MOUNT)` when
    the muxer refuses the frame, and `RtspError(CLOSED)` once the server has
    ended (`stop()`, from the moment it begins; a `cancel()` on its
    `cancel_handle()`; or the server being collected), whether or not the
    mount was removed first.
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
    def push_video_with_dts(
        self,
        nal: _BytesLike,
        *,
        pts: Pts90khz,
        dts: Pts90khz,
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
    direction left out inherits: a path rule's `inherit()` takes the table's
    default rule for that direction; the default rule's `inherit()` is the
    server-wide `auth` setting (open when there is none)."""

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
    end_publish_mount_on_publisher_exit: bool = ...
    publish_idle_timeout_ms: Optional[int] = ...

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

    A server collected without `stop()` is stopped with the GIL released
    (the collecting thread waits the drain window).
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
    def add_publish_mount(
        self, path: str, *, end_on_publisher_exit: bool | None = ...
    ) -> PublishMount: ...
    def mount_rules(self) -> MountRules: ...
    # Releases the GIL; `None` timeout waits until a mount arrives or the
    # server stops (Ctrl-C interrupts). Concurrent callers are served one
    # at a time. Raises RtspError(CLOSED) once the server has stopped or
    # been hard-cancelled (a parked call within 100 ms of the cancel).
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
    `pts`           — 90 kHz **presentation** timestamp, delivered in decode order,
                    as `int` (i64 ticks).
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
    or `None` at EOS (the RTSP server ended the session, or accepted the
    TEARDOWN `teardown()` sent on a session whose cancel was not set; a
    failed control connection also gives `None`, and
    `end_reason()` names it). `timeout_ms=N`
    bounds a single call; expiry raises `RtpError(BACKPRESSURE)` — the
    receiver stays open, call again to retry. After a cancel, once its
    `RtspSession` is collected, or after a `teardown()` on a session whose
    cancel had fired, the AUs already
    buffered are returned first, then `RtpError(CLOSED)` is raised. A
    `close()` from another thread lets a parked call return one
    held AU if there is one (a queued AU, or the one the cancel flushes);
    the receiver is then empty, later calls raise `RtpError(CLOSED)` and
    any further queued AU is discarded.

    Acts as its own iterator: `for au in receiver: ...` yields AUs until EOS;
    a cancelled loop raises `RtpError(CLOSED)`.

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
    def end_reason(self) -> Optional[EndReason]: ...
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
