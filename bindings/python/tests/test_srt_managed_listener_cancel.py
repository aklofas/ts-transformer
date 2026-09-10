"""cancel() must wake a listener-mode ManagedDemuxReceiver whose reconnect is
parked in re-accept after its peer disconnected.

Python mirror of tst-c's `loopback_cancel_wakes_managed_listener_parked_in_reaccept`
(ROADMAP "cancellable managed-listener re-accept"). Before the fix the
listener factory sat in `Listener::accept()` with nothing able to reach that
listener, and the backoff between attempts was an uninterruptible sleep, so
`cancel()` did nothing until the next peer happened to connect.

Choreography:
  1. Accept thread: `ManagedDemuxReceiver.from_url("srt://:P?mode=listener")`
     (blocks until a peer connects).
  2. Main: a `ManagedMuxSender` caller connects, pushes a few frames, then
     closes — the managed receiver sees the break and re-enters its factory
     (bind + accept) after the first backoff.
  3. Main: `cancel_handle().cancel()`; the iterator thread must end within a
     couple of seconds with `SrtError(CLOSED)` (the Python surface maps a
     caller-initiated close to CLOSED; see `errors.rs`).

If the cancel does NOT wake the accept, a rescue peer is connected so the
daemon thread can be joined and the test fails with a clear message rather
than leaving a thread parked in native accept at interpreter exit.

The second test runs the same choreography against the basic
`srt.ManagedReceiver` (raw TS bytes, `recv_bytes` instead of iteration),
which PR #188 missed.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from typing import TypeVar

import pytest

import tstrans.srt as srt
from tstrans.exceptions import SrtError, SrtErrorKind
from tstrans.mpegts import Pts90khz

from _builders.mux_programs import video_only_program as _video_only_program
from _builders.ports import free_tcp_port as _free_tcp_port

NAL_IDR = b"\x00\x00\x00\x01\x65\xBB"
# One 188-byte TS packet for the raw-bytes `ManagedReceiver` peer below.
TS_PACKET = b"\x47" + b"\x00" * 187

_Sender = TypeVar("_Sender")


def _connect_sender(open_sender: Callable[[], _Sender], budget_s: float) -> _Sender:
    """Connect a caller, retrying while the listener is between binds.

    `open_sender` is the zero-argument constructor for whichever sender the
    test needs — `ManagedMuxSender` (pushes frames) or `ManagedSender`
    (pushes raw TS bytes). Both raise `SrtError` while the listener is
    unbound, which is exactly what this loop rides out.
    """
    deadline = time.monotonic() + budget_s
    last: BaseException | None = None
    while time.monotonic() < deadline:
        try:
            return open_sender()
        except SrtError as exc:  # listener not (re)bound yet
            last = exc
            time.sleep(0.05)
    pytest.fail(f"caller could not connect within {budget_s}s: {last}")


def test_cancel_wakes_managed_listener_parked_in_reaccept() -> None:
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[srt.ManagedDemuxReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(srt.ManagedDemuxReceiver.from_url(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    accept_t = threading.Thread(target=accept_worker, daemon=True)
    accept_t.start()
    time.sleep(0.1)

    sender = _connect_sender(
        lambda: srt.ManagedMuxSender.from_url(caller_url, _video_only_program()), 5.0
    )
    accept_t.join(timeout=5.0)
    if rx_err:
        sender.close()
        pytest.fail(f"ManagedDemuxReceiver accept failed: {rx_err[0]}")
    if not rx_box:
        sender.close()
        pytest.fail("ManagedDemuxReceiver listener thread did not accept within 5 s")
    rx = rx_box[0]

    # A few frames so the link is genuinely up before the peer drops.
    for i in range(5):
        sender.send_video(NAL_IDR, pts=Pts90khz.from_raw(i * 3000), key_frame=(i == 0))
        time.sleep(0.01)

    outcome: dict[str, object] = {}

    def iterator() -> None:
        try:
            for _ev in rx:
                pass
            outcome["end"] = "StopIteration"
        except SrtError as exc:
            outcome["end"] = "SrtError"
            outcome["kind"] = exc.kind
        except Exception as exc:  # noqa: BLE001
            outcome["end"] = type(exc).__name__

    iter_t = threading.Thread(target=iterator, daemon=True)
    iter_t.start()
    time.sleep(0.3)

    # Peer drop: the managed receiver re-enters its factory (bind + accept)
    # after the default 100 ms backoff and parks there with no peer in sight.
    sender.close()
    time.sleep(1.0)

    t0 = time.monotonic()
    rx.cancel_handle().cancel()
    iter_t.join(timeout=3.0)
    woke_after = time.monotonic() - t0

    if iter_t.is_alive():
        # Rescue so the daemon thread is not left parked in native accept.
        rescue = _connect_sender(
            lambda: srt.ManagedMuxSender.from_url(caller_url, _video_only_program()), 5.0
        )
        iter_t.join(timeout=5.0)
        rescue.close()
        pytest.fail("cancel() did not wake the managed listener parked in re-accept within 3 s")

    assert woke_after < 2.0, f"cancel took {woke_after:.2f}s to wake the parked re-accept"
    assert outcome.get("end") == "SrtError", f"iteration ended via {outcome}"
    assert outcome.get("kind") == SrtErrorKind.CLOSED, f"unexpected SrtError kind: {outcome}"
    rx.close()


def test_managed_receiver_cancel_wakes_reaccept() -> None:
    """The basic `srt.ManagedReceiver` sibling of the test above.

    PR #188 made the re-accept cancellable for `ManagedDemuxReceiver` only;
    `ManagedReceiver` kept the bare `Listener::accept()` factory, so a
    `cancel()` while the factory was parked with no peer in sight did
    nothing until someone happened to connect.
    """
    port = _free_tcp_port()
    listener_url = f"srt://:{port}?mode=listener"
    caller_url = f"srt://127.0.0.1:{port}?mode=caller"

    rx_box: list[srt.ManagedReceiver] = []
    rx_err: list[BaseException] = []

    def accept_worker() -> None:
        try:
            rx_box.append(srt.ManagedReceiver.from_url(listener_url))
        except BaseException as exc:  # noqa: BLE001
            rx_err.append(exc)

    accept_t = threading.Thread(target=accept_worker, daemon=True)
    accept_t.start()
    time.sleep(0.1)

    sender = _connect_sender(lambda: srt.ManagedSender.from_url(caller_url), 5.0)
    accept_t.join(timeout=5.0)
    if rx_err:
        sender.close()
        pytest.fail(f"ManagedReceiver accept failed: {rx_err[0]}")
    if not rx_box:
        sender.close()
        pytest.fail("ManagedReceiver listener thread did not accept within 5 s")
    rx = rx_box[0]

    # Capture the cancel handle BEFORE the first `recv_bytes`: pyo3 holds the
    # `&mut self` borrow for the whole of `recv_bytes` (GIL released, borrow
    # not), so `cancel_handle()` — a `&self` method — would raise "Already
    # borrowed" once the pump thread below is parked inside a recv.
    cancel = rx.cancel_handle()

    # Receive at least once so the link is genuinely up before the peer drops.
    for _ in range(7):
        sender.send_bytes(TS_PACKET)
    first = rx.recv_bytes(max_len=1500)
    assert len(first) >= 188, f"expected a whole TS packet, got {len(first)} bytes"
    assert first[0] == 0x47

    outcome: dict[str, object] = {}

    def pump() -> None:
        try:
            while True:
                rx.recv_bytes(max_len=1500)
        except SrtError as exc:
            outcome["end"] = "SrtError"
            outcome["kind"] = exc.kind
        except BaseException as exc:  # noqa: BLE001
            outcome["end"] = type(exc).__name__

    pump_t = threading.Thread(target=pump, daemon=True)
    pump_t.start()
    time.sleep(0.3)

    # Peer drop: the managed receiver re-enters its factory (bind + accept)
    # after the default backoff and parks there with no peer in sight.
    sender.close()
    time.sleep(1.0)

    t0 = time.monotonic()
    cancel.cancel()
    pump_t.join(timeout=3.0)
    woke_after = time.monotonic() - t0

    if pump_t.is_alive():
        # Rescue so the daemon thread is not left parked in native accept:
        # the connect unparks the accept, and the already-latched cancel then
        # ends the pump on the next loop check.
        rescue = _connect_sender(lambda: srt.ManagedSender.from_url(caller_url), 5.0)
        pump_t.join(timeout=5.0)
        rescue.close()
        pytest.fail("cancel() did not wake the ManagedReceiver parked in re-accept within 3 s")

    assert woke_after < 2.0, f"cancel took {woke_after:.2f}s to wake the parked re-accept"
    assert outcome.get("end") == "SrtError", f"pump ended via {outcome}"
    assert outcome.get("kind") == SrtErrorKind.CLOSED, f"unexpected SrtError kind: {outcome}"
    rx.close()
