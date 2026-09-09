"""Type stubs for `tstrans.srt` — SRT transport bindings.

Mirrors the Wave A / Wave B / Wave C public surface exported from
`bindings/python/python/tstrans/srt.py`. Continues `py.typed` discipline.
mypy --strict clean.

The PyClass-backed types (Sender, Receiver, Socket, Listener, Builder,
MuxSender, DemuxReceiver, ReconnectPolicy, BackoffStrategy,
OverflowPolicy, ManagedSender, ManagedReceiver, ManagedMuxSender,
ManagedDemuxReceiver, SocketStats, SrtStats, CancelHandle) live in
`_native.srt` (Rust source under `bindings/python/src/srt/`). The
`SrtError` + `SrtErrorKind` re-exports come from `tstrans.exceptions`.
"""

from __future__ import annotations

from typing import (
    Any,
    Callable,
    Iterator,
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
    "Sender",
    "Receiver",
    "SocketStats",
    "SrtStats",
    "CancelHandle",
    "Builder",
    "Socket",
    "Listener",
    "MuxSender",
    "DemuxReceiver",
    "BackoffStrategy",
    "OverflowPolicy",
    "ReconnectPolicy",
    "ReconnectMode",
    "ManagedTransportStats",
    "ManagedSender",
    "ManagedReceiver",
    "ManagedMuxSender",
    "ManagedDemuxReceiver",
]

# ---------------------------------------------------------------------------
# T2 — transport types (Sender / Receiver / SocketStats / SrtStats /
# CancelHandle)
# ---------------------------------------------------------------------------


@final
class SocketStats:
    """Frozen wire-level statistics snapshot. Mirror of
    `tst_core::transport::SocketStats`. All fields are integer-valued.

    Field names match `tstrans.rtp.SocketStats` 1:1 so cross-transport
    code can read the same dataclass-shape from both. For SRT-specific
    extras (`mbps_estimated_bandwidth`, the symmetric send/recv-side
    byte-loss split), use `SrtStats`.
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
class SrtStats:
    """Frozen mirror of `tst_srt::Stats` — the libsrt-flavored 17-field
    stats struct. Exposes the SRT-rich fields that don't fit the
    abstract `SocketStats` shape:

    - `mbps_estimated_bandwidth` (libsrt's estimate; bps view lives in
      `SocketStats::link_bandwidth_bps`).
    - Symmetric send/recv-side byte-loss split
      (`bytes_lost_send_side` + `bytes_lost_recv_side`).
    - Symmetric send/recv-side packet drop split.

    `rtt_us` is the `Duration` converted to microseconds, saturating at
    `u32::MAX` — matches the `SocketStats::rtt_us` projection so callers
    can pin either accessor and get the same view.
    """

    bytes_sent: int
    bytes_received: int
    bytes_lost_recv_side: int
    bytes_lost_send_side: int
    packets_sent: int
    packets_received: int
    packets_lost_recv_side: int
    packets_lost_send_side: int
    packets_retransmitted: int
    packets_dropped_recv_side: int
    packets_dropped_send_side: int
    rtt_us: int
    send_bandwidth_bps: int
    recv_bandwidth_bps: int
    mbps_estimated_bandwidth: float
    send_buffer_packets: int
    recv_buffer_packets: int

    def __repr__(self) -> str: ...


@final
class CancelHandle:
    """Transport-side cancel handle for `Sender` / `Receiver` /
    `Listener` / `MuxSender` / `DemuxReceiver`. Calling `.cancel()`
    wakes a thread parked in `.send_bytes()` / `.recv_bytes()` /
    `.accept()` / `.__next__()` within ~3-10 ms; that call returns
    `SrtError(kind=BROKEN)` or `SrtError(kind=CLOSED)` depending on
    which libsrt path the cancel races.

    `is_cancelled()` is per-clone: each clone obtained from a fresh
    `cancel_handle()` call tracks its own observation flag, but they
    all forward `cancel()` into the same shared `Arc<dyn>` so calling
    `.cancel()` on any clone wakes the parked socket.
    """

    def cancel(self) -> None: ...
    def is_cancelled(self) -> bool: ...
    def __repr__(self) -> str: ...


@final
class Sender:
    """Python SRT sender wrapping `tst_pipeline::Sender<SrtTransport>`.

    Construct via `Sender.from_url("srt://host:port?...")` — URL must
    use `mode=caller` (default when omitted). Query parameters apply
    through `UrlOverlay::apply_to_socket`: passphrase, latency,
    streamid, mss, payloadsize, etc.
    """

    @staticmethod
    def from_url(url: str) -> Sender: ...
    def send_bytes(self, data: _BytesLike) -> None: ...
    def flush(self) -> None: ...
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def srt_stats(self) -> SrtStats: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
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
    """Python SRT receiver wrapping `tst_pipeline::Receiver<SrtTransport>`.

    Construct via `Receiver.from_url("srt://...?mode=listener")` — URL
    must use `mode=listener`. Binds the socket, listens, blocks on the
    first incoming SRT handshake. The accepted socket becomes the
    receive transport; this is a one-shot accept (subsequent peers
    must use a fresh `from_url` call or the lower-level `Listener`).
    """

    @staticmethod
    def from_url(url: str) -> Receiver: ...
    def recv_bytes(self, max_len: int = ...) -> bytes: ...
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def srt_stats(self) -> SrtStats: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> Receiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T3 — low-level primitives (Builder / Socket / Listener)
# ---------------------------------------------------------------------------


@final
class Builder:
    """Hybrid fluent + kwargs SRT URL constructor.

    Common knobs accepted both as `__init__` kwargs and as chainable
    setters. URL-provided values WIN over kwargs / setters — the
    overlay's unconditional overwrite applies AFTER the kwarg-built
    config, so the URL gets final say on any conflict.

    Mode is tracked locally via `.caller()` / `.listener()` /
    `.rendezvous()` (the rendezvous shape is forward-compat — finalize
    raises `SrtError(CONFIG_INVALID)` because tst-srt doesn't yet
    surface rendezvous mode).

    Finalize with `.connect()` (caller) → `Socket`, or `.listen()`
    (listener) → `Listener`.
    """

    def __new__(cls,
        url: str,
        *,
        latency_ms: Optional[int] = ...,
        passphrase: Optional[str] = ...,
        stream_id: Optional[str] = ...,
        congestion: Optional[str] = ...,
        connect_timeout_ms: Optional[int] = ...,
        recv_timeout_ms: Optional[int] = ...,
        send_timeout_ms: Optional[int] = ...,
    ) -> Builder: ...

    # Mode setters — chainable.
    def caller(self) -> Builder: ...
    def listener(self) -> Builder: ...
    def rendezvous(self) -> Builder: ...

    # Knob setters — chainable.
    def latency_ms(self, ms: int) -> Builder: ...
    def passphrase(self, p: str) -> Builder: ...
    def stream_id(self, s: str) -> Builder: ...
    def congestion(self, name: str) -> Builder: ...
    def connect_timeout_ms(self, ms: int) -> Builder: ...
    def recv_timeout_ms(self, ms: int) -> Builder: ...
    def send_timeout_ms(self, ms: int) -> Builder: ...
    def peer_latency_ms(self, ms: int) -> Builder: ...
    def recv_latency_ms(self, ms: int) -> Builder: ...
    def max_bandwidth_bps(self, bps: int) -> Builder: ...
    def mss(self, value: int) -> Builder: ...
    def payload_size(self, value: int) -> Builder: ...

    # Finalizers.
    def connect(self) -> Socket: ...
    def listen(self) -> Listener: ...
    def __repr__(self) -> str: ...


@final
class Socket:
    """Low-level SRT socket handle. Returned by `Builder.connect()`
    (caller mode) and `Listener.accept()` (listener-side accepted
    connection).

    Held by reference until consumed via `into_sender()` /
    `into_receiver()` / `into_mux_sender()` / `into_demux_receiver()`
    — each consumes the handle; the underlying `tst_srt::Socket` moves
    into the new wrapper.

    `close()` is a manual teardown; the destructor closes too.
    """

    def into_sender(self) -> Sender: ...
    def into_receiver(self) -> Receiver: ...
    def into_mux_sender(
        self,
        program_config: MuxerProgramConfig,
    ) -> MuxSender: ...
    def into_demux_receiver(
        self,
        *,
        demux_config: Optional[DemuxerConfig] = ...,
    ) -> DemuxReceiver: ...
    def local_addr(self) -> Tuple[str, int]: ...
    def peer_addr(self) -> Tuple[str, int]: ...
    def stream_id(self) -> Optional[str]: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> Socket: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class Listener:
    """Bound SRT listener. Returned by `Builder.listen()` (or
    constructed indirectly via `Receiver.from_url`).

    Iterate to consume accepted `Socket` instances, or call
    `accept(timeout_ms=...)` for explicit per-accept control. The
    iterator stops cleanly when `cancel_handle().cancel()` is called
    from another thread — `AcceptError::ListenerClosed` maps to
    `StopIteration` in `__next__`. Other accept errors propagate as
    `SrtError`.
    """

    def accept(self, timeout_ms: Optional[int] = ...) -> Socket: ...
    def cancel_handle(self) -> CancelHandle: ...
    def local_addr(self) -> Tuple[str, int]: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __iter__(self) -> Listener: ...
    def __next__(self) -> Socket: ...
    def __enter__(self) -> Listener: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T5 — MuxSender + DemuxReceiver convenience wrappers
# ---------------------------------------------------------------------------


@final
class MuxSender:
    """Convenience wrapper — `Sender` + `Muxer` constructed together.

    Construct via `MuxSender.from_url(url, program_config)`. URL must
    specify `?mode=caller` (the SrtUrl default). Push methods accept any
    bytes-like input (`bytes`, `bytearray`, `memoryview`, NumPy `uint8`).
    `pts` is keyword-only on every push method. The GIL is released
    around the muxer + transport work via `py.allow_threads()`.
    """

    @staticmethod
    def from_url(
        url: str,
        program_config: MuxerProgramConfig,
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

    # Send surface — multi-stream variants (explicit handle).
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

    # Stream handle accessors (first-of-kind).
    def video_handle(self) -> Optional[VideoStreamHandle]: ...
    def klv_handle(self) -> Optional[KlvStreamHandle]: ...
    def audio_handle(self) -> Optional[AudioStreamHandle]: ...
    def subtitle_handle(self) -> Optional[SubtitleStreamHandle]: ...
    def data_handle(self) -> Optional[DataStreamHandle]: ...

    def stats(self) -> Tuple[SocketStats, MuxerStats]: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
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

    Construct via `DemuxReceiver.from_url(url, *, demux_config=None)`.
    URL must specify `?mode=listener`. Iterating yields
    `tstrans.mpegts.DemuxEvent` subclass instances — no duplicated
    event type hierarchy. `__next__` blocks (releases the GIL) on the
    underlying transport read.

    `stats()` returns a tuple `(SocketStats, MuxerStats)` — same shape
    as `MuxSender.stats()` so cross-direction code can read both
    accessors identically.
    """

    @staticmethod
    def from_url(
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
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def stats(self) -> Tuple[SocketStats, MuxerStats]: ...
    def last_seen_micros(self, pid: int) -> Optional[int]: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> DemuxReceiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T6 — reconnect policy (BackoffStrategy / OverflowPolicy /
# ReconnectPolicy)
# ---------------------------------------------------------------------------


@final
class BackoffStrategy:
    """Backoff strategy for reconnect attempts.

    Construct via classmethods:
    - `BackoffStrategy.constant(ms=...)` — fixed wait between attempts.
    - `BackoffStrategy.exponential(base_ms=..., max_ms=...)` — wait =
      base * 2^(attempt-1), capped at max.

    Property accessors (`kind`, `base_ms`, `max_ms`) work uniformly
    across both variants — for the constant variant,
    `base_ms == max_ms` equals the fixed wait.
    """

    @classmethod
    def constant(cls, ms: int) -> BackoffStrategy: ...
    @classmethod
    def exponential(cls, *, base_ms: int, max_ms: int) -> BackoffStrategy: ...
    @property
    def kind(self) -> str: ...
    @property
    def base_ms(self) -> int: ...
    @property
    def max_ms(self) -> int: ...
    def __repr__(self) -> str: ...


@final
class OverflowPolicy:
    """What `ManagedTransport` does when the gap buffer is full and a
    new message arrives during an outage. IntEnum-shaped PyClass.

    - `DROP_OLDEST` (default): evict the front of the queue to make room.
    - `REJECT`: refuse to enqueue; surface an error to the caller.

    The plan sketched `DROP_NEWEST` + `BLOCK`, but the real Rust enum
    at `tst_pipeline::reconnect::gap_buffer` only has `DropOldest`
    (default) and `Reject`. This mirrors what Rust ships.
    """

    DROP_OLDEST: OverflowPolicy
    REJECT: OverflowPolicy


@final
class ReconnectMode:
    """Where `ManagedTransport` runs its reconnect loop after the inner
    transport breaks. IntEnum-shaped PyClass — compare with `==`, not
    `is`.

    - `BLOCKING` (default): reconnect runs on the caller's thread; the
      call that observed the break blocks until reconnect succeeds or
      `max_attempts` runs out.
    - `BACKGROUND`: reconnect runs on a dedicated per-outage worker
      thread. Send-side only — `ManagedReceiver` / `ManagedDemuxReceiver`
      log a warning and behave as `BLOCKING` if handed `BACKGROUND`.
      Pair with `reconnect_stats()` for drop/reconnect visibility.
    """

    BLOCKING: ReconnectMode
    BACKGROUND: ReconnectMode


@final
class ReconnectPolicy:
    """Tuning for `ManagedSender` / `ManagedReceiver` /
    `ManagedMuxSender` / `ManagedDemuxReceiver` reconnect behavior.

    Defaults mirror `tst_pipeline::ReconnectPolicy::default()`:
    - `max_attempts = 10`
    - `backoff = BackoffStrategy.exponential(base_ms=100, max_ms=10_000)`
    - `gap_buffer_capacity = 256`
    - `overflow_policy = OverflowPolicy.DROP_OLDEST`
    - `mode = ReconnectMode.BLOCKING`

    Raises `ValueError` if `gap_buffer_capacity == 0`.
    """

    def __new__(cls,
        *,
        max_attempts: Optional[int] = ...,
        backoff: Optional[BackoffStrategy] = ...,
        gap_buffer_capacity: int = ...,
        overflow_policy: OverflowPolicy = ...,
        mode: ReconnectMode = ...,
    ) -> ReconnectPolicy: ...
    @property
    def max_attempts(self) -> Optional[int]: ...
    @property
    def backoff(self) -> BackoffStrategy: ...
    @property
    def gap_buffer_capacity(self) -> int: ...
    @property
    def overflow_policy(self) -> OverflowPolicy: ...
    @property
    def mode(self) -> ReconnectMode: ...
    def __repr__(self) -> str: ...


@final
class ManagedTransportStats:
    """Snapshot of `ManagedSender` / `ManagedMuxSender` reconnect/gap
    telemetry. Returned by `reconnect_stats()`. Frozen, `get_all`-shaped
    PyClass — mirror of `tst_pipeline::ManagedTransportStats`.

    `reconnecting` is only ever `True` under `ReconnectMode.BACKGROUND`
    (always `False` in `BLOCKING` mode).
    """

    reconnect_attempts: int
    reconnect_successes: int
    gap_len: int
    gap_messages_dropped: int
    gap_bytes_dropped: int
    reconnecting: bool
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T7 — auto-reconnect basic-bytes wrappers (ManagedSender /
# ManagedReceiver)
# ---------------------------------------------------------------------------


@final
class ManagedSender:
    """Auto-reconnect SRT sender — wraps `tst_pipeline::Sender
    <ManagedTransport<SrtTransport>>`. On any Broken/Closed event from
    the inner socket, runs the captured URL through the reconnect
    factory under the configured policy and resumes sending.

    `srt_stats()` raises `SrtError(IO)` today: `ManagedTransport`
    doesn't expose the SRT-rich 17-field shape (no accessor in
    tst-pipeline). Use `socket_stats()` for the 16-field
    scheme-neutral view. A future tst-pipeline accessor will lift this.
    """

    @staticmethod
    def from_url(
        url: str,
        *,
        policy: Optional[ReconnectPolicy] = ...,
    ) -> ManagedSender: ...
    def send_bytes(self, data: _BytesLike) -> None: ...
    def flush(self) -> None: ...
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def srt_stats(self) -> SrtStats: ...
    def reconnect_stats(self) -> ManagedTransportStats: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> ManagedSender: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class ManagedReceiver:
    """Auto-reconnect SRT receiver — wraps `tst_pipeline::Receiver
    <ManagedRecvTransport<SrtTransport>>`. On any Broken/Closed event
    from the inner socket, re-runs bind + accept under the configured
    policy and resumes delivering bytes from the new connection.

    `reconnect_attempts()` exposes the total successful reconnect count
    (does NOT include the initial bind+accept).

    `srt_stats()` raises `SrtError(IO)` today (same drift as
    `ManagedSender`). Use `socket_stats()` for the 16-field view.

    `policy.mode` is send-side only: `ReconnectMode.BACKGROUND` on a
    policy handed here logs a warning and this receiver reconnects on
    the caller's thread anyway (behaves as `ReconnectMode.BLOCKING`).
    """

    @staticmethod
    def from_url(
        url: str,
        *,
        policy: Optional[ReconnectPolicy] = ...,
    ) -> ManagedReceiver: ...
    def recv_bytes(self, max_len: int = ...) -> bytes: ...
    def reconnect_attempts(self) -> int: ...
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def srt_stats(self) -> SrtStats: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> ManagedReceiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# T8 — auto-reconnect convenience wrappers (ManagedMuxSender /
# ManagedDemuxReceiver)
# ---------------------------------------------------------------------------


@final
class ManagedMuxSender:
    """Auto-reconnect MuxSender — wraps `MuxSender<ManagedTransport
    <SrtTransport>>`. Construct via `ManagedMuxSender.from_url(url,
    program_config, *, policy=ReconnectPolicy(...))`. URL must specify
    `?mode=caller`.

    `reconnect_attempts()` counts factory invocations (a non-zero value
    means the inner transport has been rebuilt — or a rebuild attempt
    failed and was retried — at least once since construction).
    """

    @staticmethod
    def from_url(
        url: str,
        program_config: MuxerProgramConfig,
        *,
        policy: Optional[ReconnectPolicy] = ...,
    ) -> ManagedMuxSender: ...

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
    def reconnect_attempts(self) -> int: ...
    def reconnect_stats(self) -> ManagedTransportStats: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> ManagedMuxSender: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


@final
class ManagedDemuxReceiver:
    """Auto-reconnect DemuxReceiver — wraps `ManagedDemuxReceiver
    <SrtTransport>`. Construct via `ManagedDemuxReceiver.from_url(url,
    *, demux_config=..., policy=...)`. URL may specify `?mode=listener`
    (default) or `?mode=caller`; the wrapper redials / re-binds on each
    reconnect as appropriate.

    On reconnect, emits a `tstrans.mpegts.DemuxEvent.ReconnectDiscontinuity`
    event before any post-reconnect events. Consumers should drop
    per-stream caches on receipt and rebuild from the next `ProgramMap`
    event.

    Drift: `srt_stats()` returns `SocketStats` (NOT `SrtStats`) today —
    `ManagedRecvTransport` doesn't expose a separate SRT stats
    accessor; the SRT-flavored fields are already in the `SocketStats`
    shape. Reserved for future projection if a richer accessor lands.

    `policy.mode` is send-side only: `ReconnectMode.BACKGROUND` on a
    policy handed here logs a warning and this receiver reconnects on
    the caller's thread anyway (behaves as `ReconnectMode.BLOCKING`).
    """

    @staticmethod
    def from_url(
        url: str,
        *,
        demux_config: Optional[DemuxerConfig] = ...,
        policy: Optional[ReconnectPolicy] = ...,
    ) -> ManagedDemuxReceiver: ...
    def __iter__(self) -> Iterator[DemuxEvent]: ...
    def __next__(self) -> DemuxEvent: ...
    def cancel_handle(self) -> CancelHandle: ...
    def socket_stats(self) -> SocketStats: ...
    def srt_stats(self) -> SocketStats: ...
    def reconnect_attempts(self) -> int: ...
    def last_seen_micros(self, pid: int) -> Optional[int]: ...
    def close(self) -> None: ...
    def is_alive(self) -> bool: ...
    def __enter__(self) -> ManagedDemuxReceiver: ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[Any],
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# Exception types — see `tstrans.exceptions`
# ---------------------------------------------------------------------------
#
# `SrtError` + `SrtErrorKind` live in `tstrans.exceptions` and are NOT
# re-exported from this module. Import them directly:
#
#     from tstrans.exceptions import SrtError, SrtErrorKind
#
# Mirrors the `tstrans.rtp` pattern (`RtpError` / `RtpErrorKind` live in
# `tstrans.exceptions` too).
