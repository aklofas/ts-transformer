"""tstrans.hls — tst-hls (HLS publisher) bindings.

The `hls` cargo feature is default-on and ships in the published wheels, so
this submodule imports out of the box. It is unavailable only in a
`--no-default-features` source build (or one that selects features without
`hls`); such a build will fail to import this submodule.

Surface:

- ``Publisher`` — abstract base class for byte-sink publishers. Mirrors
  the Rust ``tst_core::publisher::Publisher`` trait (``push_ts`` /
  ``cut_segment`` / ``finish`` / ``stats``). Direct instantiation raises
  ``TypeError``; a subclass must implement all four methods.
  ``HlsPublisher`` is registered as a *virtual* subclass, so
  ``isinstance(an_hls_publisher, Publisher)`` is True even though
  ``HlsPublisher`` does not inherit from ``Publisher``.
- ``PublisherStats`` — universal cross-publisher stats snapshot.
- ``HlsPublisher`` / ``HlsPublisherBuilder`` — the concrete HLS sink:
  segments MPEG-TS to disk + serves a built-in HTTP playlist.
- ``HlsServerHandle`` — returned by ``HlsPublisher.finish_serving()``; keeps
  the built-in HTTP server up so the terminal playlist + segments stay
  fetchable after the stream ends.
- ``MuxPublisher`` / ``MuxPublisherStats`` — pipeline shell that owns a
  muxer + an ``HlsPublisher`` and accepts elementary streams.
- ``HlsMode`` (LIVE / EVENT / VOD) + ``HlsStats``.
- ``HlsError`` / ``HlsErrorKind`` — re-exported from ``tstrans.exceptions``.

``Publisher`` is a pure-Python ``abc.ABC`` (NOT the native PyClass) so it
mixes cleanly with ``abc.ABCMeta`` and exposes ``Publisher.register(...)``.
The concrete ``HlsPublisher`` is a native PyClass with the four required
methods; it is registered here as a virtual subclass. The ABC's abstract
method list is verified against the Rust ``Publisher`` trait by
``scripts/check/python/publisher-class-mirror.sh`` (which reads ``hls.pyi``).
"""

from __future__ import annotations

import abc

from . import _native
from .exceptions import HlsError, HlsErrorKind

try:
    _hls = _native.hls
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.hls is unavailable. The `hls` cargo feature is default-on "
        "and shipped in the published wheels, so this usually means a "
        "`--no-default-features` source build (or one that omitted `hls`); "
        "rebuild with the `hls` feature enabled."
    ) from exc

# Native PyClasses populated by `bindings/python/src/hls/`.
PublisherStats = _hls.PublisherStats
HlsPublisher = _hls.HlsPublisher
HlsPublisherBuilder = _hls.HlsPublisherBuilder
HlsServerHandle = _hls.HlsServerHandle
MuxPublisher = _hls.MuxPublisher
MuxPublisherStats = _hls.MuxPublisherStats
HlsMode = _hls.HlsMode
HlsStats = _hls.HlsStats


class Publisher(abc.ABC):
    """Abstract base class for byte-sink publishers.

    A ``Publisher`` is an outbound-only, segment-aware sink for MPEG-TS
    bytes (HLS today; MPEG-DASH or other segmented outputs in future). It
    mirrors the Rust ``tst_core::publisher::Publisher`` trait.

    Subclass it and implement all four abstract methods to document a
    custom byte sink. ``HlsPublisher`` is registered as a virtual subclass
    (see ``Publisher.register(HlsPublisher)`` below), so
    ``isinstance(an_hls_publisher, Publisher)`` is True even though
    ``HlsPublisher`` does not inherit from this class.

    Direct instantiation, or instantiation of a subclass that does not
    implement every abstract method, raises ``TypeError``.
    """

    @abc.abstractmethod
    def push_ts(self, ts_bytes: bytes | bytearray | memoryview) -> None:
        """Push MPEG-TS bytes for the current segment (multiple of 188)."""
        raise NotImplementedError

    @abc.abstractmethod
    def cut_segment(self) -> None:
        """Hint that the next ``push_ts`` should start a new segment."""
        raise NotImplementedError

    @abc.abstractmethod
    def finish(self) -> None:
        """Cleanly finalize: flush, write the terminal playlist, tear down."""
        raise NotImplementedError

    @abc.abstractmethod
    def stats(self) -> PublisherStats:
        """Snapshot of publisher health (segments / bytes / segment ages)."""
        raise NotImplementedError

    def cut_segment_with_duration(self, media_duration_us: int) -> None:
        """Hint a new segment, supplying its media-presentation duration (µs).

        Mirrors the Rust ``Publisher::cut_segment_with_duration``. The default
        delegates to :meth:`cut_segment` — a custom publisher only needs to
        override it to record media-derived ``#EXTINF`` durations instead of
        wall-clock time. The ``MuxPublisher`` pipeline shell derives this
        duration from PTS and calls it automatically.
        """
        self.cut_segment()


# Register the concrete native HlsPublisher as a virtual subclass so
# `isinstance(pub, Publisher)` holds without native inheritance (the
# native HlsPublisher PyClass already provides all four methods).
Publisher.register(HlsPublisher)


__all__ = [
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
