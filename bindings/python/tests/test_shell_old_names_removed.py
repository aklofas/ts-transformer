"""The 0.8 spellings of the shell classes are gone: the names `tstrans.srt`
and `tstrans.rtp` carried for them, `Class.from_url(...)` and the
`MuxSender(url, ...)` / `DemuxReceiver(url, ...)` constructors. A shell opens
with `Class.open(url, ...)`."""

from __future__ import annotations

import pytest

import tstrans.rtp as rtp
import tstrans.srt as srt
from tstrans import shells

from _builders.mux_programs import video_only_program
from _builders.ports import free_udp_port

REMOVED_MODULE_NAMES = [
    (srt, "Sender"), (srt, "Receiver"), (srt, "MuxSender"), (srt, "DemuxReceiver"),
    (srt, "CancelHandle"), (srt, "SocketStats"), (srt, "RecvEndReason"),
    (rtp, "MuxSender"), (rtp, "DemuxReceiver"), (rtp, "CancelHandle"),
    (rtp, "SocketStats"), (rtp, "StreamEndReason"), (rtp, "Sender"), (rtp, "Receiver"),
]


@pytest.mark.parametrize(("module", "name"), REMOVED_MODULE_NAMES,
                         ids=[f"{m.__name__}.{n}" for m, n in REMOVED_MODULE_NAMES])
def test_transport_module_no_longer_carries_the_name(module, name) -> None:
    with pytest.raises(AttributeError):
        getattr(module, name)


@pytest.mark.parametrize(
    "cls", [shells.Sender, shells.Receiver, shells.MuxSender, shells.DemuxReceiver],
    ids=lambda c: c.__name__,
)
def test_shell_has_no_from_url(cls) -> None:
    assert not hasattr(cls, "from_url")


def test_mux_sender_is_not_constructible() -> None:
    with pytest.raises(TypeError):
        shells.MuxSender(f"rtp://127.0.0.1:{free_udp_port()}", video_only_program())


def test_demux_receiver_is_not_constructible() -> None:
    with pytest.raises(TypeError):
        shells.DemuxReceiver(f"rtp://127.0.0.1:{free_udp_port()}")
