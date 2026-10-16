"""tstrans.shells — one class per pipeline shell, over every transport.

`Sender`, `Receiver`, `MuxSender` and `DemuxReceiver` open from a URL with
`Class.open(url, ...)`; the scheme selects the transport (`srt://`,
`rtp://`, `udp://`, `tcp://`, `tcps://`, `rist://`) and per-transport
options are URL query keys (`?pkt_size=`, `?latency=`, ...). Failures raise
the transport's exception class (`SrtError`, `RtpError`, `UdpError`,
`TcpError`, `RistError`); muxer failures raise `MuxError`.

`EndReason` is why a receive session ended. Values 1-6 match the C
`TstStreamEndReason` enum; `END_OF_STREAM` and `RECONNECT_EXHAUSTED` are
the receive-side ends a C receiver reports as `CLEAN_TEARDOWN` and
`TRANSPORT_FAILED`.
"""

from __future__ import annotations

import enum

try:
    from . import _native
    _shells = _native.shells
except (ImportError, AttributeError) as exc:  # pragma: no cover
    raise ImportError(
        "tstrans.shells is unavailable: this tstrans was built without any "
        "transport feature (srt, rtp, udp, tcp or rist)."
    ) from exc


class EndReason(enum.IntEnum):
    """Why a receive session ended (`Receiver.end_reason()`,
    `DemuxReceiver.end_reason()`, the managed SRT receivers); `None` while
    it has not ended. Values start at 1 so `if rx.end_reason():` is
    unambiguous."""

    CLEAN_TEARDOWN = 1
    SESSION_EXPIRED = 2
    KEEPALIVE_FAILED = 3
    TRANSPORT_FAILED = 4
    PROTOCOL_ERROR = 5
    CANCELLED = 6
    END_OF_STREAM = 7
    RECONNECT_EXHAUSTED = 8


Sender = _shells.Sender
Receiver = _shells.Receiver
MuxSender = _shells.MuxSender
DemuxReceiver = _shells.DemuxReceiver
CancelHandle = _shells.CancelHandle
SocketStats = _shells.SocketStats

__all__ = ["CancelHandle", "DemuxReceiver", "EndReason", "MuxSender", "Receiver", "Sender", "SocketStats"]
