"""tstrans.srt — SRT transport bindings.

Available when tstrans was built with the `srt` cargo feature
(default-on in published wheels). Source-built without `--features srt`
will fail to import this submodule with a friendly ImportError.

Submodule contents are populated by `tstrans._native.srt`:
- `SrtStats`
- `Socket`, `Listener`, `Builder`
- `ReconnectPolicy`, `BackoffStrategy`, `OverflowPolicy`
- `ManagedSender`, `ManagedReceiver`, `ManagedMuxSender`, `ManagedDemuxReceiver`
- `ReconnectMode`, `ManagedTransportStats` (`reconnect_stats()` on
  `ManagedSender`/`ManagedMuxSender`)

The shell classes (`Sender`, `Receiver`, `MuxSender`, `DemuxReceiver`),
`CancelHandle`, `SocketStats` and the end-reason enum live in
`tstrans.shells`. The old `tstrans.srt` names (`Sender`, `Receiver`,
`MuxSender`, `DemuxReceiver`, `CancelHandle`, `SocketStats`,
`RecvEndReason`) remain as deprecated aliases that emit
`DeprecationWarning` and are removed in 0.10.0.
"""

from __future__ import annotations

import warnings as _warnings

from . import shells as _shells

try:
    from . import _native
    _srt = _native.srt
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.srt is unavailable. Wheels published to PyPI include "
        "SRT by default; if you built tstrans from source, ensure the "
        "`srt` cargo feature is enabled (it is on by default)."
    ) from exc


_DEPRECATED = {
    "Sender": "tstrans.shells.Sender",
    "Receiver": "tstrans.shells.Receiver",
    "MuxSender": "tstrans.shells.MuxSender",
    "DemuxReceiver": "tstrans.shells.DemuxReceiver",
    "CancelHandle": "tstrans.shells.CancelHandle",
    "SocketStats": "tstrans.shells.SocketStats",
    "RecvEndReason": "tstrans.shells.EndReason",
}


def __getattr__(name: str) -> object:
    """Deprecated aliases (removed in 0.10.0): the shell classes moved to
    `tstrans.shells`; `RecvEndReason` is `tstrans.shells.EndReason`."""
    target = _DEPRECATED.get(name)
    if target is None:
        raise AttributeError(f"module 'tstrans.srt' has no attribute {name!r}")
    _warnings.warn(
        f"tstrans.srt.{name} is deprecated; use {target} (removed in 0.10.0)",
        DeprecationWarning,
        stacklevel=2,
    )
    return getattr(_shells, target.rsplit(".", 1)[1])


# SRT-specific statistics.
SrtStats = _srt.SrtStats

# Low-level primitives.
Builder = _srt.Builder
Socket = _srt.Socket
Listener = _srt.Listener

# Reconnect policy ergonomics.
BackoffStrategy = _srt.BackoffStrategy
OverflowPolicy = _srt.OverflowPolicy
ReconnectPolicy = _srt.ReconnectPolicy

# Background-reconnect parity — ReconnectMode + reconnect_stats().
ReconnectMode = _srt.ReconnectMode
ManagedTransportStats = _srt.ManagedTransportStats

# Auto-reconnect basic-bytes wrappers.
ManagedSender = _srt.ManagedSender
ManagedReceiver = _srt.ManagedReceiver

# Auto-reconnect MuxSender + DemuxReceiver convenience wrappers.
ManagedMuxSender = _srt.ManagedMuxSender
ManagedDemuxReceiver = _srt.ManagedDemuxReceiver


__all__: list[str] = [
    # transport
    "SrtStats",
    # low-level
    "Builder",
    "Socket",
    "Listener",
    # policy
    "BackoffStrategy",
    "OverflowPolicy",
    "ReconnectPolicy",
    "ReconnectMode",
    "ManagedTransportStats",
    # managed basic
    "ManagedSender",
    "ManagedReceiver",
    # managed convenience wrappers
    "ManagedMuxSender",
    "ManagedDemuxReceiver",
]
