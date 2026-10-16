"""Every 0.8.0 spelling of a shell class still works for one release and
says so: one DeprecationWarning per use, and the object is the canonical
class. Removed with the aliases in 0.10.0."""

from __future__ import annotations

import threading
import warnings
from typing import Callable

import pytest

import tstrans.rtp as rtp
import tstrans.srt as srt
from tstrans import shells

from _builders.mux_programs import video_only_program
from _builders.ports import free_tcp_port, free_udp_port

MODULE_ALIASES = [
    (srt, "Sender", shells.Sender), (srt, "Receiver", shells.Receiver),
    (srt, "MuxSender", shells.MuxSender), (srt, "DemuxReceiver", shells.DemuxReceiver),
    (srt, "CancelHandle", shells.CancelHandle), (srt, "SocketStats", shells.SocketStats),
    (srt, "RecvEndReason", shells.EndReason),
    (rtp, "MuxSender", shells.MuxSender), (rtp, "DemuxReceiver", shells.DemuxReceiver),
    (rtp, "CancelHandle", shells.CancelHandle), (rtp, "SocketStats", shells.SocketStats),
    (rtp, "StreamEndReason", shells.EndReason),
    (rtp, "Sender", rtp.Transport), (rtp, "Receiver", rtp.RecvTransport),
]


@pytest.mark.parametrize(("module", "name", "target"), MODULE_ALIASES,
                         ids=[f"{m.__name__}.{n}" for m, n, _ in MODULE_ALIASES])
def test_module_alias_is_the_class_and_warns_once(module, name, target) -> None:
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        got = getattr(module, name)
    assert got is target
    assert [x.category for x in w] == [DeprecationWarning]
    assert "removed in 0.10.0" in str(w[0].message)


def _recorded(fn: Callable[[], object]) -> tuple[object, list[str]]:
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        obj = fn()
    assert all(x.category is DeprecationWarning for x in w), [str(x.message) for x in w]
    return obj, [str(x.message) for x in w]


def _one_warning(fn: Callable[[], object], needle: str = "deprecated") -> object:
    obj, messages = _recorded(fn)
    assert len(messages) == 1, messages
    assert needle in messages[0]
    return obj


def _open_srt_pair(
    listener_side: Callable[[str], object], caller_side: Callable[[str], object]
) -> tuple[object, object]:
    """Open an srt listener-side object on a thread and a caller-side object
    on this one. The deprecated spelling under test is whichever side the
    caller passes in; the other side uses the canonical `open`."""
    port = free_tcp_port()
    box: list[object] = []
    errs: list[BaseException] = []

    def accept() -> None:
        try:
            box.append(listener_side(f"srt://:{port}?mode=listener"))
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=accept, daemon=True)
    t.start()
    caller = None
    for _ in range(200):
        try:
            caller = caller_side(f"srt://127.0.0.1:{port}?mode=caller")
            break
        except Exception:  # noqa: BLE001  (listener not accepting yet)
            t.join(0.05)
    t.join(10.0)
    assert caller is not None and box, f"srt pair did not connect: {errs!r}"
    return box[0], caller


# --------------------------------------------------------------------------- #
# srt.X.from_url(...)                                                         #
# --------------------------------------------------------------------------- #


def _recorded_in_thread(fn: Callable[[str], object]) -> tuple[Callable[[str], object], list[list[str]]]:
    seen: list[list[str]] = []

    def run(url: str) -> object:
        obj, messages = _recorded(lambda: fn(url))
        seen.append(messages)
        return obj

    return run, seen


def test_srt_sender_from_url_opens_and_warns_once() -> None:
    peer, tx = _open_srt_pair(
        shells.Receiver.open,
        lambda url: _one_warning(lambda: shells.Sender.from_url(url), "from_url"),
    )
    for o in (tx, peer):
        o.close()


def test_srt_receiver_from_url_opens_and_warns_once() -> None:
    listener, seen = _recorded_in_thread(shells.Receiver.from_url)
    rx, tx = _open_srt_pair(listener, shells.Sender.open)
    assert len(seen) == 1 and len(seen[0]) == 1 and "from_url" in seen[0][0], seen
    for o in (tx, rx):
        o.close()


def test_srt_mux_sender_from_url_opens_and_warns_once() -> None:
    peer, tx = _open_srt_pair(
        shells.DemuxReceiver.open,
        lambda url: _one_warning(
            lambda: shells.MuxSender.from_url(url, video_only_program()), "from_url"
        ),
    )
    for o in (tx, peer):
        o.close()


def test_srt_demux_receiver_from_url_opens_and_warns_once() -> None:
    listener, seen = _recorded_in_thread(shells.DemuxReceiver.from_url)
    rx, tx = _open_srt_pair(
        listener, lambda url: shells.MuxSender.open(url, video_only_program())
    )
    assert len(seen) == 1 and len(seen[0]) == 1 and "from_url" in seen[0][0], seen
    for o in (tx, rx):
        o.close()


# --------------------------------------------------------------------------- #
# rtp.X(...) constructors                                                     #
# --------------------------------------------------------------------------- #


def test_rtp_mux_sender_constructor_opens_and_warns_for_the_alias_and_the_ctor() -> None:
    port = free_udp_port()
    tx, messages = _recorded(
        lambda: rtp.MuxSender(f"rtp://127.0.0.1:{port}", video_only_program(), pkt_size=1316)
    )
    assert isinstance(tx, shells.MuxSender)
    assert len(messages) == 2, messages
    assert "tstrans.rtp.MuxSender" in messages[0] and "constructing" in messages[1]
    tx.close()


def test_rtp_demux_receiver_constructor_opens_and_warns_for_the_alias_and_the_ctor() -> None:
    port = free_udp_port()
    rx, messages = _recorded(lambda: rtp.DemuxReceiver(f"rtp://127.0.0.1:{port}"))
    assert isinstance(rx, shells.DemuxReceiver)
    assert len(messages) == 2, messages
    assert "tstrans.rtp.DemuxReceiver" in messages[0] and "constructing" in messages[1]
    rx.close()


def test_rtp_sender_constructor_is_the_raw_transport_and_warns_once() -> None:
    port = free_udp_port()
    sink = shells.Receiver.open(f"rtp://127.0.0.1:{port}")
    tx = _one_warning(lambda: rtp.Sender(f"rtp://127.0.0.1:{port}", pkt_size=1316, ssrc=7))
    assert isinstance(tx, rtp.Transport)
    tx.close()
    sink.close()


def test_rtp_receiver_constructor_is_the_raw_transport_and_warns_once() -> None:
    rx = _one_warning(lambda: rtp.Receiver(f"rtp://127.0.0.1:{free_udp_port()}"))
    assert isinstance(rx, rtp.RecvTransport)
    rx.close()


# --------------------------------------------------------------------------- #
# The deprecated constructors on the canonical classes                        #
# --------------------------------------------------------------------------- #


def test_deprecated_constructors_open_and_warn() -> None:
    port = free_udp_port()
    rx = _one_warning(lambda: shells.DemuxReceiver(f"rtp://127.0.0.1:{port}"))
    tx = _one_warning(
        lambda: shells.MuxSender(f"rtp://127.0.0.1:{port}", video_only_program(), pkt_size=1316)
    )
    tx2 = _one_warning(
        lambda: shells.MuxSender.from_url(f"udp://127.0.0.1:{port}", video_only_program())
    )
    s = _one_warning(lambda: shells.Sender.from_url(f"udp://127.0.0.1:{port}"))
    for o in (tx, tx2, s, rx):
        o.close()
