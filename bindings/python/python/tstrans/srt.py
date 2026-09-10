"""tstrans.srt — SRT transport bindings.

Available when tstrans was built with the `srt` cargo feature
(default-on in published wheels). Source-built without `--features srt`
will fail to import this submodule with a friendly ImportError.

Submodule contents are populated by `tstrans._native.srt`:
- `Sender`, `Receiver`, `SocketStats`, `SrtStats`, `CancelHandle`
- `Socket`, `Listener`, `Builder`
- `MuxSender`, `DemuxReceiver`
- `ReconnectPolicy`, `BackoffStrategy`, `OverflowPolicy`
- `ManagedSender`, `ManagedReceiver`, `ManagedMuxSender`, `ManagedDemuxReceiver`
- `ReconnectMode`, `ManagedTransportStats` (`reconnect_stats()` on
  `ManagedSender`/`ManagedMuxSender`)

`RecvEndReason` is defined here in pure Python (see its docstring).
"""

from __future__ import annotations

import enum

try:
    from . import _native
    _srt = _native.srt
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.srt is unavailable. Wheels published to PyPI include "
        "SRT by default; if you built tstrans from source, ensure the "
        "`srt` cargo feature is enabled (it is on by default)."
    ) from exc


class RecvEndReason(enum.IntEnum):
    """Why a managed SRT receive session ended. Mirrors
    `tst_pipeline::RecvEndReason` 1:1, in Rust declaration order.

    Returned by `ManagedDemuxReceiver.end_reason()`; `None` means the
    stream hasn't ended yet (or ended through a path this arc doesn't
    instrument).

    A DEDICATED type, not `tstrans.rtp.StreamEndReason`: the C ABI reuses
    its RTP-shaped `TstStreamEndReason` to avoid minting a second ABI
    type, but the two are genuinely different types in Rust and Python
    has no ABI constraint — source wins.

    Pure Python, for the same reason as `tstrans.rtp.StreamEndReason`:
    the Rust-backed `#[pyclass(eq, eq_int)]` enums in this module
    (`BackoffStrategy`, `OverflowPolicy`, `ReconnectMode`) cross the
    Python→Rust boundary as constructor arguments, whereas this one only
    ever flows Rust→Python as a return value.

    Values start at 1, not 0: `end_reason()` uses `None` for "hasn't
    ended", so a falsy member would make `if rx.end_reason():`
    ambiguous. (The JVM twin uses ordinals 0/1/2 — Java enums aren't
    integers, so there is no cross-binding numeric contract here, unlike
    `StreamEndReason` which is pinned to the C enum.)

    Only two variants are reachable on the managed-SRT path today:
    `RECONNECT_EXHAUSTED` (the reconnect decorator exhausted its
    `ReconnectPolicy` budget — this is also what a plain peer close
    reports under a zero-retry policy, because a peer FIN reaches the
    decorator as a retryable break, not a clean end) and `CANCELLED`
    (the caller fired `cancel_handle()` or `close()`). `END_OF_STREAM`
    is reserved for a future transport able to signal a clean
    end-of-stream distinct from reconnect-budget exhaustion — see the
    Rust rustdoc on `RecvEndReason::EndOfStream`.
    """

    END_OF_STREAM = 1
    RECONNECT_EXHAUSTED = 2
    CANCELLED = 3


# Wave A T2 — transport-layer types.
Sender = _srt.Sender
Receiver = _srt.Receiver
SocketStats = _srt.SocketStats
SrtStats = _srt.SrtStats
CancelHandle = _srt.CancelHandle

# Wave A T3 — low-level primitives.
Builder = _srt.Builder
Socket = _srt.Socket
Listener = _srt.Listener

# Wave B T5 — MuxSender + DemuxReceiver convenience wrappers.
MuxSender = _srt.MuxSender
DemuxReceiver = _srt.DemuxReceiver

# Wave B T6 — reconnect policy ergonomics.
BackoffStrategy = _srt.BackoffStrategy
OverflowPolicy = _srt.OverflowPolicy
ReconnectPolicy = _srt.ReconnectPolicy

# Background-reconnect parity — ReconnectMode + reconnect_stats().
ReconnectMode = _srt.ReconnectMode
ManagedTransportStats = _srt.ManagedTransportStats

# Wave C T7 — auto-reconnect basic-bytes wrappers.
ManagedSender = _srt.ManagedSender
ManagedReceiver = _srt.ManagedReceiver

# Wave C T8 — auto-reconnect MuxSender + DemuxReceiver convenience wrappers.
ManagedMuxSender = _srt.ManagedMuxSender
ManagedDemuxReceiver = _srt.ManagedDemuxReceiver


__all__: list[str] = [
    # Managed receive-session end reason
    "RecvEndReason",
    # T2 transport
    "Sender",
    "Receiver",
    "SocketStats",
    "SrtStats",
    "CancelHandle",
    # T3 low-level
    "Builder",
    "Socket",
    "Listener",
    # T5 mux/demux convenience wrappers
    "MuxSender",
    "DemuxReceiver",
    # T6 policy
    "BackoffStrategy",
    "OverflowPolicy",
    "ReconnectPolicy",
    "ReconnectMode",
    "ManagedTransportStats",
    # T7 managed basic
    "ManagedSender",
    "ManagedReceiver",
    # T8 managed convenience wrappers
    "ManagedMuxSender",
    "ManagedDemuxReceiver",
]
