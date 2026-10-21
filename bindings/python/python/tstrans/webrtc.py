"""tstrans.webrtc — WebRTC (WHEP) browser playback.

Available when tstrans was built with the `webrtc` cargo feature
(default-on in published wheels). A source build without it fails to
import this submodule with a friendly ImportError.

A `WhepServer` serves mounts to browsers over WHEP (RFC 9725): a viewer
POSTs an SDP offer to ``http://<host>:<port><mount>``, the server answers,
and video (H.264 or H.265) plays with the KLV records carried in an SEI
message of each frame and on a data channel named ``klv``.

Surface:

- ``WhepServer`` — ``bind(url)``, ``add_mount`` / ``remove_mount``,
  ``stats``, ``local_addr`` / ``media_addr``, ``stop`` / ``close``.
- ``MountHandle`` — the push side of one mount: ``push_video``,
  ``push_klv``, ``push_audio`` (elementary mounts), ``push_ts`` (MP2T
  mounts), ``stats``, ``sessions``.
- ``MountConfig`` — a frozen dataclass; ``MountKind``, ``AudioPolicy`` and
  ``KlvCarriage`` are its enums.
- ``ServerStats``, ``MountStats``, ``SessionStats`` — counter snapshots.
- ``WebrtcError`` / ``WebrtcErrorKind`` — re-exported from
  ``tstrans.exceptions``.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass

from .exceptions import WebrtcError, WebrtcErrorKind
from .mpegts import VideoCodec

try:
    from . import _native

    _webrtc = _native.webrtc
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.webrtc is unavailable. Wheels published to PyPI include "
        "WebRTC by default; if you built tstrans from source, ensure the "
        "`webrtc` cargo feature is enabled (it is on by default)."
    ) from exc

WhepServer = _webrtc.WhepServer
MountHandle = _webrtc.MountHandle
ServerStats = _webrtc.ServerStats
MountStats = _webrtc.MountStats
SessionStats = _webrtc.SessionStats


class MountKind(enum.IntEnum):
    """What a mount is fed with."""

    ELEMENTARY = 0
    """Elementary pushes: ``push_video``, ``push_klv``, ``push_audio``."""
    MP2T = 1
    """An MPEG-TS byte stream: ``push_ts``. The mount demuxes it and plays
    its first video stream and the KLV records bound to it."""


class AudioPolicy(enum.IntEnum):
    """What a mount does with audio."""

    DROP = 0
    """Audio is counted and dropped; viewers get video and KLV only."""
    PASSTHROUGH_OPUS = 1
    """``push_audio`` takes Opus packets and sends them to viewers that
    negotiated Opus. Elementary mounts only."""


class KlvCarriage(enum.IntEnum):
    """Where a mount carries its KLV records."""

    SEI_AND_DATA_CHANNEL = 0
    """In an SEI message of each frame and on the ``klv`` data channel."""
    SEI_ONLY = 1
    """In an SEI message of each frame only."""
    DATA_CHANNEL_ONLY = 2
    """On the ``klv`` data channel only."""


@dataclass(frozen=True)
class MountConfig:
    """How one mount is fed and what it sends; the defaults are the Rust
    ``MountConfig::default()``. ``WhepServer.add_mount`` validates it and
    raises ``WebrtcError(MOUNT)`` for a combination it refuses."""

    kind: MountKind = MountKind.ELEMENTARY
    """Elementary pushes or an MPEG-TS byte stream."""

    video: VideoCodec = VideoCodec.H264
    """The video codec offered to viewers: H.264 or H.265."""

    audio: AudioPolicy = AudioPolicy.DROP
    """What happens to audio."""

    klv: KlvCarriage = KlvCarriage.SEI_AND_DATA_CHANNEL
    """Where KLV records travel."""

    sei_budget_bytes: int = 64 * 1024
    """The most KLV bytes one frame's SEI carries, 1 to 1 MiB; a record over
    the budget is left out of the SEI and counted."""

    viewer_queue_packets: int = 2048
    """Each viewer's queue, in RTP packets; at least 16."""


__all__ = [
    "WhepServer",
    "MountHandle",
    "MountConfig",
    "MountKind",
    "AudioPolicy",
    "KlvCarriage",
    "ServerStats",
    "MountStats",
    "SessionStats",
    "WebrtcError",
    "WebrtcErrorKind",
]
