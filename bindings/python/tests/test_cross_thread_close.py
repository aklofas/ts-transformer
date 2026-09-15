"""`close()` from another thread while a call is parked — sender-side
parity for the receiver-side contract PRs #205/#209 established.

Every class below used to close through `&mut self`: a cross-thread
`close()` during a parked (or in-flight, GIL-released) call tripped
PyO3's borrow check and raised `RuntimeError: Already borrowed` instead
of ending the parked call. Each test parks (or keeps in flight) one call
on a worker thread, runs `close()` on a second thread with a bounded
join, and asserts (a) `close()` raised nothing and returned within 2 s,
(b) the parked call ended with the documented kind.

The tests keep a rescue path so a daemon thread parked in native I/O is
unparked before the test fails rather than at interpreter exit.
"""

from __future__ import annotations

import socket
import threading
import time
from typing import Callable

import pytest

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port
from _builders.ports import free_udp_port as _free_udp_port

TS_PACKET = b"\x47" + b"\x00" * 187
TS_BUNDLE = TS_PACKET * 7          # one full 1316-byte framing bundle
NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"

_CLOSE_BUDGET_S = 2.0


def _close_on_thread(obj: object) -> tuple[threading.Thread, list[BaseException]]:
    """Run `obj.close()` on its own daemon thread; join with the WP budget."""
    errs: list[BaseException] = []

    def closer() -> None:
        try:
            obj.close()
        except BaseException as exc:  # noqa: BLE001
            errs.append(exc)

    t = threading.Thread(target=closer, daemon=True)
    t.start()
    t.join(_CLOSE_BUDGET_S)
    return t, errs


def _assert_close_ok(t: threading.Thread, errs: list[BaseException], what: str) -> None:
    assert not errs, f"{what}.close() raised {errs!r}"
    assert not t.is_alive(), f"{what}.close() did not return within {_CLOSE_BUDGET_S} s"


def _spin_sender(send_one: Callable[[], None], stop: threading.Event) -> dict[str, object]:
    """Worker body for the non-parking senders: call `send_one` until it
    raises or `stop` is set; record how it ended."""
    outcome: dict[str, object] = {}
    try:
        while not stop.is_set():
            send_one()
    except BaseException as exc:  # noqa: BLE001
        outcome["exc"] = exc
    return outcome
