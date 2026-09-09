"""Type stubs for `tstrans.hls` — HLS publisher bindings (Plan A5b Wave C).

Mirrors the `Publisher`, `PublisherStats`, `HlsPublisher`,
`HlsPublisherBuilder`, `MuxPublisher`, `MuxPublisherStats`, `HlsMode`, and
`HlsStats` PyClass-backed types exported from `bindings/python/src/hls/`.

``HlsError`` and ``HlsErrorKind`` live in ``tstrans.exceptions`` and are
re-exported here for convenience.

mypy --strict clean.
"""

from __future__ import annotations

import abc

from enum import IntEnum
from typing import Any, Union, final

# A bytes-like input — `bytes`, `bytearray`, `memoryview`, NumPy uint8, or
# any object implementing the buffer protocol. Concrete extraction happens
# in Rust via a two-path fast/fallback pattern.
_BytesLike = Union[bytes, bytearray, memoryview, Any]

__all__: list[str] = [
    "Publisher",
    "PublisherStats",
    "HlsPublisher",
    "HlsPublisherBuilder",
    "HlsServerHandle",
    "MuxPublisher",
    "MuxPublisherStats",
    "HlsMode",
    "HlsStats",
    "HlsError",
    "HlsErrorKind",
]

# ---------------------------------------------------------------------------
# HlsErrorKind / HlsError — re-exported from tstrans.exceptions
# ---------------------------------------------------------------------------


class HlsErrorKind(IntEnum):
    """Discriminator for ``HlsError.kind``. Mirrors ``tst_hls::HlsErrorKind``."""

    URL = 0
    IO = 1
    BIND_FAILED = 2
    INVALID_CONFIG = 3
    UNALIGNED_PUSH_TS = 4
    FINISHED = 5
    TLS_DISABLED = 6
    TLS = 7
    INTERNAL = 8


class HlsError(Exception):
    """Raised by ``tstrans.hls`` operations. Discriminate via ``.kind``."""

    kind: HlsErrorKind
    message: str

    def __init__(self, *, kind: HlsErrorKind, message: str) -> None: ...


# ---------------------------------------------------------------------------
# HlsMode — playlist mode
# ---------------------------------------------------------------------------


@final
class HlsMode:
    """HLS playlist mode. Mirrors ``tst_hls::HlsMode``.

    ``LIVE`` rolling-window, ``EVENT`` monotone-grow, ``VOD`` all-at-once
    on finish. Int-comparable (``==`` / ``hash``).
    """

    LIVE: HlsMode
    EVENT: HlsMode
    VOD: HlsMode

    def __int__(self) -> int: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# PublisherStats — universal cross-publisher stats
# ---------------------------------------------------------------------------


@final
class PublisherStats:
    """Frozen universal stats snapshot. Mirrors
    ``tst_core::publisher::PublisherStats``.

    The duration fields are surfaced as microseconds (divide by 1_000_000
    for seconds); ``None`` when no segment is open / completed yet.
    """

    segments_written: int
    """Total completed segments written."""
    bytes_written: int
    """Total bytes pushed into the sink."""
    current_segment_age_us: int | None
    """Wall-clock age of the open segment, in microseconds (``None`` if none open)."""
    last_segment_duration_us: int | None
    """Duration of the most recent completed segment, in microseconds."""

    def __new__(cls,
        segments_written: int,
        bytes_written: int,
        current_segment_age_us: int | None = ...,
        last_segment_duration_us: int | None = ...,
    ) -> PublisherStats: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# HlsStats — richer HLS-specific stats
# ---------------------------------------------------------------------------


@final
class HlsStats:
    """Frozen HLS-specific stats snapshot. Mirrors ``tst_hls::HlsStats``.

    For cross-publisher metrics use ``HlsPublisher.stats()`` (returns
    ``PublisherStats``) instead.
    """

    segments_written: int
    """Total completed segments (history + current run)."""
    bytes_pushed_total: int
    """Total bytes accepted by ``push_ts`` across all segments."""
    open_segment_bytes: int
    """Bytes in the currently-open segment (0 between cuts)."""
    forced_cuts: int
    """Segments cut by the wall-clock hard-cap fallback because a keyframe was
    overdue (keyframe-driven flow only). A persistently non-zero value means
    the upstream GOP length exceeds the configured cap."""

    def __new__(cls,
        segments_written: int,
        bytes_pushed_total: int,
        open_segment_bytes: int,
        forced_cuts: int,
    ) -> HlsStats: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# Publisher — abstract base class
# ---------------------------------------------------------------------------


class Publisher(abc.ABC):
    """Abstract base class for byte-sink publishers.

    Mirrors the Rust ``tst_core::publisher::Publisher`` trait. Direct
    instantiation raises ``TypeError``; subclasses must implement all four
    methods. ``HlsPublisher`` is a registered virtual subclass.
    """

    @abc.abstractmethod
    def push_ts(self, ts_bytes: _BytesLike) -> None:
        """Push MPEG-TS bytes for the current segment (multiple of 188)."""
        ...

    @abc.abstractmethod
    def cut_segment(self) -> None:
        """Hint that the next ``push_ts`` should start a new segment."""
        ...

    def cut_segment_with_duration(self, media_duration_us: int) -> None: ...

    @abc.abstractmethod
    def finish(self) -> None:
        """Flush, write the terminal playlist, tear down the sink."""
        ...

    @abc.abstractmethod
    def stats(self) -> PublisherStats:
        """Snapshot of publisher health."""
        ...


# ---------------------------------------------------------------------------
# HlsPublisher — concrete HLS sink
# ---------------------------------------------------------------------------


@final
class HlsPublisher:
    """HLS publisher: segments MPEG-TS to disk + serves an HTTP playlist.

    A registered virtual subclass of :class:`Publisher`. Construct via
    ``HlsPublisher.builder()``. ``finish()`` (or handing it to
    ``MuxPublisher.with_config_hls``) consumes the inner publisher;
    subsequent operations raise ``HlsError(kind=FINISHED)``.
    """

    @staticmethod
    def builder() -> HlsPublisherBuilder:
        """Return a fresh builder. Chain setters then call ``.build()``."""
        ...

    def push_ts(self, ts_bytes: _BytesLike) -> None:
        """Push pre-muxed MPEG-TS bytes (multiple of 188).

        Raises ``HlsError(kind=UNALIGNED_PUSH_TS)`` for a non-188-multiple,
        ``HlsError(kind=FINISHED)`` if consumed.
        """
        ...

    def cut_segment(self) -> None:
        """Cut the current segment (IDR boundary)."""
        ...

    def cut_segment_with_duration(self, media_duration_us: int) -> None: ...

    def finish(self) -> None:
        """Finalize: flush, write terminal playlist, stop HTTP server. Consumes."""
        ...

    def stats(self) -> PublisherStats:
        """Universal cross-publisher stats."""
        ...

    def hls_stats(self) -> HlsStats:
        """Richer HLS-specific stats."""
        ...

    def local_addr(self) -> str | None:
        """Bound HTTP server address as ``"ip:port"``, or ``None``."""
        ...

    def local_port(self) -> int:
        """Bound TCP port (0 if no server)."""
        ...

    def render_playlist(self, is_event: bool = ...) -> str:
        """Render current playlist text (terminal form when ``is_event``)."""
        ...

    def close(self) -> None:
        """Idempotent ``finish()`` — never raises if already finished."""
        ...

    def finish_serving(self) -> HlsServerHandle:
        """Finalize but keep the HTTP server serving the terminal playlist.

        Like :meth:`finish`, but the built-in HTTP server stays up (via the
        returned :class:`HlsServerHandle`) so clients can fetch the terminal
        playlist + every segment after the stream ends — the way a VOD /
        EVENT stream becomes observable. **Consumes** the publisher;
        subsequent operations raise ``HlsError(kind=FINISHED)``.
        """
        ...

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# HlsServerHandle — keeps a finished playlist served
# ---------------------------------------------------------------------------


@final
class HlsServerHandle:
    """Live HTTP server serving a finished HLS playlist + its segments.

    Returned by :meth:`HlsPublisher.finish_serving`. Keeps the built-in
    server up so clients can fetch the terminal playlist and every segment
    file after the stream has ended. Use as a context manager (``__exit__``
    calls :meth:`shutdown`) or call :meth:`shutdown` / :meth:`close`
    explicitly; the server also stops when the handle is dropped.
    """

    def local_addr(self) -> str:
        """Bound HTTP server address as ``"ip:port"``.

        Raises ``HlsError(kind=FINISHED)`` after shutdown.
        """
        ...

    def local_port(self) -> int:
        """Bound TCP port.

        Raises ``HlsError(kind=FINISHED)`` after shutdown.
        """
        ...

    def shutdown(self) -> None:
        """Stop serving and drain the runtime. Idempotent."""
        ...

    def close(self) -> None:
        """Alias for :meth:`shutdown` (idempotent)."""
        ...

    def __enter__(self) -> HlsServerHandle: ...
    def __exit__(
        self,
        exc_type: object | None = ...,
        exc_value: object | None = ...,
        traceback: object | None = ...,
    ) -> bool: ...
    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# HlsPublisherBuilder — builder for HlsPublisher
# ---------------------------------------------------------------------------


@final
class HlsPublisherBuilder:
    """Builder for ``HlsPublisher``. All setters return ``self`` for chaining."""

    def __init__(self) -> None: ...
    def bind(self, addr: str) -> HlsPublisherBuilder:
        """HTTP server bind address (e.g. ``"127.0.0.1:0"``)."""
        ...

    def output_dir(self, path: str) -> HlsPublisherBuilder:
        """Filesystem directory for ``.ts`` segments + ``playlist.m3u8``."""
        ...

    def segment_duration_ms(self, ms: int) -> HlsPublisherBuilder:
        """Target segment duration in milliseconds."""
        ...

    def max_segment_duration_ms(self, ms: int) -> HlsPublisherBuilder:
        """Hard cap (ms) on an open segment's wall-clock age in the
        keyframe-driven flow (force-cuts when a keyframe is overdue).

        Defaults to ``2 × segment_duration``; must be ``>= segment_duration``
        at ``build()``. ``ms == 0`` leaves the library default in place (no
        reset-to-default); callers wanting the default never call this.
        """
        ...

    def playlist_window(self, n: int) -> HlsPublisherBuilder:
        """Rolling-window size (segments visible in a LIVE playlist)."""
        ...

    def mode(self, mode: HlsMode) -> HlsPublisherBuilder:
        """Playlist mode (LIVE / EVENT / VOD)."""
        ...

    def basic_auth(self, user: str, password: str) -> HlsPublisherBuilder:
        """Enable HTTP Basic auth."""
        ...

    def enable_tls(self, cert: str, key: str) -> HlsPublisherBuilder:
        """Enable HTTPS via PEM cert + key file paths.

        Compiled in by default (tst-py ``tls`` feature; wheels ship it).
        A source build without ``tls`` raises
        ``HlsError(kind=TLS_DISABLED)`` at ``build()``.
        """
        ...

    def from_url(self, url: str) -> HlsPublisherBuilder:
        """Seed config from an ``hls://`` / ``hlss://`` URL.

        Raises ``HlsError(kind=URL)`` on a bad URL.
        """
        ...

    def build(self) -> HlsPublisher:
        """Build the publisher (binds the HTTP server immediately).

        Raises ``HlsError`` (``BIND_FAILED`` / ``INVALID_CONFIG`` /
        ``TLS_DISABLED``) per the failure.
        """
        ...

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# MuxPublisherStats — shell stats
# ---------------------------------------------------------------------------


@final
class MuxPublisherStats:
    """Frozen ``MuxPublisher`` shell stats. Mirrors
    ``tst_pipeline::MuxPublisherStats``."""

    bytes_pushed: int
    """Total TS bytes drained from the muxer and handed to the publisher."""
    drain_calls: int
    """Total muxer drain calls that produced at least one chunk."""
    cut_calls: int
    """Total explicit + auto (keyframe) cut_segment calls."""

    def __repr__(self) -> str: ...


# ---------------------------------------------------------------------------
# MuxPublisher — muxer + HlsPublisher pipeline shell
# ---------------------------------------------------------------------------


@final
class MuxPublisher:
    """Owns a muxer + an ``HlsPublisher``; push elementary streams.

    Construct via ``MuxPublisher.with_config_hls(publisher, program_config)``
    — which *consumes* the ``HlsPublisher``. Recover the publisher (e.g. to
    ``finish()`` it cleanly) via ``finish_into_publisher()``.
    """

    @staticmethod
    def with_config_hls(
        publisher: HlsPublisher,
        program_config: Any,
    ) -> MuxPublisher:
        """Build from a single-program config + an ``HlsPublisher`` (consumed).

        ``program_config`` is a ``tstrans.mpegts.MuxerProgramConfig``.
        Raises ``HlsError(kind=INVALID_CONFIG)`` if the muxer rejects it.
        """
        ...

    def send_video(
        self, nal: _BytesLike, *, pts: Any, key_frame: bool = ...
    ) -> None:
        """Push one video access unit (Annex-B). Auto-cuts on ``key_frame``."""
        ...

    def send_klv(self, klv: _BytesLike, *, pts: Any, stream_index: int = ...) -> None:
        """Push one KLV blob. ``stream_index`` selects the KLV stream."""
        ...

    def send_audio(self, frames: _BytesLike, *, pts: Any) -> None:
        """Push one or more pre-framed audio frames."""
        ...

    def send_subtitle(self, payload: _BytesLike, *, pts: Any) -> None:
        """Push one subtitle payload."""
        ...

    def cut_segment(self) -> None:
        """Explicit segment-cut hint (IDR boundary)."""
        ...

    def finish_into_publisher(self) -> HlsPublisher:
        """Consume the shell and return the owned ``HlsPublisher``.

        Caller should then ``finish()`` it. Raises
        ``HlsError(kind=FINISHED)`` if already consumed.
        """
        ...

    def stats(self) -> MuxPublisherStats:
        """Shell-level stats."""
        ...

    def publisher_stats(self) -> PublisherStats:
        """Publisher-side universal stats."""
        ...

    def __repr__(self) -> str: ...
