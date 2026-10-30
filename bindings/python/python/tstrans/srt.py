"""tstrans.srt — SRT transport bindings.

Available when tstrans was built with the `srt` cargo feature
(default-on in published wheels). Source-built without `--features srt`
will fail to import this submodule with a friendly ImportError.

Submodule contents are populated by `tstrans._native.srt`:
- `SrtStats`
- `Socket`, `Listener`, `Builder`
- `ListenRule`, `ListenRules` (`Listener.rules()`); `RejectReason` and
  `user_defined` are defined here
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

import enum as _enum
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

# Per-stream-ID listen rules.
ListenRule = _srt.ListenRule
ListenRules = _srt.ListenRules
_native_reject_reason_table = _srt._reject_reason_table


class RejectReason(_enum.IntEnum):
    """The predefined SRT reject codes (`SRT_REJX_*`, 1000..=1999) a
    `ListenRule.reject` can carry; the caller's connect error names the
    reason. `ListenRule.reject` also takes any other int >= 1000; use
    `user_defined` for application codes (2000 and up)."""

    FALLBACK = 1000
    KEY_NOT_SUPPORTED = 1001
    FILEPATH = 1002
    HOST_NOT_FOUND = 1003
    BAD_REQUEST = 1400
    UNAUTHORIZED = 1401
    OVERLOAD = 1402
    FORBIDDEN = 1403
    NOT_FOUND = 1404
    BAD_MODE = 1405
    UNACCEPTABLE = 1406
    CONFLICT = 1409
    NOT_SUPPORTED_MEDIA = 1415
    LOCKED = 1423
    FAILED_DEPENDENCY = 1424
    INTERNAL_SERVER_ERROR = 1500
    UNIMPLEMENTED = 1501
    GATEWAY = 1502
    DOWN = 1503
    VERSION_UNSUPPORTED = 1505
    NO_ROOM = 1507


def user_defined(code: int) -> int:
    """An application reject code for `ListenRule.reject`. Raises
    `ValueError` below 2000 (`SRT_REJC_USERDEFINED`; lower codes are
    libsrt's or predefined)."""
    if code < 2000:
        raise ValueError(f"user-defined reject codes start at 2000, got {code}")
    return code

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
    # listen rules
    "ListenRule",
    "ListenRules",
    "RejectReason",
    "user_defined",
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
